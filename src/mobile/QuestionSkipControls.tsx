/**
 * 单题跳过控件（受控原型）：**已承诺操作控制器（questionSkipOperations）的纯订阅面**。
 *
 * 本组件不拥有任何请求寿命：phase/packet/accepted 全部来自 registry slot
 * （(sessionId, full group4) 唯一），跨 disabled/unmount/A-B-A 保持。组件只有两个
 * 职责：
 * 1. **CommittedGate 发布**（稳定 host layout effect，render 不写）：每次 commit 把
 *    {scopeKey, disabled, fact} 交给控制器；unmount/key 切换的 cleanup 发布
 *    withdrawal（撤 prewrite；issued 与 Done{accepted} 都不动，Done 跨挂载保留）。
 * 2. 渲染 slot 快照 + 把用户意图转成控制器调用。所有准入（gate/state/资格/
 *    distinguishability/epoch 二次比对）都在控制器的同步守卫里，按钮 disabled 只是
 *    供能面——handler 侧守卫是权威。
 *
 * 文案诚实：只有 closed_verified 可说「已关闭原生卡」；unknown 仅原包原 ID 重试。
 */
import { useCallback, useLayoutEffect, useRef, useSyncExternalStore } from "react";
import i18n from "@/i18n";
import type { GroupDisplayView, QuestionDisplayDecision } from "./codexQuestionSkipApi";
import {
  beginQuestionSkipRestore,
  confirmQuestionSkipPairing,
  getQuestionSkipSnapshot,
  issueQuestionSkipDisplayOnly,
  openQuestionSkipPairing,
  publishQuestionSkipGate,
  questionSkipCandidatesDistinguishable,
  questionSkipScopeKey,
  resolveEffectiveQuestionSkipDecision,
  retryQuestionSkipIssued,
  selectQuestionSkipCandidate,
  setQuestionSkipAck,
  subscribeQuestionSkip,
  type QuestionSkipSnapshot,
} from "./questionSkipOperations";

export interface QuestionSkipControlsProps {
  sessionId: string;
  view: GroupDisplayView;
  /** 写锁（投影 stale 等）：撤 prewrite 准入；issued 不受影响。 */
  disabled?: boolean;
  /** 跳过/恢复成功后的父层刷新信号（控制器在 gate 准入下触发，stale scope 静默）。 */
  onChanged?: () => void;
  /** 历史只读条目：不提供 start/restore（无新 skip、无伪可答）。 */
  readOnly?: boolean;
  /** 题干展示（历史 question 或已证唯一 current fallback）；null → source 标识。 */
  questionTitle?: string | null;
  /** 多个恢复条目的无障碍区分标签（诚实 source fallback，不借新 gen 题文）。 */
  ariaLabel?: string | null;
  /** Done 释放/恢复 expectedRevision 用的准入事实来源。缺省（undefined）= 旧独立
   *  挂载，view 即 canonical 模拟；SessionDetail 的 dock 传 **帧事实**（服务端
   *  display 账原值，不含本端回执回声——回执回声不得解除 Done，恢复反馈要留到
   *  canonical 真正接管）。**显式 null** = 该条目没有可用服务端帧事实（历史条目
   *  缺帧或账 invalid）：发布 null fact，不拿 view/回执冒充 canonical。
   *  readOnly 实例不发布也不撤销 gate。 */
  gateFact?: { decision: QuestionDisplayDecision; revision: number } | null;
}

function errorCopy(error: QuestionSkipSnapshot["error"]): string {
  if (error === null) return "";
  const key =
    error.field === "pairing"
      ? "mobile.codex.skip.pairingFailed"
      : error.field === "skip"
        ? "mobile.codex.skip.skipFailed"
        : "mobile.codex.skip.restoreFailed";
  return i18n.t(key, { message: error.reason });
}

