// ZCode 统一控制面板（T7.2 Slice B1a）：手机端最小发送面。
//
// 纪律（与 T7.2 计划 / useZcodeControl 对齐）：
// - 连接态只来自 hook 的官方 GET；未确认一律 `unknown`，已保存链接绝不当成已连接
//   （文案里也不出现「已连接 / 已配对」这类断言性措辞）。
// - 发送只走官方 tagged send（hook.submit → POST /m/api/v1/zcode/actions），
//   **不**回退旧 `session-send`、无头 CLI 或终端按键。
// - 回执如实：accepted=已受理（≠ 已生效），confirmed 才显示已生效，unknown 保持中性；
//   send_failed/rejected/unknown 保留草稿、不自动重发。就地原因只展示本地化文案，
//   `data-reason-code` 才是冻结错误码本身。
import { useEffect, useRef, useState } from "react";
import i18n from "@/i18n";
import {
  isZcodeErrorCode,
  type ActionReceipt,
  type CapabilityFlag,
  type CapabilityView,
  type ConnectionView,
  type ControlAction,
  type PendingView,
  type QuestionView,
  type SessionView,
} from "@/lib/api/zcodeControl";
import useZcodeControl from "./useZcodeControl";
import { fetchZcodeControl } from "./api";

interface ZcodeControlPanelProps {
  /** 会话原 ID（服务端按它绑定 ZCode 会话；手机不自报 workspace） */
  sessionId: string;
}

/** 冻结原因码 → 本地化文案键；表外码统一中性兜底（码本身只进 data-reason-code） */
const REASON_KEYS: Record<string, string> = {
  needs_config: "mobile.zcodeControl.reasonNeedsConfig",
  needs_auth: "mobile.zcodeControl.reasonNeedsAuth",
  offline: "mobile.zcodeControl.reasonOffline",
  conflict: "mobile.zcodeControl.reasonConflict",
  not_ready: "mobile.zcodeControl.reasonNotReady",
  unknown: "mobile.zcodeControl.reasonUnknown",
};

/** 回执状态 → 本地化文案键：accepted/confirmed/unknown 三种语义互不冒充 */
const STATUS_KEYS: Record<string, string> = {
  accepted: "mobile.zcodeControl.statusAccepted",
  confirmed: "mobile.zcodeControl.statusConfirmed",
  noop: "mobile.zcodeControl.statusNoop",
  rejected: "mobile.zcodeControl.statusRejected",
  send_failed: "mobile.zcodeControl.statusSendFailed",
  unknown: "mobile.zcodeControl.statusUnknown",
};

/**
 * 写面硬门：`connection.state === "paired"` **且** `session.ready === true` 才放行。
 * 返回 null = 可写；否则返回冻结原因码（陈旧投影/未配对连接一律零写）。
 * `connecting` 等配对前态不在冻结码表内 → 统一按 `not_ready` 呈现。
 */
function actionGateReason(
  connection: ConnectionView | null,
  session: SessionView | null
): string | null {
  if (connection === null) return "unknown";
  if (connection.state !== "paired") {
    if (isZcodeErrorCode(connection.reasonCode)) return connection.reasonCode;
    if (isZcodeErrorCode(connection.state)) return connection.state;
    return "not_ready";
  }
  if (session === null || !session.ready) return "not_ready";
  return null;
}

interface PendingCardProps {
  pending: PendingView;
  sessionId: string;
  generation: number;
  capabilities: CapabilityView;
  /** 写面硬门原因（paired + ready）；null = 门开。陈旧投影绝不可点。 */
  gateReason: string | null;
  submit: (action: ControlAction) => Promise<ActionReceipt>;
}

