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
import { useEffect, useRef, useState, type ClipboardEvent as ReactClipboardEvent } from "react";
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
  type ZcodeErrorCode,
  ZcodeControlApiError,
} from "@/lib/api/zcodeControl";
import useZcodeControl from "./useZcodeControl";
import {
  attachmentMarkup,
  isImageFile,
  newAttachmentId,
  type PendingAttachment,
} from "./attachments";
import { ApiError, fetchZcodeControl, postZcodeTakeover, uploadAttachment } from "./api";

interface ZcodeControlPanelProps {
  /** 会话原 ID（服务端按它绑定 ZCode 会话；手机不自报 workspace） */
  sessionId: string;
}

/**
 * 冻结原因码 → 本地化文案键（21 码全覆盖；`Record<ZcodeErrorCode, string>` 保证漏码
 * 在编译期报错）。码本身只进 `data-reason-code`，用户只看到这里的本地化文案。
 */
export const ZCODE_REASON_KEYS: Record<ZcodeErrorCode, string> = {
  needs_config: "mobile.zcodeControl.reasonNeedsConfig",
  needs_auth: "mobile.zcodeControl.reasonNeedsAuth",
  offline: "mobile.zcodeControl.reasonOffline",
  conflict: "mobile.zcodeControl.reasonConflict",
  unknown: "mobile.zcodeControl.reasonUnknown",
  not_ready: "mobile.zcodeControl.reasonNotReady",
  unsupported_protocol: "mobile.zcodeControl.reasonUnsupportedProtocol",
  // 账本/静态知识判死的**永久**不支持（Rust codes::UNSUPPORTED）：给专文案，不落中性兜底。
  unsupported: "mobile.zcodeControl.reasonUnsupported",
  // 协议身份不一致（命令 clientId ≠ 握手身份）：确定性客户端缺陷，提示升级/反馈，不重试。
  client_mismatch: "mobile.zcodeControl.reasonClientMismatch",
  needs_evidence: "mobile.zcodeControl.reasonNeedsEvidence",
  no_session: "mobile.zcodeControl.reasonNoSession",
  target_ambiguous: "mobile.zcodeControl.reasonTargetAmbiguous",
  stale_generation: "mobile.zcodeControl.reasonStaleGeneration",
  stale_pending: "mobile.zcodeControl.reasonStalePending",
  invalid_action: "mobile.zcodeControl.reasonInvalidAction",
  invalid_answer: "mobile.zcodeControl.reasonInvalidAnswer",
  forbidden_option: "mobile.zcodeControl.reasonForbiddenOption",
  action_payload_conflict: "mobile.zcodeControl.reasonActionPayloadConflict",
  storage_failed: "mobile.zcodeControl.reasonStorageFailed",
  payload_too_large: "mobile.zcodeControl.reasonPayloadTooLarge",
  stop_target_changed: "mobile.zcodeControl.reasonStopTargetChanged",
};

/** 原因码 → 当前语言的用户文案；词表外的码一律中性兜底（绝不回显码原文）。 */
export function zcodeReasonText(code: string): string {
  const key = isZcodeErrorCode(code)
    ? ZCODE_REASON_KEYS[code]
    : "mobile.zcodeControl.reasonUnknown";
  return i18n.t(key);
}

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
  /** 全局动作在途（hook.submitting）：在途期间作答/审批面一并禁用。 */
  submitting: boolean;
  /** 宿主会话投影修订号：变化 = 宿主已推进，受理锁定随新投影重开。 */
  revision: number | null;
  submit: (action: ControlAction) => Promise<ActionReceipt>;
}