export default function QuestionSkipControls({
  sessionId,
  view,
  disabled = false,
  onChanged,
  readOnly = false,
  questionTitle = null,
  ariaLabel = null,
  gateFact = undefined,
}: QuestionSkipControlsProps) {
  const group = view.group;
  const key = questionSkipScopeKey(sessionId, group);
  const snapshot = useSyncExternalStore(
    useCallback((listener: () => void) => subscribeQuestionSkip(key, listener), [key]),
    useCallback(() => getQuestionSkipSnapshot(key), [key]),
    useCallback(() => getQuestionSkipSnapshot(key), [key])
  );

  // readOnly 重试成功后的 onChanged 防护（本实例不发布 gate，无 gate 准入可依据）：
  // 只有**实例仍挂载于同一 key 且未 disabled** 时才通知父层刷新；卸载/换 key/stale 后
  // 静默沉淀事实（回执账仍落，父层下次自然读取）。键与写锁经 layout effect 同步，
  // 保持 render 不写 refs 的既有约束。
  const hostRef = useRef({ key, disabled, mounted: false });
  useLayoutEffect(() => {
    hostRef.current.disabled = disabled;
  }, [disabled]);
  useLayoutEffect(() => {
    hostRef.current.mounted = true;
    hostRef.current.key = key;
    return () => {
      hostRef.current.mounted = false;
    };
  }, [key]);
  const guardedOnChanged = (scopeKey: string) => {
    if (!hostRef.current.mounted) return;
    if (hostRef.current.key !== scopeKey) return; // 已换 scope：旧回执静默沉淀
    if (hostRef.current.disabled) return; // 写锁/stale：不打扰父层
    onChanged?.();
  };

  // CommittedGate 发布：**稳定 host layout commit**（render 不写 refs）。cleanup 发布
  // withdrawal（unmount / key 切换）：撤 prewrite；issued 与 Done{accepted} 保留。
  // 依赖全部用**标量**（gateFact 存在性/decision/revision）——父层同值新对象 rerender
  // 不得重跑本 effect（cleanup 的 withdrawal 会误撤配对 prewrite）；回执回声解除
  // Done 的既有修复不回退（readOnly 分支整体不发布 gate）。
  // gateFact 三态：undefined = 未提供（旧独立 fixture，view 即 canonical 模拟）；
  // null = 显式无服务端帧事实（不拿 view/回执冒充 canonical，Done 不释放）；
  // 对象 = 服务端帧事实。
  const gateFactExplicitNull = gateFact === null;
  const gateFactPresent = gateFact !== undefined && gateFact !== null;
  const gateFactDecision = gateFactPresent ? gateFact.decision : null;
  const gateFactRevision = gateFactPresent ? gateFact.revision : null;
  useLayoutEffect(() => {
    // readOnly（历史记录）**不拥有 gate**：既不发布准入，也不在挂载/卸载时写共享
    // gate——同 scope 的活动实例（dock）可能正在配对，只读条目的寿命不得撤销它。
    // 只读条目唯一的写出口是槽内已冻结 issued 的原包重试（retry 的 readOnly 分支
    // 免 gate 准入，但仍受 disabled 与槽状态同步守卫）。
    if (readOnly) return;
    publishQuestionSkipGate(key, {
      scopeKey: key,
      disabled,
      fact: gateFactExplicitNull
        ? null
        : gateFactPresent && gateFactDecision !== null && gateFactRevision !== null
          ? { decision: gateFactDecision, revision: gateFactRevision }
          : { decision: view.decision, revision: view.revision },
    });
    return () => {
      publishQuestionSkipGate(key, { scopeKey: key, disabled: true, fact: null });
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [
    key,
    disabled,
    readOnly,
    gateFactPresent,
    gateFactExplicitNull,
    gateFactDecision,
    gateFactRevision,
    view.decision,
    view.revision,
  ]);

  if (readOnly) {
    // 只读历史条目**不遮已签发操作**：Unknown{issued} 的原包重试入口必须在场
    // （决策翻 active 只改展示分支，不撤 issued）。readOnly 实例不拥有 gate，重试
    // 走控制器 readOnly 分支：只重发本槽已冻结的原包，不新发动作；disabled 仍同步
    // 拒绝，按钮 state 同步反映该拒绝（供能面与权威一致）。
    // 只读条目唯一的写出口是槽内已冻结 issued 的原包重试（retry 的 readOnly 分支
    // 免 gate 准入，但仍受 disabled 与槽状态同步守卫）。重试成功的 onChanged 只在
    // 本实例仍挂载于同一 key 且未 disabled 时发出（guardedOnChanged）。
    const pendingRetry =
      snapshot.state === "unknown" && snapshot.issued !== null ? (
        <button
          type="button"
          data-testid="question-skip-retry"
          disabled={disabled}
          onClick={() =>
            void retryQuestionSkipIssued(key, {
              onChanged: onChanged === undefined ? undefined : () => guardedOnChanged(key),
              readOnly: true,
              disabled,
            })
          }
          className="mt-1 rounded-lg border border-[var(--cb)] px-3 py-1.5 text-xs disabled:opacity-40"
        >
          {i18n.t("mobile.codex.skip.retry")}
        </button>
      ) : null;
    return (
      <div
        data-testid="question-skip-record"
        role="group"
        aria-label={ariaLabel ?? questionTitle ?? undefined}
        className="rounded-xl border border-[var(--cb)] bg-[var(--cbg)] px-3 py-2 text-xs text-[var(--mut)]"
      >
        <p className={questionTitle === null ? "break-all" : "truncate"}>
          {questionTitle ?? ariaLabel ?? i18n.t("mobile.codex.skip.questionFallback")}
        </p>
        <p>{i18n.t("mobile.codex.skip.readRestoredRecord")}</p>
        {pendingRetry}
      </div>
    );
  }

  const effective = resolveEffectiveQuestionSkipDecision(snapshot.accepted, {
    decision: view.decision,
    revision: view.revision,
  });
  const pairing = snapshot.pairing;
  const issuedKind = snapshot.issued?.kind ?? null;
  const errorText = errorCopy(snapshot.error);
  const titleLine = questionTitle ?? ariaLabel ?? null;
  // Display-only 次级入口：ready（配对前）与当前 pairing（原生 preview 错误/合法空
  // 候选降级）都提供；admission（含 done+effective active）在控制器守卫。
  const displayOnlyVisible =
    (snapshot.state === "ready" || snapshot.state === "pairing") && effective.decision === "active";
  const displayOnlyButton = displayOnlyVisible ? (
    <button
      type="button"
      data-testid="question-skip-display-only"
      onClick={() => void issueQuestionSkipDisplayOnly(key, { sessionId, group, onChanged })}
      className="self-start text-[11px] text-[var(--mut)] underline"
    >
      {i18n.t("mobile.codex.skip.displayOnly")}
    </button>
  ) : null;

  const pairingPanel =
    snapshot.state === "pairing" && pairing !== null ? (
      <div className="space-y-2 text-xs">
        <p className="font-medium text-[var(--tx)]">{i18n.t("mobile.codex.skip.pairingTitle")}</p>
        {pairing.candidates.map((candidate) => (
          <button
            key={candidate.token}
            type="button"
            data-testid={`question-skip-candidate-${candidate.token}`}
            aria-pressed={pairing.selectedToken === candidate.token}
            onClick={() => selectQuestionSkipCandidate(key, candidate.token)}
            className="block w-full rounded-lg border border-[var(--cb)] px-2 py-2 text-left aria-pressed:border-[var(--btnp)] aria-pressed:bg-[var(--bub)]"
          >
            <span className="flex items-start gap-2 font-medium">
              <span aria-hidden="true" className="shrink-0 text-[var(--tx)]">
                {pairing.selectedToken === candidate.token ? "●" : "○"}
              </span>
              <span>{candidate.question}</span>
            </span>
            {candidate.options.length > 0 && (
              <span className="block text-[var(--mut)]">{candidate.options.join(" / ")}</span>
            )}
            {typeof candidate.label === "string" && candidate.label.length > 0 && (
              <span
                data-testid={`question-skip-candidate-label-${candidate.token}`}
                className="block text-[11px] text-[var(--mut)]"
              >
                {candidate.label}
              </span>
            )}
          </button>
        ))}
        {pairing.selectedToken !== null && (
          <div className="space-y-1">
            {(["thread", "count", "draft"] as const).map((field) => (
              <button
                key={field}
                type="button"
                data-testid={`question-skip-ack-${field}`}
                aria-pressed={pairing.acks[field]}
                onClick={() => setQuestionSkipAck(key, field, !pairing.acks[field])}
                className="flex min-h-9 w-full items-start gap-2 rounded-md px-1 py-2 text-left aria-pressed:bg-[var(--bub)]"
              >
                <span
                  aria-hidden="true"
                  className="inline-flex size-4 shrink-0 items-center justify-center rounded border border-[var(--mut)] text-[var(--tx)]"
                >
                  {pairing.acks[field] ? "✓" : ""}
                </span>
                <span>
                  {i18n.t(
                    field === "thread"
                      ? "mobile.codex.skip.ackThread"
                      : field === "count"
                        ? "mobile.codex.skip.ackCount"
                        : "mobile.codex.skip.ackDraft"
                  )}
                </span>
              </button>
            ))}
          </div>
        )}
        <button
          type="button"
          data-testid="question-skip-confirm"
          disabled={
            disabled ||
            pairing.selectedToken === null ||
            !(pairing.acks.thread && pairing.acks.count && pairing.acks.draft) ||
            !questionSkipCandidatesDistinguishable(pairing.candidates)
          }
          onClick={() => void confirmQuestionSkipPairing(key, { sessionId, group, onChanged })}
          className="rounded-lg bg-[var(--btnp)] px-3 py-1.5 text-xs font-medium text-[var(--btnpt)] disabled:opacity-40"
        >
          {i18n.t("mobile.codex.skip.confirm")}
        </button>
      </div>
    ) : null;

  const startButton =
    snapshot.state === "ready" && effective.decision === "active" ? (
      <button
        type="button"
        data-testid="question-skip-start"
        disabled={disabled}
        onClick={() =>
          void openQuestionSkipPairing(key, {
            sessionId,
            group,
            effectiveDecision: effective.decision,
          })
        }
        className="rounded-lg border border-[var(--cb)] px-3 py-1.5 text-xs disabled:opacity-40"
      >
        {snapshot.previewing
          ? i18n.t("mobile.codex.skip.pairingBusy")
          : i18n.t("mobile.codex.skip.start")}
      </button>
    ) : null;

  // 原生效果择新：accepted 回执只在 **revision 不落后于 view** 时作为效果来源；
  // canonical 投影（view）已带更高 revision 的事实（哪怕 unresolved）时以它为准
  // ——旧 accepted closed 不得覆盖新 unknown。reason 随效果来源走（回执/view）。
  const acceptedOutcome =
    snapshot.accepted !== null &&
    snapshot.accepted.kind === "skip" &&
    snapshot.accepted.resultRevision >= view.revision
      ? snapshot.accepted
      : null;
  const effectFact =
    acceptedOutcome !== null ? acceptedOutcome.nativeEffect : view.latestNativeEffect;
  const effectReason = acceptedOutcome !== null ? acceptedOutcome.reasonCode : view.reasonCode;
  const effectText =
    effective.decision === "skipped" && effectFact !== null
      ? i18n.t(`mobile.codex.skip.effect_${effectFact}`) +
        (effectReason !== null ? ` (${effectReason})` : "")
      : null;

  // restore 按钮：skipped 世界里 ready/done 可发新 restore；writing/unknown{restore}
  // 在场但禁用/由 handler 拒绝（issued 未决时同 kind 新 ID 被控制器拒绝——按钮在
  // 场是供能面，拒绝是权威）。
  const restoreVisible =
    effective.decision === "skipped" &&
    (snapshot.state === "ready" ||
      snapshot.state === "done" ||
      (snapshot.state === "writing" && issuedKind === "restore") ||
      (snapshot.state === "unknown" && issuedKind === "restore"));
  const restoreButton = restoreVisible ? (
    <button
      type="button"
      data-testid="question-skip-restore"
      disabled={disabled || snapshot.state === "writing"}
      onClick={() => void beginQuestionSkipRestore(key, { sessionId, group, onChanged })}
      className="rounded-lg border border-[var(--cb)] px-3 py-1.5 text-xs disabled:opacity-40"
    >
      {snapshot.state === "writing" && issuedKind === "restore"
        ? i18n.t("mobile.codex.skip.restoringBusy")
        : i18n.t("mobile.codex.skip.restore")}
    </button>
  ) : null;

  const retryButton =
    snapshot.state === "unknown" && issuedKind !== null ? (
      <button
        type="button"
        data-testid="question-skip-retry"
        onClick={() => void retryQuestionSkipIssued(key, { onChanged })}
        className="rounded-lg border border-[var(--cb)] px-3 py-1.5 text-xs"
      >
        {i18n.t("mobile.codex.skip.retry")}
      </button>
    ) : null;

  const busyText =
    snapshot.state === "writing" && issuedKind !== null
      ? issuedKind === "skip"
        ? i18n.t("mobile.codex.skip.skippingBusy")
        : i18n.t("mobile.codex.skip.restoringBusy")
      : null;
  const restoredNote =
    snapshot.state === "done" &&
    snapshot.accepted !== null &&
    snapshot.accepted.kind === "restore" ? (
      <p data-testid="question-skip-restored" className="text-xs text-emerald-600">
        {i18n.t("mobile.codex.skip.restored")}
      </p>
    ) : null;

  return (
    <div
      data-testid="question-skip-controls"
      role="group"
      aria-label={ariaLabel ?? questionTitle ?? undefined}
      className="rounded-xl border border-[var(--cb)] bg-[var(--cbg)] px-3 py-2"
    >
      {titleLine !== null && (
        <p
          data-testid="question-skip-title"
          className={`text-xs text-[var(--mut)] ${questionTitle === null ? "break-all" : "truncate"}`}
        >
          {titleLine}
        </p>
      )}
      {startButton}
      {pairingPanel}
      {displayOnlyButton}
      {snapshot.state === "writing" && issuedKind === null && (
        <p className="text-xs text-[var(--mut)]">{i18n.t("mobile.codex.skip.pairingBusy")}</p>
      )}
      {busyText !== null && <p className="text-xs text-[var(--mut)]">{busyText}</p>}
      {effectText !== null && (
        <p data-testid={`question-skip-effect-${effectFact}`} className="text-xs text-[var(--mut)]">
          {effectText}
        </p>
      )}
      {restoredNote}
      {(restoreButton !== null || retryButton !== null) && (
        <div className="mt-1 flex items-center gap-2">
          {restoreButton}
          {retryButton}
        </div>
      )}
      {errorText !== "" && (
        <p
          data-testid="question-skip-error"
          className="mt-1 text-xs text-amber-700 dark:text-amber-400"
        >
          {errorText}
        </p>
      )}
    </div>
  );
}
