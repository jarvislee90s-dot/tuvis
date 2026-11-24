import { useEffect, useRef, useState, type ReactNode } from "react";
import { useAppTranslation } from "@/hooks/use-app-translation";
import { formatInvokeError } from "@/lib/invokeError";
import DshQuestionGroupCard from "./DshQuestionGroupCard";
import {
  ApiError,
  answerDshLiveQuestion,
  answerDshQuestionGroup,
  fetchDshLiveQuestions,
  fetchDshQuestions,
  type DshLiveQuestion,
  type DshQuestionAnswerItem,
  type DshQuestionSnapshot,
} from "./api";

// 挂在会话页上，切换布局时保留快照、草稿和提交状态。
export function useDshSessionQuestions(
  instanceId: string | undefined,
  sessionId: string,
  refreshTick: number,
  transcriptQuestionSeq: number | null
) {
  const { t } = useAppTranslation();
  const transcriptWaiting = transcriptQuestionSeq !== null;
  const [liveReady, setLiveReady] = useState(false);
  const [livePending, setLivePending] = useState<DshLiveQuestion[]>([]);
  const [continued, setContinued] = useState<DshQuestionSnapshot | null>(null);
  const [refreshAfterSubmit, setRefreshAfterSubmit] = useState(0);
  const [submitting, setSubmitting] = useState(false);
  const [feedback, setFeedback] = useState<string | null>(null);
  const [waitingReceipt, setWaitingReceipt] = useState<{
    id: string;
    live: boolean;
    transcriptSeq: number | null;
  } | null>(null);
  const [receiptEvent, setReceiptEvent] = useState<string | null>(null);
  const pendingGeneration = useRef<number | undefined>(undefined);
  const firstEmptyGeneration = useRef<number | undefined>(undefined);
  const conflictEvent = useRef<string | null>(null);
  const submissionInFlight = useRef(false);
  const continuedActions = useRef(new Map<string, string>());
  const drafts = useRef(new Map<string, unknown>());

  useEffect(() => {
    if (!instanceId) return;
    const controller = new AbortController();
    const { signal } = controller;
    void fetchDshLiveQuestions(instanceId, sessionId, signal)
      .then(({ live, pending, generation }) => {
        if (signal.aborted) return;
        // ready 帧早于未决事件重放。跨代空表且当前转写仍待答时，保留旧卡并禁用提交。
        const replayPending =
          live &&
          pending.length === 0 &&
          transcriptWaiting &&
          pendingGeneration.current !== undefined &&
          pendingGeneration.current !== generation &&
          firstEmptyGeneration.current !== generation;
        firstEmptyGeneration.current = replayPending ? generation : undefined;
        setLiveReady(live && !replayPending);
        if (!live || replayPending) return;
        pendingGeneration.current = pending.length > 0 ? generation : undefined;
        setLivePending(pending);
        const conflict = conflictEvent.current;
        if (conflict && !pending.some((item) => item.eventId === conflict)) {
          conflictEvent.current = null;
          setFeedback(t("mobile.dshQuestions.alreadySettled"));
        }
      })
      .catch(() => {
        if (!signal.aborted) setLiveReady(false);
      });
    void fetchDshQuestions(instanceId, sessionId, signal)
      .then((snapshot) => {
        if (!signal.aborted && snapshot.verified) setContinued(snapshot);
      })
      .catch(() => {});
    return () => controller.abort();
  }, [instanceId, sessionId, refreshTick, refreshAfterSubmit, transcriptWaiting, t]);

  const liveQuestion = livePending[0];
  const receiptAccepted = liveQuestion !== undefined && receiptEvent === liveQuestion.eventId;
  const continuedGroup = continued?.groups?.[0];
  const group = liveQuestion
    ? { callId: liveQuestion.eventId, questions: liveQuestion.questions, state: "open" }
    : continuedGroup;
  const consoleOnly = group?.questions.some((question) => question.intent != null) ?? false;
  const draftKey = JSON.stringify([
    instanceId,
    sessionId,
    liveQuestion ? "live" : "continued",
    group?.callId,
    liveQuestion ? null : continued?.revisionToken,
  ]);

  const waitingInTranscript =
    waitingReceipt !== null &&
    (waitingReceipt.transcriptSeq !== null
      ? waitingReceipt.transcriptSeq === transcriptQuestionSeq
      : waitingReceipt.live
        ? livePending.some((question) => question.eventId === waitingReceipt.id)
        : continued?.groups?.some((question) => question.callId === waitingReceipt.id));

  async function submit(answers: DshQuestionAnswerItem[]) {
    if (!instanceId || !group || consoleOnly || submissionInFlight.current) return;
    if (liveQuestion && (!liveReady || receiptAccepted)) return;
    submissionInFlight.current = true;
    setSubmitting(true);
    setFeedback(null);
    setWaitingReceipt(null);
    try {
      if (liveQuestion) {
        await answerDshLiveQuestion({
          instanceId,
          sessionId,
          eventId: liveQuestion.eventId,
          answers,
        });
        setReceiptEvent(liveQuestion.eventId);
        setWaitingReceipt({
          id: liveQuestion.eventId,
          live: true,
          transcriptSeq: transcriptQuestionSeq,
        });
      } else if (continued?.revisionToken) {
        const actionId =
          continuedActions.current.get(draftKey) ??
          `act-${Date.now().toString(36)}-${Math.random().toString(36).slice(2, 10)}`;
        continuedActions.current.set(draftKey, actionId);
        const result = await answerDshQuestionGroup({
          instanceId,
          sessionId,
          groupId: group.callId,
          revisionToken: continued.revisionToken,
          actionId,
          answers,
        });
        const state = result.action?.receipt.state;
        const accepted = result.hostAccepted || state === "accepted" || state === "succeeded";
        if (state === "failed" || state === "cancelled" || state === "rejected") {
          continuedActions.current.delete(draftKey);
        }
        if (state === "unknown_delivery") {
          setFeedback(t("mobile.dshQuestions.deliveryUnknown"));
        } else if (!accepted) {
          setFeedback(t("mobile.dshQuestions.hostRejected"));
        } else if (state !== "succeeded") {
          setWaitingReceipt({
            id: group.callId,
            live: false,
            transcriptSeq: transcriptQuestionSeq,
          });
        }
      }
    } catch (error) {
      if (
        liveQuestion &&
        error instanceof ApiError &&
        error.status === 409 &&
        error.data?.error === "DSH_ACTION_STATE_CONFLICT"
      ) {
        conflictEvent.current = liveQuestion.eventId;
        setFeedback(t("mobile.dshQuestions.checkingStatus"));
      } else if (
        liveQuestion &&
        error instanceof ApiError &&
        error.data?.error === "DSH_WRITE_TIMEOUT"
      ) {
        setFeedback(t("mobile.dshQuestions.deliveryUnknown"));
      } else {
        setFeedback(formatInvokeError(error, t));
      }
    } finally {
      submissionInFlight.current = false;
      setSubmitting(false);
      setRefreshAfterSubmit((tick) => tick + 1);
    }
  }

  function renderQuestions(fallback: ReactNode, sessionRunning: boolean) {
    if (!instanceId) return fallback;
    const status =
      feedback ??
      (sessionRunning && waitingInTranscript ? t("mobile.dshQuestions.accepted") : null);
    return (
      <>
        {!liveReady && sessionRunning && (
          <p
            role="status"
            data-testid="dsh-question-connecting"
            className="px-3 text-xs text-[var(--mut)]"
          >
            {t("mobile.dshQuestions.connecting")}
          </p>
        )}
        {status && (
          <p role="status" className="px-3 text-xs text-[var(--mut)]">
            {status}
          </p>
        )}
        {group ? (
          <DshQuestionGroupCard
            key={draftKey}
            group={group}
            draftKey={draftKey}
            live={liveQuestion !== undefined}
            readOnly={consoleOnly}
            submitting={submitting}
            disabled={liveQuestion !== undefined && (!liveReady || receiptAccepted)}
            onSubmit={(answers) => void submit(answers)}
            loadDraft={(key) => drafts.current.get(key) as never}
            saveDraft={(key, draft) => drafts.current.set(key, draft)}
          />
        ) : (
          fallback
        )}
      </>
    );
  }
  return { renderQuestions, groupKey: group ? draftKey : null };
}
