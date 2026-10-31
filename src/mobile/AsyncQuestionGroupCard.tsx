/**
 * B04/B05/B06 + J03—J09：同组多题单卡逐题填写、一次提交。
 *
 * 关键不变量：
 * - 只有 `pending/temporarily_blocked` 成员可填写；已答/待核对单独列出；
 * - 组身份覆盖全部成员；成员换代保留其他稿，并**隔离旧稿**供查看/恢复/丢弃；
 * - Accepted 仅受理，自动/手动核对都有单次与整体时间上界；Unknown 只核对；
 * - 核对响应按 actionId+轮次+退休集合收敛，旧响应不能重新锁死已退休动作；
 * - 显式复核保存**具体已答 key 集合**，新增已答成员必须重新确认；
 * - 整组转 Unknown 时组件不被父层卸载（由 SessionDetail 选组保持挂载）；
 * - 暂停提示只在确有暂停事实时显示一次；正常可答组不误报；
 * - T04 紧凑口径（spec §3）：无回执不留空行；自由填写默认收起、显式收起不丢稿、
 *   有自由稿自动展开；长题干按实际布局三行收纳（line-clamp-3 恒开），「展开全文」
 *   入口由实测溢出驱动（scrollHeight > clientHeight，禁字符数判据）；题干/选项在
 *   `[data-font-scale]` 作用域内跟随字号档位（100% 基准 14px），按钮与卡头保持
 *   固定操作尺寸。
 */
import { useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState } from "react";
import i18n from "@/i18n";
import {
  answerCodexAsyncGroup,
  isCodexPreDeliveryRefusal,
  newActionId,
  verifyCodexAction,
  type ActionReceipt,
  type CodexAsyncQuestionView,
  type CodexActionVerifyView,
} from "./codexIpcApi";
import { ApiError } from "./api";
import InteractiveCard from "./InteractiveCard";
import QuestionChoiceRow from "./QuestionChoiceRow";

const SUBMIT_DEADLINE_MS = 30_000;
const VERIFY_TIMEOUT_MS = 10_000;
const CONFIRM_TOTAL_MS = 120_000;
const CONFIRM_POLL_MS = 2_000;
const PAUSE_REASON = "operation_queue_paused";

type Phase = "idle" | "submitting" | "submitted" | "unknown" | "confirmed";

type StaleDraft = {
  id: string;
  questionKey: string;
  title: string;
  selected: string | null;
  text: string;
};

type VerifyResult =
  | { kind: "stale" }
  | { kind: "timeout" }
  | { kind: "failed" }
  | { kind: "view"; view: CodexActionVerifyView };

function memberIdentity(question: CodexAsyncQuestionView): string {
  return JSON.stringify([
    question.questionKey,
    question.contextToken,
    question.questionIndex,
    question.title,
    question.options,
  ]);
}

function draftKey(question: CodexAsyncQuestionView): string {
  return `${question.questionKey}\u0000${question.contextToken}`;
}

function groupStorageKey(sessionId: string, members: CodexAsyncQuestionView[]): string {
  return `codex:group:${sessionId}:${members[0].sourceTurnId}:${members[0].sourceItemId}`;
}

export interface AsyncQuestionGroupCardProps {
  sessionId: string;
  /** 同来源组的**全部**成员（含已答/待核对），用于稳定计数和部分复核。 */
  questions: CodexAsyncQuestionView[];
  onChanged?: () => void;
  /** J08：在 Tuvis 中按组收纳（不向 Desktop 发送答案/取消/打断）。 */
  onArchive?: () => void;
  /**
   * 字号档位（spec §3.3）：题干/选项/自由输入内容在 `[data-font-scale]` 作用域内
   * 跟随档位（100% 基准 14px = text-sm 相对单位）；按钮与卡头**不在**作用域内，
   * 保持稳定操作尺寸。默认 1（主题原值）。
   */
  fontScale?: number;
}