/** 单张 pending 卡：只读呈现 + 宿主原值回传；关闭的能力面禁用并说明原因。 */
function PendingCard({
  pending,
  sessionId,
  generation,
  capabilities,
  gateReason,
  submitting,
  revision,
  submit,
}: PendingCardProps) {
  /** 每题的宿主选项原值集合：单选恒 0/1 个，多选按点击顺序累加（toggle 增删）。 */
  const [picked, setPicked] = useState<Record<string, string[]>>({});
  const [freeAnswers, setFreeAnswers] = useState<Record<string, string>>({});
  const snoozedRef = useRef(false);
  /** 同帧双击闸：一次用户动作只允许一个在途 POST（actionId 不重复生成）。 */
  const inFlightRef = useRef(false);
  /** 敏感作答的即时值：失焦清可见态，提交仍能带上本次输入（仅内存，不落存储）。 */
  const sensitiveRef = useRef<Record<string, string>>({});
  /**
   * 本卡已受理/已生效的锁定：在宿主 pending/session 推进（revision 变化）前，
   * 作答/审批面一律不可再点——同一份答案绝不换新 actionId 重发。
   */
  const [settledState, setSettledState] = useState<"accepted" | "confirmed" | null>(null);
  const settledRef = useRef(false);
  /** snooze POST 在途：提交禁用并显示「正在暂停计时」（不静默吞掉用户点击）。 */
  const [snoozeInFlight, setSnoozeInFlight] = useState(false);

  // 宿主投影推进才解锁受理态；初始挂载（settled 为空）是幂等的。
  useEffect(() => {
    settledRef.current = false;
    setSettledState(null);
  }, [revision]);

  /** 一次动作一个在途槽：同帧重入/受理后重入一律丢弃，不产生第二个 actionId。 */
  const submitOnce = (action: ControlAction, onSettled?: () => void): boolean => {
    if (inFlightRef.current || settledRef.current) return false;
    inFlightRef.current = true;
    void submit(action)
      .then((receipt) => {
        // snooze 只暂停计时，不锁卡；作答/审批的受理/生效才进入锁定态。
        if (action.kind === "snooze") return;
        if (receipt.state === "accepted" || receipt.state === "confirmed") {
          settledRef.current = true;
          setSettledState(receipt.state);
        }
      })
      .catch(() => {})
      .finally(() => {
        inFlightRef.current = false;
        onSettled?.();
      });
    return true;
  };

  const isPermission = pending.kind === "permission";
  const isUserInput = pending.kind === "userInput";
  const flagFor = (question: QuestionView): CapabilityFlag =>
    question.multiSelect ? capabilities.multiSelect : capabilities.singleSelect;
  // 仅 allowOnce/deny 可回传；fullAccess 及其他永久/未知 kind 不渲染。
  const permissionOptions = pending.options.filter(
    (option) => option.kind === "allowOnce" || option.kind === "deny"
  );

  // 卡片级只展示首个关闭原因：阅读面照常，作答面禁用。
  // 写面硬门优先于能力矩阵：连接未配对 / 投影 ready=false → 全套作答面零写。
  // 审批卡过滤后无可选 option（宿主只给 fullAccess/未知 kind）→ 一样要就地说明，
  // 否则卡片只剩摘要、无按钮也无原因（用户只会以为卡坏了）。
  const capabilityBlocked: CapabilityFlag | null = isPermission
    ? capabilities.approvalReply.enabled
      ? permissionOptions.length === 0
        ? { enabled: false, reasonCode: "unsupported_protocol" }
        : null
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
  // 按码如实呈现：每个冻结原因码都有独立 zh/en 文案（表外码才用中性兜底）；
  // 仅当服务端未给原因码时退回「该作答形态暂未开放」的上下文说明。
  const blockedText =
    blocked?.reasonCode != null
      ? zcodeReasonText(blocked.reasonCode)
      : i18n.t("mobile.zcodeControl.pendingBlocked");

  /** auto_resolution：首次有效编辑补发一次 snooze（尽力而为，不阻塞渲染）。 */
  const snoozeOnce = () => {
    if (gateReason !== null) return; // 陈旧投影不发 snooze（写面硬门）
    if (!pending.autoResolution || snoozedRef.current) return;
    if (inFlightRef.current || settledRef.current) return;
    snoozedRef.current = true;
    setSnoozeInFlight(true);
    // 与最终提交共用卡片级在途槽；结束（无论成败）复位可见提示。
    const started = submitOnce(
      {
        kind: "snooze",
        sessionId,
        generation,
        interactionId: pending.interactionId,
      },
      () => setSnoozeInFlight(false)
    );
    if (!started) setSnoozeInFlight(false);
  };

  const submitAnswer = () => {
    if (blocked !== null || submitting || settledRef.current || inFlightRef.current) return;
    const answers = pending.questions
      .map((question) => {
        // 敏感作答失焦后可见态已清空，提交取本次挂载的内存值（不落任何存储）。
        const value = pending.sensitive
          ? (sensitiveRef.current[question.questionText] ?? freeAnswers[question.questionText])
          : freeAnswers[question.questionText];
        const text = value?.length ? value : null;
        // 兜底：一次作答只允许一类——有文本就丢弃选中项，否则 freeText 恒 null。
        // （交互层已互斥；这里防历史状态/异常路径把两类混着上送给宿主。）
        return {
          questionText: question.questionText,
          selectedValues: text !== null ? [] : (picked[question.questionText] ?? []),
          freeText: text,
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

  // 仅 allowOnce/deny 可回传（`permissionOptions` 已在能力闸处算好）；
  // fullAccess 及其他永久/未知 kind 不渲染。
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
            disabled={blocked !== null || submitting || settledState !== null}
            onClick={() => {
              if (blocked !== null || submitting || settledRef.current) return;
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
                  const selected = picked[question.questionText]?.includes(option.value) === true;
                  return (
                    <button
                      key={option.value}
                      type="button"
                      data-testid={`zcode-pending-option-${index}`}
                      aria-pressed={selected}
                      disabled={
                        blocked !== null || submitting || settledState !== null || !flag.enabled
                      }
                      onClick={() => {
                        if (blocked !== null || submitting || settledRef.current || !flag.enabled) {
                          return;
                        }
                        setPicked((prev) => {
                          const current = prev[question.questionText] ?? [];
                          // 多选 = toggle（增删，保留点击顺序）；单选 = 替换为本次点击项。
                          const next = question.multiSelect
                            ? current.includes(option.value)
                              ? current.filter((value) => value !== option.value)
                              : [...current, option.value]
                            : [option.value];
                          return { ...prev, [question.questionText]: next };
                        });
                        // 互斥：选项生效即清同题自由作答（敏感内存值一并删），
                        // 否则提交会同时带 selectedValues 与 freeText（宿主要求二选一）。
                        setFreeAnswers((prev) => {
                          if (!(question.questionText in prev)) return prev;
                          const next = { ...prev };
                          delete next[question.questionText];
                          return next;
                        });
                        delete sensitiveRef.current[question.questionText];
                        snoozeOnce();
                      }}
                      className={`rounded-lg border px-3 py-1 text-sm text-[var(--tx)] disabled:opacity-40 ${
                        selected ? "border-[var(--btnp)] bg-[var(--btnp)]/10" : "border-[var(--cb)]"
                      }`}
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
                  disabled={
                    blocked !== null || !capabilities.freeText.enabled || settledState !== null
                  }
                  onChange={(event) => {
                    if (blocked !== null || !capabilities.freeText.enabled) return;
                    const value = event.target.value;
                    // 敏感作答只存本次挂载的组件状态，不落任何草稿缓存。
                    if (pending.sensitive) {
                      if (value.length > 0) sensitiveRef.current[question.questionText] = value;
                      else delete sensitiveRef.current[question.questionText];
                    }
                    setFreeAnswers((prev) => ({ ...prev, [question.questionText]: value }));
                    // 互斥：一开始自由作答，同题已选中的选项即失效（只能提交一类）。
                    setPicked((prev) => {
                      if (!(question.questionText in prev)) return prev;
                      const next = { ...prev };
                      delete next[question.questionText];
                      return next;
                    });
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
          disabled={blocked !== null || submitting || settledState !== null}
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
      {blocked === null && settledState !== null && (
        <p
          data-testid="zcode-pending-notice"
          data-state={settledState}
          className="mt-1 text-xs text-[var(--mut)]"
        >
          {i18n.t(STATUS_KEYS[settledState])}
        </p>
      )}
      {blocked === null && settledState === null && snoozeInFlight && (
        <p
          data-testid="zcode-pending-notice"
          data-state="snoozing"
          className="mt-1 text-xs text-[var(--mut)]"
        >
          {i18n.t("mobile.zcodeControl.pendingSnoozing")}
        </p>
      )}
    </div>
  );
}

export default function ZcodeControlPanel({ sessionId }: ZcodeControlPanelProps) {
  const { connection, session, reason, submitting, draftCleared, submit, refresh } =
    useZcodeControl(sessionId);
  const [draft, setDraft] = useState("");
  const [receipt, setReceipt] = useState<ActionReceipt | null>(null);
  /** 已保存的官方链接（只读 GET /m/api/v1/official-control；读不到 → null = 不挂入口）。 */
  const [officialUrl, setOfficialUrl] = useState<string | null>(null);
  /** 同帧双击闸：一次用户动作只允许一个在途 POST（双击不产生第二个 actionId）。 */
  const inFlightRef = useRef(false);
  /** 最近一次 send 提交的原文：accepted 迟到时只清这份，不碰回执到达前续写的草稿。 */
  const submittedDraftRef = useRef<string | null>(null);
  /** 草稿即时值（effect 之外也要读最新值，避免闭包拿到旧草稿）。 */
  const draftRef = useRef(draft);
  // 三轮 C：附件态（与 MessageComposer 同一上传管线/条目形状/引用约定）。
  const [attachments, setAttachments] = useState<PendingAttachment[]>([]);
  // 渲染期写 ref 被 react-hooks 规则禁止：同步镜像改在 effect（发送读 refs 语义不变）。
  const attachmentsRef = useRef<PendingAttachment[]>([]);
  useEffect(() => {
    attachmentsRef.current = attachments;
  }, [attachments]);
  /** 上传中条目的取消柄（× 移除 = XHR abort，防白传到底）。 */
  const uploadCtrlsRef = useRef(new Map<string, AbortController>());
  /** 本次 send 提交的就绪附件 id：accepted 清的是「随本次发送」的条目，不碰续加。 */
  const submittedAttachmentIdsRef = useRef<string[] | null>(null);
  /** 附件闸提示（上传中/存在失败项时点击发送）：瞬态，随下次发送动作清除。 */
  const [attachNotice, setAttachNotice] = useState<"uploading" | "failed" | null>(null);
  const attachInputRef = useRef<HTMLInputElement | null>(null);

  // 仅 send 的 accepted 允许清本次草稿（hook 语义）；失败/拒绝/未知一律保留。
  // 迟到回执竞态：accepted 到达时若草稿已被续写成别的内容，那是**新**输入，必须保留——
  // 只有草稿仍等于本次提交原文才清空（连点两次发送的场景由 hook 复位标记兜底）。
  useEffect(() => {
    if (!draftCleared) return;
    const submitted = submittedDraftRef.current;
    // 竞态守卫先取比较结果（草稿被续写 → 那是新输入，草稿与附件都保留）。
    const unchanged = submitted !== null && draftRef.current === submitted;
    if (unchanged) {
      draftRef.current = "";
      setDraft("");
    }
    // 三轮 C：send accepted 时只清「随本次发送」的附件条目；草稿被续写时附件同样
    // 保留——它们属于未确认发出的那份内容。
    const sentIds = submittedAttachmentIdsRef.current;
    if (unchanged && sentIds !== null) {
      submittedAttachmentIdsRef.current = null;
      setAttachments((prev) => prev.filter((a) => !sentIds.includes(a.id)));
    }
  }, [draftCleared]);

  // 换会话清旧回执与旧草稿：上一次会话的受理状态不得贴到新会话。
  useEffect(() => {
    setReceipt(null);
    setDraft("");
    draftRef.current = "";
    submittedDraftRef.current = null;
    // 三轮 C：附件与在途上传同样不跨会话残留（取消在途 XHR，防止串盘）。
    for (const ctrl of uploadCtrlsRef.current.values()) ctrl.abort();
    uploadCtrlsRef.current.clear();
    setAttachments([]);
    submittedAttachmentIdsRef.current = null;
    setAttachNotice(null);
    // 抢回面同样不跨会话残留（T8）。
    setTakeoverBusy(false);
    setTakeoverStillConflict(false);
    setTakeoverError(null);
    takeoverInFlightRef.current = false;
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

  // ==== T8 冲突态「断开并抢回」 ====
  /** 抢回 POST 在途（busy 态 + 同帧双击闸）。 */
  const [takeoverBusy, setTakeoverBusy] = useState(false);
  /** 服务端一次抢回后仍 conflict：只提示先关官方页面，绝不二次请求。 */
  const [takeoverStillConflict, setTakeoverStillConflict] = useState(false);
  /** 抢回失败的冻结原因码（文案按码本地化，码只进 data-reason-code）。 */
  const [takeoverError, setTakeoverError] = useState<ZcodeErrorCode | "unknown" | null>(null);
  const takeoverInFlightRef = useRef(false);

  /** 一次抢回一个在途槽：busy 期间重入一律丢弃（绝不产生第二次 POST）。 */
  const handleTakeover = () => {
    if (takeoverInFlightRef.current) return;
    takeoverInFlightRef.current = true;
    setTakeoverBusy(true);
    setTakeoverStillConflict(false);
    setTakeoverError(null);
    void postZcodeTakeover()
      .then((view) => {
        // 仍 conflict（官方页还在线）：服务端已按契约只试一次，这里只提示。
        setTakeoverStillConflict(view.state === "conflict");
        // 成功（含仍 conflict）后重拉权威连接/会话视图（现有 GET 纠偏机制）。
        void refresh();
      })
      .catch((error: unknown) => {
        // 失败显示冻结码原因：网络层（status=null）按 offline 口径，其余按服务端码。
        setTakeoverError(
          error instanceof ZcodeControlApiError
            ? (error.code ?? (error.status === null ? "offline" : "unknown"))
            : "unknown"
        );
      })
      .finally(() => {
        takeoverInFlightRef.current = false;
        setTakeoverBusy(false);
      });
  };

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
    // 新提交开始即清旧回执（C2）：上一次的「已受理」绝不得贴到在途/失败/未知的
    // 新动作上——accepted 越级正是本面板要防的失真。
    setReceipt(null);
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
  // 能力关闭就地说明（I1）：paired + ready 但 send 能力关时，禁用按钮之外必须有一句
  // 可见原因（服务端 reasonCode），否则用户只会以为按钮坏了。
  const sendCapabilityReason =
    gateReason === null && session !== null && !session.capabilities.send.enabled
      ? (session.capabilities.send.reasonCode ?? "not_ready")
      : null;

  // 三轮 C：附件上传（通用通道；状态机与 MessageComposer 同形，无速率估算）。
  const addZcodeFiles = (files: File[]) => {
    for (const file of files) {
      const id = newAttachmentId();
      const isImage = isImageFile(file);
      setAttachments((prev) => [
        ...prev,
        { id, name: file.name || (isImage ? file.name : "file"), isImage, status: "uploading" },
      ]);
      const ctrl = new AbortController();
      uploadCtrlsRef.current.set(id, ctrl);
      void uploadAttachment(sessionId, file, ctrl.signal, (loaded, total) => {
        setAttachments((prev) =>
          prev.map((a) => (a.id === id ? { ...a, loaded, total: total > 0 ? total : null } : a)),
        );
      })
        .then((res) => {
          if (res === null) throw new ApiError(403, i18n.t("mobile.zcodeControl.deviceInvalid"));
          setAttachments((prev) =>
            prev.map((a) => (a.id === id ? { ...a, status: "ready", path: res.path } : a)),
          );
        })
        .catch((e: unknown) => {
          if (ctrl.signal.aborted) {
            // 用户取消：chip 已随 removeZcodeAttachment 移除，静默收尾。
            setAttachments((prev) => prev.filter((a) => a.id !== id));
            return;
          }
          const reason =
            e instanceof ApiError
              ? e.message
              : e instanceof Error
                ? e.message
                : String(e);
          setAttachments((prev) =>
            prev.map((a) => (a.id === id ? { ...a, status: "failed", error: reason } : a)),
          );
        })
        .finally(() => {
          uploadCtrlsRef.current.delete(id);
        });
    }
  };

  const removeZcodeAttachment = (id: string) => {
    // 上传中移除 = 取消（XHR abort），与 MessageComposer 同语义。
    uploadCtrlsRef.current.get(id)?.abort();
    uploadCtrlsRef.current.delete(id);
    setAttachments((prev) => prev.filter((a) => a.id !== id));
  };

  /** 粘贴图片走同一上传链路；非图片放行默认文本行为。 */
  const handleZcodePaste = (e: ReactClipboardEvent<HTMLTextAreaElement>) => {
    const images = Array.from(e.clipboardData?.files ?? []).filter((f) => isImageFile(f));
    if (images.length === 0) return;
    e.preventDefault();
    addZcodeFiles(images);
  };

  const handleSend = () => {
    if (!canSend || session === null || inFlightRef.current) return;
    const current = attachmentsRef.current;
    // 附件闸：上传中或存在失败项时不静默丢附件发送（三轮 C 契约）。
    if (current.some((a) => a.status === "uploading")) {
      setAttachNotice("uploading");
      return;
    }
    if (current.some((a) => a.status === "failed")) {
      setAttachNotice("failed");
      return;
    }
    const markup = attachmentMarkup(current);
    const text = draft.trim();
    if (text.length === 0 && markup.length === 0) return;
    // 记下本次提交原文与随行附件：accepted 清「同一份」，失败/未知全保留。
    submittedDraftRef.current = draft;
    submittedAttachmentIdsRef.current = current
      .filter((a) => a.status === "ready")
      .map((a) => a.id);
    setAttachNotice(null);
    runOnce({
      kind: "send",
      sessionId,
      generation: session.generation,
      text: markup ? (text ? `${draft}\n${markup}` : markup) : draft, // 多行原样上行
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
          {zcodeReasonText(displayReason)}
        </p>
      )}
      {/* T8 冲突态「断开并抢回」：只在 conflict 出现。确认文案常驻按钮旁（点击即
          执行一次），busy 禁用；仍 conflict 只提示先关官方页面，失败显示冻结码原因。 */}
      {connection?.state === "conflict" && (
        <div className="mb-2">
          <button
            type="button"
            data-testid="zcode-takeover"
            disabled={takeoverBusy}
            onClick={handleTakeover}
            className="rounded-full border border-[var(--cb)] px-4 py-1 text-sm text-[var(--tx)] disabled:opacity-40"
          >
            {i18n.t(
              takeoverBusy ? "mobile.zcodeControl.takeoverBusy" : "mobile.zcodeControl.takeover"
            )}
          </button>
          <p data-testid="zcode-takeover-hint" className="mt-1 text-xs text-[var(--mut)]">
            {i18n.t("mobile.zcodeControl.takeoverHint")}
          </p>
          {takeoverStillConflict && (
            <p
              data-testid="zcode-takeover-still-conflict"
              className="mt-1 text-xs text-[var(--mut)]"
            >
              {i18n.t("mobile.zcodeControl.takeoverStillConflict")}
            </p>
          )}
          {takeoverError !== null && (
            <p
              data-testid="zcode-takeover-error"
              data-reason-code={takeoverError}
              className="mt-1 text-xs text-[var(--mut)]"
            >
              {zcodeReasonText(takeoverError)}
            </p>
          )}
        </div>
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
            submitting={submitting}
            revision={session.revision}
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
              {zcodeReasonText(selectionReason)}
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
              {zcodeReasonText(interruptReason)}
            </p>
          )}
        </div>
      )}
      {attachments.length > 0 && (
        <div className="flex flex-wrap gap-2">
          {attachments.map((a) => (
            <span
              key={a.id}
              data-testid="zcode-attachment"
              data-status={a.status}
              title={a.error ?? a.path ?? undefined}
              className="max-w-full truncate rounded-md border border-[var(--cb)] px-2 py-1 text-xs text-[var(--tx)]"
            >
              {a.isImage ? "🖼" : "📎"} {a.name}
              {a.status === "uploading" && (
                <span className="ml-1 text-[var(--mut)]">
                  {typeof a.loaded === "number"
                    ? ` ${a.loaded}${a.total ? `/${a.total}` : ""}B`
                    : ""}
                </span>
              )}
              {a.status === "failed" && a.error && (
                <span className="ml-1 text-red-500">{a.error}</span>
              )}
              <button
                type="button"
                data-testid="zcode-attachment-remove"
                aria-label={i18n.t("mobile.zcodeControl.removeAttachment")}
                onClick={() => removeZcodeAttachment(a.id)}
                className="ml-1 text-[var(--mut)]"
              >
                ×
              </button>
            </span>
          ))}
        </div>
      )}
      {attachNotice !== null && (
        <p data-testid="zcode-attach-notice" className="text-xs text-amber-500">
          {i18n.t(
            attachNotice === "uploading"
              ? "mobile.zcodeControl.attachBlockedUploading"
              : "mobile.zcodeControl.attachBlockedFailed",
          )}
        </p>
      )}
      <div className="flex items-end gap-2">
        <button
          type="button"
          data-testid="zcode-attach"
          aria-label={i18n.t("mobile.zcodeControl.attachAria")}
          onClick={() => attachInputRef.current?.click()}
          className="shrink-0 rounded-full bg-[var(--btnp)]/10 px-3 py-2 text-sm text-[var(--tx)]"
        >
          +
        </button>
        <input
          ref={attachInputRef}
          data-testid="zcode-attachment-input"
          type="file"
          multiple
          className="hidden"
          onChange={(e) => {
            const files = Array.from(e.target.files ?? []);
            e.target.value = "";
            if (files.length > 0) addZcodeFiles(files);
          }}
        />
        <textarea
          data-testid="zcode-draft"
          aria-label={i18n.t("mobile.zcodeControl.draftAria")}
          value={draft}
          rows={2}
          onChange={(e) => {
            draftRef.current = e.target.value;
            setDraft(e.target.value);
          }}
          onPaste={handleZcodePaste}
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
      {sendCapabilityReason !== null && (
        <p
          data-testid="zcode-send-reason"
          data-reason-code={sendCapabilityReason}
          className="mt-1 text-xs text-[var(--mut)]"
        >
          {zcodeReasonText(sendCapabilityReason)}
        </p>
      )}
    </div>
  );
}