/** 单张 pending 卡：只读呈现 + 宿主原值回传；关闭的能力面禁用并说明原因。 */
function PendingCard({
  pending,
  sessionId,
  generation,
  capabilities,
  gateReason,
  submit,
}: PendingCardProps) {
  const [picked, setPicked] = useState<Record<string, string>>({});
  const [freeAnswers, setFreeAnswers] = useState<Record<string, string>>({});
  const snoozedRef = useRef(false);
  /** 同帧双击闸：一次用户动作只允许一个在途 POST（actionId 不重复生成）。 */
  const inFlightRef = useRef(false);
  /** 敏感作答的即时值：失焦清可见态，提交仍能带上本次输入（仅内存，不落存储）。 */
  const sensitiveRef = useRef<Record<string, string>>({});

  /** 一次动作一个在途槽：同帧重入直接丢弃，不产生第二个 actionId。 */
  const submitOnce = (action: ControlAction) => {
    if (inFlightRef.current) return;
    inFlightRef.current = true;
    void submit(action)
      .catch(() => {})
      .finally(() => {
        inFlightRef.current = false;
      });
  };

  const isPermission = pending.kind === "permission";
  const isUserInput = pending.kind === "userInput";
  const flagFor = (question: QuestionView): CapabilityFlag =>
    question.multiSelect ? capabilities.multiSelect : capabilities.singleSelect;

  // 卡片级只展示首个关闭原因：阅读面照常，作答面禁用。
  // 写面硬门优先于能力矩阵：连接未配对 / 投影 ready=false → 全套作答面零写。
  const capabilityBlocked: CapabilityFlag | null = isPermission
    ? capabilities.approvalReply.enabled
      ? null
      : capabilities.approvalReply
    : isUserInput
      ? (pending.questions
          .flatMap((question) => [
            flagFor(question),
            ...(question.freeTextAllowed ? [capabilities.freeText] : []),
          ])
          .find((flag) => !flag.enabled) ?? null)
      : { enabled: false, reasonCode: "unsupported_protocol" };
  const blocked: CapabilityFlag | null =
    gateReason !== null ? { enabled: false, reasonCode: gateReason } : capabilityBlocked;
  const blockedText =
    gateReason !== null
      ? i18n.t("mobile.zcodeControl.actionBlocked")
      : i18n.t("mobile.zcodeControl.pendingBlocked");

  /** auto_resolution：首次有效编辑补发一次 snooze（尽力而为，不阻塞渲染）。 */
  const snoozeOnce = () => {
    if (gateReason !== null) return; // 陈旧投影不发 snooze（写面硬门）
    if (!pending.autoResolution || snoozedRef.current) return;
    snoozedRef.current = true;
    submitOnce({
      kind: "snooze",
      sessionId,
      generation,
      interactionId: pending.interactionId,
    });
  };

  const submitAnswer = () => {
    if (blocked !== null) return;
    const answers = pending.questions
      .map((question) => {
        // 敏感作答失焦后可见态已清空，提交取本次挂载的内存值（不落任何存储）。
        const value = pending.sensitive
          ? (sensitiveRef.current[question.questionText] ?? freeAnswers[question.questionText])
          : freeAnswers[question.questionText];
        return {
          questionText: question.questionText,
          selectedValues: picked[question.questionText] ? [picked[question.questionText]] : [],
          freeText: value?.length ? value : null,
        };
      })
      .filter((answer) => answer.selectedValues.length > 0 || answer.freeText !== null);
    if (answers.length === 0) return;
    submitOnce({
      kind: "answer",
      sessionId,
      generation,
      interactionId: pending.interactionId,
      answers,
    });
  };

  // 仅 allowOnce/deny 可回传；fullAccess 及其他永久/未知 kind 不渲染。
  const permissionOptions = pending.options.filter(
    (option) => option.kind === "allowOnce" || option.kind === "deny"
  );
  let optionIndex = 0;

  return (
    <div
      data-testid="zcode-pending-card"
      data-interaction-id={pending.interactionId}
      className="mb-2 rounded-lg border border-[var(--cb)] px-3 py-2"
    >
      {pending.toolName !== null && <p className="text-xs text-[var(--mut)]">{pending.toolName}</p>}
      {pending.summary !== null && <p className="text-sm text-[var(--tx)]">{pending.summary}</p>}
      {isPermission &&
        permissionOptions.map((option) => (
          <button
            key={option.optionId}
            type="button"
            data-testid={`zcode-permission-option-${option.optionId}`}
            disabled={blocked !== null}
            onClick={() => {
              if (blocked !== null) return;
              submitOnce({
                kind: "permission",
                sessionId,
                generation,
                interactionId: pending.interactionId,
                optionId: option.optionId,
              });
            }}
            className="mt-1 mr-2 rounded-lg border border-[var(--cb)] px-3 py-1 text-sm text-[var(--tx)] disabled:opacity-40"
          >
            {option.label}
          </button>
        ))}
      {isUserInput &&
        pending.questions.map((question, questionIndex) => {
          const flag = flagFor(question);
          return (
            <div key={`${questionIndex}-${question.questionText}`} className="mt-1">
              <p className="text-sm text-[var(--tx)]">{question.header}</p>
              <p className="text-sm text-[var(--tx)]">{question.questionText}</p>
              <div className="mt-1 flex flex-wrap gap-1">
                {question.options.map((option) => {
                  const index = optionIndex++;
                  return (
                    <button
                      key={option.value}
                      type="button"
                      data-testid={`zcode-pending-option-${index}`}
                      disabled={blocked !== null || !flag.enabled}
                      onClick={() => {
                        if (blocked !== null || !flag.enabled) return;
                        setPicked((prev) => ({
                          ...prev,
                          [question.questionText]: option.value,
                        }));
                        snoozeOnce();
                      }}
                      className="rounded-lg border border-[var(--cb)] px-3 py-1 text-sm text-[var(--tx)] disabled:opacity-40"
                    >
                      {option.label}
                    </button>
                  );
                })}
              </div>
              {question.freeTextAllowed && (
                <input
                  type={pending.sensitive ? "password" : "text"}
                  data-testid={pending.sensitive ? "zcode-pending-sensitive-input" : undefined}
                  value={freeAnswers[question.questionText] ?? ""}
                  disabled={blocked !== null || !capabilities.freeText.enabled}
                  onChange={(event) => {
                    if (blocked !== null || !capabilities.freeText.enabled) return;
                    const value = event.target.value;
                    // 敏感作答只存本次挂载的组件状态，不落任何草稿缓存。
                    if (pending.sensitive) sensitiveRef.current[question.questionText] = value;
                    setFreeAnswers((prev) => ({ ...prev, [question.questionText]: value }));
                    snoozeOnce();
                  }}
                  onBlur={() => {
                    // 敏感输入失焦即清可见值（防肩窥/留屏）；提交值仍取本次内存副本。
                    if (!pending.sensitive) return;
                    setFreeAnswers((prev) => {
                      const next = { ...prev };
                      delete next[question.questionText];
                      return next;
                    });
                  }}
                  className="mt-1 w-full rounded-lg border border-[var(--cb)] px-3 py-1 text-sm text-[var(--tx)] disabled:opacity-40"
                />
              )}
            </div>
          );
        })}
      {isUserInput && (
        <button
          type="button"
          data-testid="zcode-pending-submit"
          disabled={blocked !== null}
          onClick={submitAnswer}
          className="mt-1 rounded-full bg-[var(--btnp)]/10 px-4 py-1 text-sm text-[var(--tx)] disabled:opacity-40"
        >
          {i18n.t("mobile.zcodeControl.submit")}
        </button>
      )}
      {blocked !== null && (
        <p
          data-testid="zcode-pending-reason"
          data-reason-code={blocked.reasonCode ?? "unknown"}
          className="mt-1 text-xs text-[var(--mut)]"
        >
          {blockedText}
        </p>
      )}
    </div>
  );
}