export default function AsyncQuestionGroupCard({
  sessionId,
  questions,
  onChanged,
  onArchive,
  fontScale = 1,
}: AsyncQuestionGroupCardProps) {
  const members = useMemo(
    () => [...questions].sort((a, b) => a.questionIndex - b.questionIndex),
    [questions]
  );
  const live = useMemo(
    () => members.filter((q) => q.state === "pending" || q.state === "temporarily_blocked"),
    [members]
  );
  const answered = useMemo(() => members.filter((q) => q.state === "answered"), [members]);
  const unsettled = useMemo(
    () =>
      members.filter(
        (q) => q.state !== "pending" && q.state !== "temporarily_blocked" && q.state !== "answered"
      ),
    [members]
  );
  const fingerprint = useMemo(() => members.map(memberIdentity).join("|"), [members]);
  const storageKey = useMemo(
    () => (members.length > 0 ? groupStorageKey(sessionId, members) : ""),
    [sessionId, members]
  );
  const [index, setIndex] = useState(0);
  const [drafts, setDrafts] = useState<Record<string, { selected: string | null; text: string }>>(
    {}
  );
  // T04（spec §3.5）：自由填写默认收起；用户的显式展开/收起按题记忆，覆盖自动行为
  // （undefined = 未显式操作 → 有自由稿自动展开）。显式收起**不丢稿**——稿在 drafts。
  const [freeExplicit, setFreeExplicit] = useState<Record<string, boolean>>({});
  // T04（spec §3.6，集中审查 D4 修订）：长题干三行收纳按**实测布局溢出**判定——
  // line-clamp-3 恒开（展开态除外），「展开全文」入口只在量得 scrollHeight >
  // clientHeight（三行装不下）时在场。窄屏/125% 字号下 80 字以内也可能超三行，
  // 字符数启发式不等价于按实际三行收纳（审查点名必修，禁字符数判据）。
  // 展开态仍按题记忆（titleExpanded）。
  const titleRef = useRef<HTMLParagraphElement | null>(null);
  const [titleOverflow, setTitleOverflow] = useState<Record<string, boolean>>({});
  const [titleExpanded, setTitleExpanded] = useState<Record<string, boolean>>({});
  const [staleDrafts, setStaleDrafts] = useState<StaleDraft[]>([]);
  const [staleOpen, setStaleOpen] = useState(false);
  const [phase, setPhase] = useState<Phase>("idle");
  const [receipt, setReceipt] = useState<ActionReceipt | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [timedOut, setTimedOut] = useState(false);
  const [confirmedAnsweredKeys, setConfirmedAnsweredKeys] = useState<string[]>([]);
  const [confirmedCount, setConfirmedCount] = useState(0);
  const [hasAnchor, setHasAnchor] = useState(false);
  const [verifying, setVerifying] = useState(false);
  const actionIdRef = useRef<string | null>(null);
  const submitEpochRef = useRef(0);
  const verifySeqRef = useRef(0);
  const retiredActionsRef = useRef<Set<string>>(new Set());
  const deadlineRef = useRef<{ epoch: number; timer: number } | null>(null);
  const pollRef = useRef<number | null>(null);
  const pollTokenRef = useRef(0);
  const aliveRef = useRef(true);
  const membersRef = useRef(members);
  const currentKey = live[Math.min(index, Math.max(0, live.length - 1))]?.questionKey;
  const currentIndex = Math.max(
    0,
    live.findIndex((q) => q.questionKey === currentKey)
  );
  const current = live[currentIndex];

  const clearDeadline = (epoch: number) => {
    if (deadlineRef.current !== null && deadlineRef.current.epoch === epoch) {
      window.clearTimeout(deadlineRef.current.timer);
      deadlineRef.current = null;
    }
  };
  const clearPoll = () => {
    pollTokenRef.current += 1;
    if (pollRef.current !== null) {
      window.clearTimeout(pollRef.current);
      pollRef.current = null;
    }
  };

  useEffect(() => {
    aliveRef.current = true;
    return () => {
      aliveRef.current = false;
      if (deadlineRef.current !== null) window.clearTimeout(deadlineRef.current.timer);
      if (pollRef.current !== null) window.clearTimeout(pollRef.current);
    };
  }, []);

  /** J04：退休时推进轮次并登记 action，旧核对响应不得回写。 */
  const retireAnchor = (_epoch: number, actionId: string) => {
    if (actionIdRef.current !== actionId) return;
    retiredActionsRef.current.add(actionId);
    submitEpochRef.current += 1;
    actionIdRef.current = null;
    setHasAnchor(false);
    clearPoll();
    if (window.sessionStorage.getItem(storageKey) === actionId) {
      window.sessionStorage.removeItem(storageKey);
    }
  };

  // J06：任一成员换代 → 推进轮次，旧稿移到独立隔离区（可查看/恢复/丢弃）。
  useEffect(() => {
    submitEpochRef.current += 1;
    setIndex(0);
    setPhase("idle");
    setReceipt(null);
    setError(null);
    setTimedOut(false);
    setConfirmedAnsweredKeys([]);
    clearPoll();
    const previousMembers = new Map(
      membersRef.current.map((question) => [draftKey(question), question])
    );
    setDrafts((prev) => {
      const valid = new Map(members.map((q) => [draftKey(q), q]));
      const next: Record<string, { selected: string | null; text: string }> = {};
      const stale: StaleDraft[] = [];
      for (const [key, draft] of Object.entries(prev)) {
        if (valid.has(key)) {
          next[key] = draft;
          continue;
        }
        if ((draft.text ?? "").trim().length > 0 || draft.selected !== null) {
          const source = previousMembers.get(key);
          stale.push({
            id: key,
            questionKey: source?.questionKey ?? key,
            title: source?.title ?? key,
            selected: draft.selected,
            text: draft.text,
          });
        }
      }
      if (stale.length > 0) setStaleDrafts((old) => [...stale, ...old]);
      return next;
    });
    membersRef.current = members;
    const anchor = window.sessionStorage.getItem(storageKey);
    actionIdRef.current = anchor;
    setHasAnchor(anchor !== null && !retiredActionsRef.current.has(anchor));
    if (anchor && !retiredActionsRef.current.has(anchor)) setPhase("unknown");
    if (deadlineRef.current !== null) {
      window.clearTimeout(deadlineRef.current.timer);
      deadlineRef.current = null;
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [fingerprint, storageKey]);

  // 全部成员正式回答 → 立即核销（投影 answered 即 Desktop 已收下答案；
  // 不再要求本卡先经历 submitted 阶段，否则 Unknown 恢复的卡会永远堆在底部）。
  useEffect(() => {
    if (
      members.length > 0 &&
      members.every((q) => q.state === "answered") &&
      phase !== "confirmed"
    ) {
      const actionId = actionIdRef.current;
      if (actionId !== null) retireAnchor(submitEpochRef.current, actionId);
      setPhase("confirmed");
      setConfirmedCount(members.length);
      clearPoll();
      onChanged?.();
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [members, phase]);

  const draftFor = (question: CodexAsyncQuestionView) =>
    drafts[draftKey(question)] ?? { selected: null, text: "" };
  const answerFor = (question: CodexAsyncQuestionView): string => {
    const draft = draftFor(question);
    if (draft.text.trim().length > 0) return draft.text.trim();
    // 防御：只有当前 options 里真实存在的 selected 才算有效答案；旧题被移除的
    // 选项绝不能在“看不见”的情况下提交。
    return draft.selected !== null && question.options.includes(draft.selected)
      ? draft.selected
      : "";
  };
  const setDraft = (
    question: CodexAsyncQuestionView,
    patch: Partial<{ selected: string | null; text: string }>
  ) => {
    const key = draftKey(question);
    setDrafts((prev) => ({
      ...prev,
      [key]: {
        selected: prev[key]?.selected ?? null,
        text: prev[key]?.text ?? "",
        ...patch,
      },
    }));
  };

  const allLiveAnswered = live.length > 0 && live.every((q) => answerFor(q).length > 0);
  /** T04：自由填写展开态——显式操作优先；未显式操作时，有自由稿自动展开。 */
  const freeOpenFor = (question: CodexAsyncQuestionView): boolean => {
    const explicit = freeExplicit[draftKey(question)];
    if (explicit !== undefined) return explicit;
    return draftFor(question).text.trim().length > 0;
  };
  const titleExpandedFor = (question: CodexAsyncQuestionView): boolean =>
    titleExpanded[draftKey(question)] ?? false;
  /** 三行收纳的实测重算：挂载/换题/展开收起/字号档位变化时量一次；ResizeObserver
   *  覆盖分屏拖动、旋屏等元素盒宽变化（window 不动也触发），window resize 兜底
   *  无 RO 环境（jsdom 测试亦经此或 deps 重跑触发重测）。 */
  useLayoutEffect(() => {
    const el = titleRef.current;
    if (el === null || current === undefined) return;
    const key = draftKey(current);
    const measure = () => {
      // 展开态不覆写溢出记忆：展开后 scrollHeight === clientHeight，照量会把
      // 「收起」入口量没；溢出记忆只由收起态（line-clamp-3 在场）刷新。
      if (titleExpanded[key]) return;
      const overflow = el.scrollHeight > el.clientHeight;
      setTitleOverflow((prev) => (prev[key] === overflow ? prev : { ...prev, [key]: overflow }));
    };
    measure();
    const observer = new ResizeObserver(measure);
    observer.observe(el);
    window.addEventListener("resize", measure);
    return () => {
      observer.disconnect();
      window.removeEventListener("resize", measure);
    };
  }, [current, titleExpanded, fontScale]);
  const hasPause = live.some((q) => !q.actionable && q.disabledReasonCode === PAUSE_REASON);
  const submittable =
    live.length > 0 && live.every((q) => q.actionable || q.disabledReasonCode === PAUSE_REASON);
  const blockedQuestion = live.find((q) => !q.actionable && q.disabledReasonCode !== PAUSE_REASON);
  const blockedReason =
    blockedQuestion?.disabledReasonCode ?? (unsettled.length > 0 ? "question_unsettled" : null);
  const confirmedSet = new Set(confirmedAnsweredKeys);
  const acknowledgedAnswered = answered.filter((q) => confirmedSet.has(q.questionKey));
  const needsConfirm = answered.length > 0 && acknowledgedAnswered.length !== answered.length;
  const canSubmit =
    phase === "idle" &&
    allLiveAnswered &&
    submittable &&
    blockedReason === null &&
    !needsConfirm &&
    !timedOut;

  const reasonText = (code: string | null): string => {
    if (!code) return "";
    if (code === PAUSE_REASON) return i18n.t("mobile.codex.async.queuePausedNote");
    const known: Record<string, string> = {
      operation_runtime_active: "mobile.codex.async.reasons.running",
      operation_blocking_pending: "mobile.codex.async.reasons.pendingNative",
      operation_contract_unmatched: "mobile.codex.async.reasons.noContract",
      operation_no_evidence: "mobile.codex.async.reasons.noEvidence",
      operation_auto_adapt_off: "mobile.codex.async.reasons.adaptOff",
      question_unsettled: "mobile.codex.async.reasons.unsettled",
    };
    if (known[code]) return i18n.t(known[code]);
    // 其余机器码走通用 errors.* 文案（与单题卡同源）；未知码原样显示，不猜。
    const generic = `errors.${code.replace(/_([a-z0-9])/g, (_, c: string) => c.toUpperCase())}`;
    return i18n.t(generic, { defaultValue: code });
  };

  /** J03/J04：单次 10s 有界 + 序号/退休/轮次守卫；旧响应一律丢弃。 */
  const verifyWithGuards = useCallback(
    async (actionId: string, epoch: number): Promise<VerifyResult> => {
      const seq = ++verifySeqRef.current;
      const controller = new AbortController();
      const abortTimer = window.setTimeout(() => controller.abort(), VERIFY_TIMEOUT_MS);
      const timeout = new Promise<"timeout">((resolve) => {
        window.setTimeout(() => resolve("timeout"), VERIFY_TIMEOUT_MS);
      });
      const stale = () =>
        !aliveRef.current ||
        seq !== verifySeqRef.current ||
        submitEpochRef.current !== epoch ||
        actionIdRef.current !== actionId ||
        retiredActionsRef.current.has(actionId);
      try {
        const result = await Promise.race([
          verifyCodexAction(actionId, controller.signal),
          timeout,
        ]);
        if (stale()) return { kind: "stale" };
        if (result === "timeout") return { kind: "timeout" };
        return { kind: "view", view: result };
      } catch {
        if (stale()) return { kind: "stale" };
        return { kind: "failed" };
      } finally {
        window.clearTimeout(abortTimer);
      }
    },
    []
  );

  const handleVerifyView = useCallback(
    (actionId: string, epoch: number, view: CodexActionVerifyView) => {
      const keys = view.answeredQuestionKeys ?? [];
      setConfirmedCount(keys.length);
      if (members.every((q) => keys.includes(q.questionKey))) {
        retireAnchor(epoch, actionId);
        setPhase("confirmed");
        onChanged?.();
        return "confirmed" as const;
      }
      if (view.outcome === "not_delivered" && view.stage === "settled") {
        retireAnchor(epoch, actionId);
        setTimedOut(false);
        setReceipt({
          actionId,
          delivery: "not_sent",
          route: "none",
          turnId: null,
          envelopeId: null,
          reasonCode: view.reasonCode,
        });
        setPhase("idle");
        setError(
          i18n.t("mobile.codex.async.submitFailed", {
            message: view.reasonCode ?? "not_sent",
          })
        );
        return "not_sent" as const;
      }
      setPhase("submitted");
      return "waiting" as const;
    },
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [members, onChanged]
  );

  /** 自动核对：单次有界 + 整体 120s 有界；到界进 Unknown 并保留核对入口。 */
  const startConfirmPoll = useCallback(
    (actionId: string, epoch: number) => {
      clearPoll();
      const token = pollTokenRef.current;
      const overallDeadline = Date.now() + CONFIRM_TOTAL_MS;
      const tick = async () => {
        if (token !== pollTokenRef.current) return;
        const result = await verifyWithGuards(actionId, epoch);
        if (result.kind === "stale" || token !== pollTokenRef.current) return;
        if (result.kind === "view") {
          const outcome = handleVerifyView(actionId, epoch, result.view);
          if (outcome !== "waiting") return;
        }
        if (Date.now() >= overallDeadline) {
          setPhase("unknown");
          return;
        }
        pollRef.current = window.setTimeout(() => void tick(), CONFIRM_POLL_MS);
      };
      void tick();
    },
    [verifyWithGuards, handleVerifyView]
  );

  const submit = async () => {
    if (!canSubmit) return;
    const epoch = ++submitEpochRef.current;
    setPhase("submitting");
    setError(null);
    setTimedOut(false);
    setReceipt(null);
    clearPoll();
    if (deadlineRef.current !== null) window.clearTimeout(deadlineRef.current.timer);
    deadlineRef.current = {
      epoch,
      timer: window.setTimeout(() => {
        if (submitEpochRef.current !== epoch) return;
        setTimedOut(true);
        setPhase("unknown");
      }, SUBMIT_DEADLINE_MS),
    };
    if (actionIdRef.current === null) {
      actionIdRef.current = newActionId();
      setHasAnchor(true);
      retiredActionsRef.current.delete(actionIdRef.current);
      window.sessionStorage.setItem(storageKey, actionIdRef.current);
    }
    const actionId = actionIdRef.current;
    const answers = live.map((q) => ({
      questionKey: q.questionKey,
      contextToken: q.contextToken,
      answer: answerFor(q),
    }));
    const ack = answered.filter((q) => confirmedSet.has(q.questionKey)).map((q) => q.questionKey);
    const startedAt = Date.now();
    const stale = () => !aliveRef.current || submitEpochRef.current !== epoch;
    try {
      const r = await answerCodexAsyncGroup({
        sessionId,
        actionId,
        answers,
        acknowledgedAnswered: ack,
        frontBudgetMs: Math.max(0, SUBMIT_DEADLINE_MS - (Date.now() - startedAt)),
      });
      if (stale()) return;
      clearDeadline(epoch);
      setReceipt(r);
      if (r.delivery === "accepted") {
        setPhase("submitted");
        startConfirmPoll(actionId, epoch);
      } else if (r.delivery === "rejected") {
        retireAnchor(epoch, actionId);
        setPhase("idle");
        setError(i18n.t("mobile.codex.async.groupRejected"));
      } else if (r.delivery === "not_sent") {
        retireAnchor(epoch, actionId);
        setTimedOut(false);
        setPhase("idle");
        setError(
          i18n.t("mobile.codex.async.submitFailed", {
            message: r.reasonCode ?? "not_sent",
          })
        );
      } else {
        setPhase("unknown");
      }
    } catch (e) {
      if (stale()) return;
      clearDeadline(epoch);
      if (
        e instanceof ApiError &&
        (isCodexPreDeliveryRefusal(e) || (e.status !== null && e.status < 500))
      ) {
        retireAnchor(epoch, actionId);
        setTimedOut(false);
        setPhase("idle");
        const reason = typeof e.data?.reason === "string" ? e.data.reason : e.message;
        setError(i18n.t("mobile.codex.async.submitFailed", { message: reason }));
        return;
      }
      setPhase("unknown");
    }
  };

  const reverify = async () => {
    const actionId = actionIdRef.current;
    if (!actionId) return;
    setVerifying(true);
    const epoch = submitEpochRef.current;
    try {
      const result = await verifyWithGuards(actionId, epoch);
      if (result.kind === "stale") return;
      if (result.kind === "view") {
        const outcome = handleVerifyView(actionId, epoch, result.view);
        if (outcome === "waiting") startConfirmPoll(actionId, epoch);
      } else {
        setPhase("unknown");
      }
    } finally {
      if (aliveRef.current) setVerifying(false);
    }
  };

  const restoreStale = (stale: StaleDraft) => {
    const target = members.find((q) => q.questionKey === stale.questionKey);
    if (!target) return;
    const hasText = (stale.text ?? "").trim().length > 0;
    // R01：旧选项可能已被新题移除。只有仍存在时才恢复为选中；否则把旧标签
    // 落到**可见自由输入**，保证“看见什么就提交什么”，绝不留下隐形 selected。
    const optionExists = stale.selected !== null && target.options.includes(stale.selected);
    setDrafts((prev) => ({
      ...prev,
      [draftKey(target)]: hasText
        ? { selected: null, text: stale.text }
        : optionExists
          ? { selected: stale.selected, text: "" }
          : { selected: null, text: stale.selected ?? "" },
    }));
    setStaleDrafts((prev) => prev.filter((item) => item.id !== stale.id));
    setIndex(0);
  };

  if (members.length === 0) return null;

  const locked = phase === "submitting" || phase === "submitted" || phase === "confirmed";
  const canGoNext =
    current !== undefined &&
    answerFor(current).length > 0 &&
    phase !== "submitting" &&
    phase !== "submitted" &&
    phase !== "confirmed";
  const fallback = live.length === 0 && !hasAnchor && unsettled.length > 0;

  return (
    <InteractiveCard
      tone="question"
      testId="codex-async-question"
      mode="async-question"
      dataAttributes={{
        phase,
        progress: `${live.length === 0 ? 0 : currentIndex + 1}/${live.length}`,
      }}
      title={
        <>
          {i18n.t("mobile.codex.async.title")}
          {live.length > 1 && (
            <span className="ml-2 text-xs text-[var(--mut)]" data-testid="codex-async-progress">
              {i18n.t("mobile.codex.async.progress", {
                current: currentIndex + 1,
                total: live.length,
              })}
            </span>
          )}
        </>
      }
      titleSuffix={
        <span className="text-[10px] text-[var(--mut)]">
          {i18n.t("mobile.codex.async.members", {
            answered: answered.length,
            total: members.length,
          })}
        </span>
      }
    >
      {hasPause && (
        <p
          data-testid="codex-async-pause-note"
          className="mt-2 rounded-lg bg-amber-500/10 px-2 py-1 text-xs text-amber-800 dark:text-amber-300"
        >
          {i18n.t("mobile.codex.async.queuePausedNote")}
        </p>
      )}
      {staleDrafts.length > 0 && (
        <div
          data-testid="codex-async-stale-note"
          className="mt-2 rounded-lg bg-amber-500/10 px-2 py-1 text-xs text-amber-800 dark:text-amber-300"
        >
          <button
            type="button"
            data-testid="codex-async-stale-toggle"
            onClick={() => setStaleOpen((value) => !value)}
            className="underline"
          >
            {i18n.t("mobile.codex.async.staleDraft", { count: staleDrafts.length })}
          </button>
          {staleOpen &&
            staleDrafts.map((stale) => (
              <div
                key={stale.id}
                data-testid="codex-async-stale-item"
                className="mt-1 flex flex-wrap items-center justify-between gap-2"
              >
                <span className="min-w-0 truncate">
                  {stale.title}: {stale.text || stale.selected}
                </span>
                <span className="flex shrink-0 gap-2">
                  <button
                    type="button"
                    data-testid="codex-async-stale-restore"
                    onClick={() => restoreStale(stale)}
                    className="underline"
                  >
                    {i18n.t("mobile.codex.async.restoreDraft")}
                  </button>
                  <button
                    type="button"
                    data-testid="codex-async-stale-discard"
                    onClick={() =>
                      setStaleDrafts((prev) => prev.filter((item) => item.id !== stale.id))
                    }
                    className="underline"
                  >
                    {i18n.t("mobile.codex.async.discardDraft")}
                  </button>
                </span>
              </div>
            ))}
        </div>
      )}
      {answered.length > 0 && (
        <div
          data-testid="codex-async-answered-list"
          className="mt-2 rounded-lg bg-emerald-500/10 px-2 py-1 text-xs"
        >
          <p>{i18n.t("mobile.codex.async.alreadyAnswered", { count: answered.length })}</p>
          {answered.map((q) => (
            <p key={q.questionKey} className="truncate text-[var(--mut)]">
              {q.title}
            </p>
          ))}
          {live.length > 0 && (
            <button
              type="button"
              data-testid="codex-async-partial-confirm"
              aria-pressed={!needsConfirm}
              onClick={() => setConfirmedAnsweredKeys(answered.map((q) => q.questionKey))}
              className={`mt-1 rounded px-2 py-0.5 underline ${
                needsConfirm ? "text-amber-700" : "text-emerald-700"
              }`}
            >
              {needsConfirm
                ? i18n.t("mobile.codex.async.confirmPartial", { count: live.length })
                : i18n.t("mobile.codex.async.partialConfirmed")}
            </button>
          )}
        </div>
      )}
      {unsettled.length > 0 && (
        <p
          data-testid="codex-async-unsettled"
          className="mt-1 text-xs text-amber-700 dark:text-amber-400"
        >
          {i18n.t("mobile.codex.async.unsettled", { count: unsettled.length })}
        </p>
      )}

      {fallback ? (
        <div data-testid="codex-async-fallback" className="mt-2 space-y-1 text-xs">
          <p>{i18n.t("mobile.codex.async.fallbackHint")}</p>
          {unsettled.map((q) => (
            <div key={q.questionKey} className="rounded border border-[var(--bd)] px-2 py-1">
              <p className="font-medium">{q.title}</p>
              {q.options.length > 0 && <p className="text-[var(--mut)]">{q.options.join(" / ")}</p>}
            </div>
          ))}
          <p className="text-[var(--mut)]">{i18n.t("mobile.codex.async.fallbackBoundary")}</p>
        </div>
      ) : current !== undefined ? (
        <div className="mt-2 space-y-2" data-testid="codex-async-current">
          {/* T04（spec §3.3/§3.4，集中审查 D4 修订）：题干 100% 基准 14px（text-sm
              相对档位单位），在字号作用域内跟随顶部字号设置；卡头与按钮不在作用域内。
              三行收纳恒开（展开态除外），展开入口由下方实测溢出驱动。 */}
          <div data-font-scale={fontScale}>
            <p
              ref={titleRef}
              className={`text-sm font-medium ${titleExpandedFor(current) ? "" : "line-clamp-3"}`}
            >
              {current.title}
            </p>
          </div>
          {titleOverflow[draftKey(current)] === true && (
            <button
              type="button"
              data-testid="codex-async-title-toggle"
              onClick={() =>
                setTitleExpanded((prev) => ({
                  ...prev,
                  [draftKey(current)]: !titleExpandedFor(current),
                }))
              }
              className="text-[11px] text-[var(--mut)] underline"
            >
              {titleExpandedFor(current)
                ? i18n.t("mobile.codex.async.collapseTitle")
                : i18n.t("mobile.codex.async.expandTitle")}
            </button>
          )}
          {current.options.length > 0 && (
            // T04（spec §3.6）：长选项组内部滚动（30dvh 上限），题号与提交操作保持可达。
            <div
              data-font-scale={fontScale}
              className="flex max-h-[30dvh] flex-col gap-1 overflow-y-auto"
            >
              {current.options.map((option, optionIndex) => {
                const draft = draftFor(current);
                return (
                  <QuestionChoiceRow
                    key={option}
                    index={optionIndex}
                    label={option}
                    checked={draft.selected === option}
                    multiSelect={false}
                    disabled={locked}
                    onClick={() =>
                      // C04：选项与自由文本是互斥的可见回答模式；点选项即清掉
                      // 旧自由稿，避免“看着选了乙、实际发旧文本”。
                      setDraft(current, { selected: option, text: "" })
                    }
                  />
                );
              })}
            </div>
          )}
          {freeOpenFor(current) ? (
            <>
              {/* T04（spec §3.5）：自由填写区——输入文字与选项继续互斥；同一字号作用域 */}
              <div data-font-scale={fontScale}>
                <label className="block text-xs text-[var(--mut)]">
                  {i18n.t("mobile.codex.async.freeLabel")}
                  <input
                    data-testid="codex-async-free"
                    type="text"
                    value={draftFor(current).text}
                    disabled={locked}
                    maxLength={10000}
                    onChange={(event) =>
                      // 输入自由答案即退出选项模式；可见选中与实发内容保持一致。
                      setDraft(current, { selected: null, text: event.target.value })
                    }
                    className="mt-1 w-full rounded-lg border border-[var(--cb)] px-2.5 py-1.5 text-sm text-[var(--tx)] placeholder:text-[var(--mut)] focus:ring-2 focus:ring-[var(--btnp)] focus:outline-none disabled:opacity-50"
                  />
                </label>
              </div>
              <button
                type="button"
                data-testid="codex-async-free-collapse"
                onClick={() => setFreeExplicit((prev) => ({ ...prev, [draftKey(current)]: false }))}
                className="self-start text-[11px] text-[var(--mut)] underline"
              >
                {i18n.t("mobile.codex.async.dismiss")}
              </button>
            </>
          ) : (
            <button
              type="button"
              data-testid="codex-async-free-toggle"
              onClick={() => setFreeExplicit((prev) => ({ ...prev, [draftKey(current)]: true }))}
              className="self-start text-[11px] text-[var(--mut)] underline"
            >
              {i18n.t("mobile.codex.async.otherAnswers")}
            </button>
          )}
          {blockedReason && !current.actionable && (
            <p
              data-testid="codex-async-blocked-reason"
              className="text-xs text-amber-700 dark:text-amber-400"
            >
              {reasonText(blockedReason)}
            </p>
          )}
        </div>
      ) : (
        <p data-testid="codex-async-no-live" className="mt-2 text-xs text-[var(--mut)]">
          {i18n.t("mobile.codex.async.noLive")}
        </p>
      )}

      {live.length > 0 && (
        <footer className="mt-3 flex flex-wrap items-center gap-2">
          {live.length > 1 && currentIndex > 0 && (
            <button
              type="button"
              data-testid="codex-async-prev"
              disabled={phase === "submitting"}
              onClick={() => setIndex(currentIndex - 1)}
              className="rounded-full border border-[var(--bd)] px-2.5 py-1 text-xs text-[var(--mut)]"
            >
              {i18n.t("mobile.codex.async.prev")}
            </button>
          )}
          {live.length > 1 && currentIndex < live.length - 1 ? (
            <button
              type="button"
              data-testid="codex-async-next"
              disabled={!canGoNext}
              onClick={() => setIndex(currentIndex + 1)}
              className="rounded-full bg-[var(--btnp)] px-3 py-1 text-xs font-medium text-[var(--btnpt)] disabled:opacity-40"
            >
              {i18n.t("mobile.codex.async.next")}
            </button>
          ) : (
            <button
              type="button"
              data-testid="codex-async-submit"
              disabled={!canSubmit}
              onClick={() => void submit()}
              className="w-full rounded-full bg-[var(--btnp)] px-3 py-1.5 text-sm font-medium text-[var(--btnpt)] disabled:opacity-40"
            >
              {i18n.t("mobile.codex.async.submitAll", { count: live.length })}
            </button>
          )}
          {phase === "submitted" && (
            <span data-testid="codex-async-waiting" className="text-xs text-[var(--mut)]">
              {i18n.t("mobile.codex.async.waitingConfirm", {
                confirmed: confirmedCount,
                total: members.length,
              })}
            </span>
          )}
          {phase === "confirmed" && (
            <span data-testid="codex-async-done" className="text-xs text-emerald-600">
              {i18n.t("mobile.codex.async.groupDone")}
            </span>
          )}
        </footer>
      )}
      {(phase === "unknown" || phase === "submitted") && hasAnchor && (
        <div className="mt-2">
          <button
            type="button"
            data-testid="codex-async-verify"
            aria-busy={verifying}
            onClick={() => void reverify()}
            className="rounded-lg border border-[var(--bd)] px-3 py-1.5 text-xs"
          >
            {i18n.t("mobile.codex.async.verify")}
          </button>
        </div>
      )}
      {/* T04（spec §3.4）：没有回执/错误不留占位空行——note 只在确有内容时渲染。 */}
      {(() => {
        const note =
          error ??
          (phase === "unknown"
            ? timedOut
              ? i18n.t("mobile.codex.async.timedOut")
              : i18n.t("mobile.codex.async.unknown")
            : receipt?.delivery === "accepted"
              ? i18n.t("mobile.codex.async.submitted")
              : "");
        return note ? (
          <p data-testid="codex-async-note" className="mt-2 text-xs text-[var(--mut)]">
            {note}
          </p>
        ) : null;
      })()}
      {onArchive && (
        <button
          type="button"
          data-testid="codex-async-archive"
          onClick={onArchive}
          className="mt-1 text-[10px] text-[var(--mut)] underline"
        >
          {i18n.t("mobile.codex.async.archiveGroup")}
        </button>
      )}
      {/* T04：翻页/翻题说明只对多题组有意义——单题不显示多余翻页说明（spec §3.1）。 */}
      {live.length > 1 && (
        <p className="mt-2 text-[10px] text-[var(--mut)]">
          {i18n.t("mobile.codex.async.groupHint", { total: members.length })}
        </p>
      )}
    </InteractiveCard>
  );
}