export default function ZcodeControlPanel({ sessionId }: ZcodeControlPanelProps) {
  const { connection, session, reason, submitting, draftCleared, submit } =
    useZcodeControl(sessionId);
  const [draft, setDraft] = useState("");
  const [receipt, setReceipt] = useState<ActionReceipt | null>(null);
  /** 已保存的官方链接（只读 GET /m/api/v1/official-control；读不到 → null = 不挂入口）。 */
  const [officialUrl, setOfficialUrl] = useState<string | null>(null);
  /** 同帧双击闸：一次用户动作只允许一个在途 POST（双击不产生第二个 actionId）。 */
  const inFlightRef = useRef(false);

  // 仅 send 的 accepted 允许清本次草稿（hook 语义）；失败/拒绝/未知一律保留。
  useEffect(() => {
    if (draftCleared) setDraft("");
  }, [draftCleared]);

  // 换会话清旧回执与旧草稿：上一次会话的受理状态不得贴到新会话。
  useEffect(() => {
    setReceipt(null);
    setDraft("");
  }, [sessionId]);

  // A5 基线「打开官方页面」的显式辅助入口：只读已保存链接（不自动打开、不代理点击）。
  useEffect(() => {
    let alive = true;
    setOfficialUrl(null);
    void fetchZcodeControl().then((availability) => {
      if (alive) setOfficialUrl(availability.url);
    });
    return () => {
      alive = false;
    };
  }, [sessionId]);

  const openOfficial = () => {
    if (officialUrl === null) return;
    window.open(officialUrl, "_blank", "noopener,noreferrer");
  };

  // 写面硬门：连接已配对 + 会话就绪（陈旧投影 ready=false 一律零写）。
  const gateReason = actionGateReason(connection, session);
  // 发送区原因优先取 hook 的如实连接/回执原因；连接正常但投影未就绪时显示 not_ready。
  const displayReason = reason ?? gateReason;

  /** 一次动作一个在途槽：同帧重入直接丢弃，回执只认第一次提交。 */
  const runOnce = (action: ControlAction) => {
    if (inFlightRef.current) return;
    inFlightRef.current = true;
    void submit(action)
      .then(setReceipt)
      .catch(() => {
        // 传输层失败：hook 已置冻结原因码；草稿保留，绝不自动重发。
      })
      .finally(() => {
        inFlightRef.current = false;
      });
  };

  // 硬门：连接已配对 + 会话就绪 + send 能力开启 + 无在途提交。空草稿不阻挡按钮
  // （按钮代表控制通道就绪），但点击时判空、绝不 POST 空动作。
  const canSend =
    gateReason === null && session !== null && session.capabilities.send.enabled && !submitting;

  const handleSend = () => {
    if (!canSend || session === null || draft.trim().length === 0) return;
    if (inFlightRef.current) return;
    runOnce({
      kind: "send",
      sessionId,
      generation: session.generation,
      text: draft, // 多行原样上行；trim 只用于判空
    });
  };

  // 打断：只认新鲜 control 里唯一的 primaryTurn（多目标拒绝，禁止随取第一个）；
  // 只发 stop，绝不夹带 sendText。
  const stopTargets = (session?.control?.activeWorks ?? []).filter(
    (work) => work.kind === "primaryTurn" && work.foregroundExecutionId !== null
  );
  const stopTarget = stopTargets.length === 1 ? stopTargets[0].foregroundExecutionId : null;
  const canInterrupt =
    gateReason === null &&
    session !== null &&
    session.capabilities.interrupt.enabled &&
    stopTarget !== null &&
    !submitting;
  // 不可打断的就地原因：写面硬门 > 能力关闭 > 多目标歧义。
  const interruptReason =
    gateReason ??
    (session?.capabilities.interrupt.enabled === false
      ? session.capabilities.interrupt.reasonCode
      : stopTarget === null
        ? "target_ambiguous"
        : null);

  const handleInterrupt = () => {
    if (!canInterrupt || session === null || stopTarget === null) return;
    runOnce({
      kind: "stop",
      sessionId,
      generation: session.generation,
      expectedForegroundExecutionId: stopTarget,
    });
  };

  // 模型/档位：目录只来自 session.selection（当前 provider）；写面硬门 + 能力两闸
  // 同开才可提交，任一关闭 → 两个控件都禁用并显示可见原因。
  const selection = session?.selection ?? null;
  const currentChoice =
    selection?.models.find((choice) => choice.modelId === selection.model) ?? null;
  const selectionEnabled =
    gateReason === null &&
    session?.capabilities.model.enabled === true &&
    session.capabilities.thoughtLevel.enabled === true;
  const selectionReason =
    gateReason ??
    session?.capabilities.model.reasonCode ??
    session?.capabilities.thoughtLevel.reasonCode ??
    "unknown";
  const currentThought =
    selection?.thought != null && currentChoice?.thoughtLevels.includes(selection.thought)
      ? selection.thought
      : (currentChoice?.thoughtLevels[0] ?? "");

  const submitSelection = (model: string, thought: string) => {
    if (session === null || !selectionEnabled || thought === "") return;
    runOnce({ kind: "selection", sessionId, generation: session.generation, model, thought });
  };

  const handleModelChange = (modelId: string) => {
    const choice = selection?.models.find((candidate) => candidate.modelId === modelId);
    if (choice === undefined) return;
    // 换模型时档位落在新模型的合法值里：保持旧档位，否则取首档。
    const thought = selection?.thought;
    const kept =
      thought != null && choice.thoughtLevels.includes(thought)
        ? thought
        : (choice.thoughtLevels[0] ?? "");
    submitSelection(modelId, kept);
  };

  const handleThoughtChange = (thought: string) => {
    if (selection === null || currentChoice?.thoughtLevels.includes(thought) !== true) return;
    submitSelection(selection.model, thought);
  };

  return (
    <div
      data-testid="zcode-control-panel"
      data-connection-state={connection?.state ?? "unknown"}
      className="shrink-0 border-t border-[var(--cb)] px-3 py-2"
    >
      {displayReason !== null && (
        <p
          data-testid="zcode-control-reason"
          data-reason-code={displayReason}
          className="mb-2 text-xs text-[var(--mut)]"
        >
          {i18n.t(REASON_KEYS[displayReason] ?? "mobile.zcodeControl.reasonUnknown")}
        </p>
      )}
      {/* A5 显式辅助入口：只打开**已保存**的官方链接（挂载不自动外跳、不代理点击）。 */}
      {officialUrl !== null && (
        <button
          type="button"
          data-testid="zcode-open-official"
          onClick={openOfficial}
          className="mb-2 rounded-full border border-[var(--cb)] px-3 py-1 text-xs text-[var(--mut)]"
        >
          {i18n.t("mobile.composer.zcodeOpenOfficial")}
        </button>
      )}
      {receipt !== null && (
        <p
          data-testid="zcode-action-status"
          data-action-state={receipt.state}
          className="mb-2 text-xs text-[var(--mut)]"
        >
          {i18n.t(STATUS_KEYS[receipt.state] ?? "mobile.zcodeControl.statusUnknown")}
        </p>
      )}
      {session !== null &&
        session.pending.map((pending) => (
          <PendingCard
            key={pending.interactionId}
            pending={pending}
            sessionId={sessionId}
            generation={session.generation}
            capabilities={session.capabilities}
            gateReason={gateReason}
            submit={submit}
          />
        ))}
      {selection !== null && (
        <div className="mb-2 flex flex-wrap items-center gap-2">
          <select
            data-testid="zcode-model-select"
            aria-label={i18n.t("mobile.zcodeControl.modelAria")}
            value={selection.model}
            disabled={!selectionEnabled}
            onChange={(e) => handleModelChange(e.target.value)}
            className="rounded-lg border border-[var(--cb)] px-2 py-1 text-sm text-[var(--tx)] disabled:opacity-40"
          >
            {selection.models.map((choice) => (
              <option key={choice.modelId} value={choice.modelId}>
                {choice.modelId}
              </option>
            ))}
          </select>
          <select
            data-testid="zcode-thought-select"
            aria-label={i18n.t("mobile.zcodeControl.thoughtAria")}
            value={currentThought}
            disabled={!selectionEnabled}
            onChange={(e) => handleThoughtChange(e.target.value)}
            className="rounded-lg border border-[var(--cb)] px-2 py-1 text-sm text-[var(--tx)] disabled:opacity-40"
          >
            {(currentChoice?.thoughtLevels ?? []).map((level) => (
              <option key={level} value={level}>
                {level}
              </option>
            ))}
          </select>
          {!selectionEnabled && (
            <p
              data-testid="zcode-selection-reason"
              data-reason-code={selectionReason}
              className="text-xs text-[var(--mut)]"
            >
              {gateReason !== null
                ? i18n.t("mobile.zcodeControl.actionBlocked")
                : i18n.t("mobile.zcodeControl.selectionBlocked")}
            </p>
          )}
        </div>
      )}
      {stopTargets.length > 0 && (
        <div className="mb-2 flex flex-wrap items-center gap-2">
          <button
            type="button"
            data-testid="zcode-interrupt"
            disabled={!canInterrupt}
            onClick={handleInterrupt}
            className="rounded-full border border-[var(--cb)] px-4 py-1 text-sm text-[var(--tx)] disabled:opacity-40"
          >
            {i18n.t("mobile.zcodeControl.interrupt")}
          </button>
          {!canInterrupt && interruptReason !== null && (
            <p
              data-testid="zcode-interrupt-reason"
              data-reason-code={interruptReason}
              className="text-xs text-[var(--mut)]"
            >
              {gateReason !== null
                ? i18n.t("mobile.zcodeControl.actionBlocked")
                : i18n.t("mobile.zcodeControl.interruptBlocked")}
            </p>
          )}
        </div>
      )}
      <div className="flex items-end gap-2">
        <textarea
          data-testid="zcode-draft"
          aria-label={i18n.t("mobile.zcodeControl.draftAria")}
          value={draft}
          rows={2}
          onChange={(e) => setDraft(e.target.value)}
          placeholder={i18n.t("mobile.zcodeControl.draftPlaceholder")}
          className="min-h-0 flex-1 resize-none rounded-lg border border-[var(--cb)] px-3 py-2 text-sm text-[var(--tx)] placeholder:text-[var(--mut)] focus:ring-2 focus:ring-[var(--btnp)] focus:outline-none"
        />
        <button
          type="button"
          data-testid="zcode-send"
          disabled={!canSend}
          onClick={() => void handleSend()}
          className="shrink-0 rounded-full bg-[var(--btnp)]/10 px-4 py-2 text-sm text-[var(--tx)] disabled:opacity-40"
        >
          {i18n.t(submitting ? "mobile.zcodeControl.sending" : "mobile.zcodeControl.send")}
        </button>
      </div>
    </div>
  );
}
