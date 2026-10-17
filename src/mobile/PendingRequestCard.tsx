// 原生 Pending 卡（Task 9）：Desktop IPC 的 command / permissions 审批与 Plan
// blocking input 问答的移动端面。
//
// **红线**（计划 Task 9 / spec S3/S5）：
// - 只回显 requestKey + contextToken；ownerClientId / targetClientId / raw method
//   由服务端 Owner Registry 解析（S3），本组件经 respondCodexPending 的白名单
//   契约体上行，**绝不**自造这些字段；
// - UNKNOWN_DELIVERY 是明确回执（已发出、结果未知）——显示「投递结果未知，请在
//   Desktop 核对」，**绝不出现自动重试按钮**（S5）；
// - Accepted 只表示已受理，**不标 Turn 完成**；
// - requestKey/token 变化（owner 换代 / 请求重发）→ 清旧草稿与旧回执，绝不把
//   旧上下文的答案灌进新请求（不误答）；
// - 请求消失以新投影为准（父组件按 requestKey 挂载/卸载，本组件不自行判断）；
// - **提交 ID 生命周期（R12）**：提交生成的 actionId 保持到「结果核对完成」——
//   网络异常（丢回执）不换 ID、不解锁重投，改用 fetchCodexAction 只读核对原
//   action（同 request 借新 ID 再次提交 = 双投窗口，服务端 R4 同款防线）；
// - **响应代际守卫（R12）**：以 {requestKey, contextToken} 为代际，token 轮换后
//   旧 Promise 的 resolve/异常/核对结果一律作废——不回写 receipt、不清新草稿、
//   不覆盖新卡状态、不上行 onReceipt。
//
// isSecret 的问题用 password 输入（不回显）；提交成功后立即清草稿——投影刷新
// 换卡之前秘密也不残留在 DOM。
import { useEffect, useRef, useState } from "react";
import InteractiveCard from "./InteractiveCard";
import i18n from "@/i18n";
import {
  fetchCodexAction,
  newActionId,
  respondCodexPending,
  type ActionReceipt,
  type ControlAvailability,
  type PendingView,
  type QuestionView,
} from "./codexIpcApi";
import { ApiError } from "./api";

// ============================================================
// 四档能力横幅（服务端下发档位，前端只渲染不推断——spec F8）
// ============================================================

/** 能力横幅：四档各一句 + stale 保留视图说明。data-availability / data-stale
 *  供测试与上游分诊（文案断言在 PendingRequestCard.test 固定 zh 后逐字断言）。 */
export function CodexAvailabilityBanner({
  availability,
  stale,
}: {
  availability: ControlAvailability;
  stale: boolean;
}) {
  return (
    <div
      data-testid="codex-availability-banner"
      data-availability={availability}
      data-stale={stale ? "true" : "false"}
      className="flex flex-wrap items-center gap-2 rounded-lg bg-[var(--btnp)]/10 px-3 py-2 text-xs text-[var(--tx)]"
    >
      <span data-testid="codex-availability-label">
        {i18n.t(`mobile.codex.availability.${availability}`)}
      </span>
      {stale && (
        <span data-testid="codex-stale-note" className="text-amber-700 dark:text-amber-400">
          {i18n.t("mobile.codex.staleBanner")}
        </span>
      )}
    </div>
  );
}

// ============================================================
// 卡片
// ============================================================

interface PendingRequestCardProps {
  sessionId: string;
  request: PendingView;
  /** 回执上行（200 明确回执，含 unknown_delivery——由父组件决定是否消费） */
  onReceipt?: (receipt: ActionReceipt) => void;
  /** 投影 stale（禁写）：卡可见但一切动作禁用（保留最后视图，不误答旧上下文） */
  writeLocked?: boolean;
}

/** 不支持/不可操作的原因文案（disabledReasonCode → 用户可读；未知码落通用档） */
function disabledCopy(code: string | null): string {
  if (code === "capability_unverified") {
    return i18n.t("mobile.codex.pending.unverified");
  }
  return i18n.t("mobile.codex.pending.unsupported");
}

/** 回执 → 底注文案（unknown 是「结果未知」不是失败，更不是可重试错误——S5） */
function receiptCopy(r: ActionReceipt): string {
  if (r.delivery === "unknown_delivery") return i18n.t("mobile.codex.pending.unknownDelivery");
  if (r.delivery === "accepted") return i18n.t("mobile.codex.pending.accepted");
  if (r.delivery === "rejected") return i18n.t("mobile.codex.pending.rejectedReceipt");
  return i18n.t("mobile.codex.pending.notSent");
}

/** R12：账本查询终态 → 明确回执（网络异常后的只读核对结果）。accepted = 已受理；
 *  rejected 分支**尊重原始回执语义**（2026-10-17 复审 C5）：DAO 用 rejected 状态
 *  同时承载 NotSent/Rejected，区别以 receipt.delivery 为准——not_sent = 明确零
 *  投递（底注「未发送」，按钮保留：再按 = 新 actionId，零投递上重试安全）；
 *  rejected = 明确拒绝（锁卡）。绝不按粗粒度 state 重造不同语义。
 *  cancelled/queued/reserved/unknown = 未终态或已撤 → null（保持「投递结果未知」）。 */
function actionViewToReceipt(
  actionId: string,
  view: { state: string; receipt: ActionReceipt | null }
): ActionReceipt | null {
  if (view.state === "accepted") {
    return {
      actionId,
      delivery: "accepted",
      route: view.receipt?.route ?? "desktop_ipc",
      turnId: view.receipt?.turnId ?? null,
      envelopeId: view.receipt?.envelopeId ?? null,
      reasonCode: view.receipt?.reasonCode ?? null,
    };
  }
  if (view.state === "rejected") {
    return {
      actionId,
      delivery: view.receipt?.delivery ?? "rejected",
      route: view.receipt?.route ?? "desktop_ipc",
      turnId: view.receipt?.turnId ?? null,
      envelopeId: view.receipt?.envelopeId ?? null,
      reasonCode: view.receipt?.reasonCode ?? null,
    };
  }
  return null;
}

/** C5：**可靠的投递前拒绝**判定——只有我方机器码错误体能认定零投递、ID 让位
 *  解锁再按（新动作）；其余一切（5xx/代理错误/畸形响应体）与无响应同：提交结果
 *  未知，保留同 ID 只读核对（S5：代理可在服务端已处理后返回 504，新 ID 重按 =
 *  双投窗口）。 */
const PRE_DELIVERY_ERROR_CODES = new Set([
  "bad_request", // malformed / deny_unknown_fields（登记前拒绝）
  "answers_mismatch", // 答案限额校验（登记前拒绝）
  "answer_not_option",
  "invalid_id",
  "forbidden", // 设备门禁（gate 层，登记前拒绝）
  "no_session", // 会话不在快照（无任何投递）
  "no_action", // 动作不存在（他人/已清除）
]);

function isPreDeliveryRefusal(e: ApiError): boolean {
  if (e.status === null) return false;
  const code = typeof e.data?.error === "string" ? e.data.error : null;
  return code !== null && PRE_DELIVERY_ERROR_CODES.has(code);
}

/** 一题的已答标签：勾选的选项 + 「其他」自由文本（isOther 才有输入框） */
function answerLabels(q: QuestionView, picked: string[], free: string): string[] {
  const labels = [...picked];
  const f = free.trim();
  if (q.isOther && f) labels.push(f);
  return labels;
}

export default function PendingRequestCard({
  sessionId,
  request,
  onReceipt,
  writeLocked = false,
}: PendingRequestCardProps) {
  const [busy, setBusy] = useState(false);
  const [receipt, setReceipt] = useState<ActionReceipt | null>(null);
  const [error, setError] = useState<string | null>(null);
  /** 逐题作答态（qid → 已勾选项 / 自由文本草稿） */
  const [picked, setPicked] = useState<Record<string, string[]>>({});
  const [freeTexts, setFreeTexts] = useState<Record<string, string>>({});
  /** R12：结果核对中（提交网络异常后的只读核对在途）——动作锁定，防借新 ID 双投 */
  const [verifying, setVerifying] = useState(false);
  /** R12：核对完成但结果未知（查不到/未终态）——保持锁定（S5：绝不自动重试），
   *  等新投影换代（token 轮换）重置 */
  const [outcomeUnknown, setOutcomeUnknown] = useState(false);
  /** R12：本次提交的 actionId——从「提交发出」保持到「结果核对完成」。网络异常
   *  后不换 ID 不解锁重投，改用 fetchCodexAction 只读核对原 action。 */
  const submitActionIdRef = useRef<string | null>(null);
  /** R12：上下文代际守卫——{requestKey, contextToken} 换代即 +1。旧代际的在途
   *  Promise（resolve/异常/核对结果）落地时比对代际，过期一律丢弃。 */
  const generationRef = useRef(0);

  // **上下文换代防线**：requestKey 不变但 contextToken 变化（owner 换代 / 请求
  // 消失重现）= 服务端判定旧上下文作废——旧草稿、旧回执、旧错误一律清空，
  // 绝不把旧态灌进新 token 的应答里（不误答）。requestKey 变化由父组件换 key
  // 重挂兜底，这里覆盖「同 key 新 token」的路径。R12：换代同时推进代际并作废
  // 旧提交 ID（旧代际在途响应此后不得回写新卡）。
  useEffect(() => {
    generationRef.current += 1;
    submitActionIdRef.current = null;
    setPicked({});
    setFreeTexts({});
    setReceipt(null);
    setError(null);
    setBusy(false);
    setVerifying(false);
    setOutcomeUnknown(false);
  }, [request.requestKey, request.contextToken]);

  const isInput = request.kind === "user_input";
  const operable =
    request.actionable &&
    (isInput || request.kind === "command_approval" || request.kind === "permissions_approval");

  // 未答题面（防静默丢题第一道闸；第二道在服务端——空答案 400）
  const unanswered = isInput
    ? request.questions.filter(
        (q) => answerLabels(q, picked[q.id] ?? [], freeTexts[q.id] ?? "").length === 0
      )
    : [];
  const canSubmit = isInput && unanswered.length === 0;

  // 已回执锁定：unknown/accepted/rejected 后动作全部禁用（防双答——unknown 绝不
  // 重发原动作）。not_sent 是保守判定的零投递：保留按钮（再按 = 新 actionId，
  // 零投递基础上重试安全）。R12：核对中/结果未知同样锁定（提交结果没核对清楚
  // 之前，任何再次提交都是双投窗口）。
  const locked = receipt !== null && receipt.delivery !== "not_sent";
  const disabled = busy || locked || verifying || outcomeUnknown || writeLocked || !operable;

  const respond = async (action: "approve" | "reject" | "answer") => {
    // R12：核对中/结果未知/已回执锁定一律拦截（answer 也不例外——锁定就是锁定，
    // 提交按钮自身的 disabled 只剩 UX 层意义）
    if (busy || verifying || outcomeUnknown || locked) return;
    if (disabled && action !== "answer") return;
    setBusy(true);
    setError(null);
    const myGen = generationRef.current;
    const stale = () => generationRef.current !== myGen;
    // R12：一次用户动作一个 ID；网络异常后**沿同 ID**核对（不换 ID 重发）
    if (submitActionIdRef.current === null) {
      submitActionIdRef.current = newActionId();
    }
    const actionId = submitActionIdRef.current;
    try {
      const r = await respondCodexPending({
        sessionId,
        actionId,
        requestKey: request.requestKey,
        contextToken: request.contextToken,
        action,
        // 白名单契约：仅 answer 携带 answers（approve/reject 多一个键都会 400）
        ...(action === "answer"
          ? {
              answers: Object.fromEntries(
                request.questions.map((q) => [
                  q.id,
                  answerLabels(q, picked[q.id] ?? [], freeTexts[q.id] ?? ""),
                ])
              ),
            }
          : {}),
      });
      if (stale()) return; // 旧 token 的响应：不回写 receipt、不清新草稿、不上行
      // 明确回执 = 结果核对完成 → ID 让位（下一次用户动作 = 新 ID）
      submitActionIdRef.current = null;
      setReceipt(r);
      // 提交成功即清草稿：isSecret 的回答不残留在 DOM（换卡前也不残留）
      setPicked({});
      setFreeTexts({});
      onReceipt?.(r);
    } catch (e) {
      if (stale()) return; // 旧 token 的异常：不覆盖新卡状态
      // 409 的两臂（复审 P3-5）：stale_context = 过期上下文（等新投影换 token）；
      // action_conflict = 同 actionId 重放不同载荷。两者都是投递前明确拒绝——
      // ID 让位（新上下文/新动作生成新 ID），文案按错误码精确区分不串。
      if (
        e instanceof ApiError &&
        e.status === 409 &&
        (e.data?.error === "stale_context" || e.data?.error === "action_conflict")
      ) {
        submitActionIdRef.current = null;
        setError(
          e.data?.error === "stale_context"
            ? i18n.t("mobile.codex.pending.staleContext")
            : i18n.t("mobile.codex.pending.failed", { message: "action_conflict" })
        );
        return;
      }
      if (e instanceof ApiError && isPreDeliveryRefusal(e)) {
        // C5：**可靠的投递前拒绝**（我方机器码错误体：400 校验 / 403 设备门 /
        // 404 会话或动作不存在）——零投递可判，ID 让位，解锁可再按（新动作新 ID）。
        submitActionIdRef.current = null;
        const reason = typeof e.data?.reason === "string" ? e.data.reason : e.message;
        setError(i18n.t("mobile.codex.pending.failed", { message: reason }));
        return;
      }
      // 其余一切（5xx/代理错误/无响应可读）= **提交结果未知**：代理可在服务端已
      // 处理后返回 504——不换 ID、不解锁重投（新 ID 重按 = 双投窗口，S5），改用
      // fetchCodexAction 只读核对原 action，按查询结果呈现（R12/C5）。
      setVerifying(true);
      try {
        const view = await fetchCodexAction(actionId);
        if (stale()) return;
        const confirmed = actionViewToReceipt(actionId, view);
        if (confirmed !== null) {
          // 账本终态（accepted/rejected）= 结果核对完成：按查询结果呈现（与 200
          // 回执同口径），ID 让位
          submitActionIdRef.current = null;
          setReceipt(confirmed);
          setPicked({});
          setFreeTexts({});
          onReceipt?.(confirmed);
        } else {
          // 未终态（queued/reserved/unknown）/已取消：结果未知——保持锁定（绝不
          // 自动重试），等新投影换代重置
          setOutcomeUnknown(true);
        }
      } catch {
        if (stale()) return;
        // 查不到/核对查询自身失败：结果未知——保持锁定（ID 保留，核对未完成）
        setOutcomeUnknown(true);
      } finally {
        if (!stale()) setVerifying(false);
      }
    } finally {
      if (!stale()) setBusy(false);
    }
  };

  // ---- 卡头（按 kind 分档；unsupported 也是一等可见态——能力诚实）----
  const titleKey =
    request.kind === "command_approval"
      ? "mobile.codex.pending.titleCommand"
      : request.kind === "permissions_approval"
        ? "mobile.codex.pending.titlePermissions"
        : isInput
          ? "mobile.codex.pending.titleInput"
          : "mobile.codex.pending.titleUnsupported";
  const tone = request.kind === "user_input" ? "question" : operable ? "approve" : "neutral";

  return (
    <InteractiveCard
      tone={tone}
      testId="codex-pending-card"
      mode={request.kind}
      pulsing={operable && !locked && !verifying && !outcomeUnknown && !writeLocked}
      title={i18n.t(titleKey)}
      titleSuffix={
        <span data-testid="codex-pending-request-key" className="text-xs text-[var(--mut)]">
          {" "}
          · Desktop
        </span>
      }
    >
      {/* 主体：命令 / 权限摘要 / 逐题作答 */}
      {request.command && (
        <pre
          data-testid="codex-pending-command"
          className="mt-1 max-h-40 overflow-auto rounded bg-[var(--cb)]/40 px-2 py-1 text-xs break-all whitespace-pre-wrap"
        >
          {/* cwd 前缀独立节点：命令正文必须是独立 text node（getByText 精确匹配
              与用户点选复制都以命令原文为单位） */}
          {request.cwd && <span className="text-[var(--mut)]">{request.cwd}$ </span>}
          <span data-testid="codex-pending-command-text">{request.command}</span>
        </pre>
      )}
      {request.permissionSummary && (
        <p data-testid="codex-pending-permission" className="mt-1 text-xs break-words">
          {request.permissionSummary}
        </p>
      )}
      {request.reason && <p className="mt-1 text-xs text-[var(--mut)]">{request.reason}</p>}

      {!operable && (
        <p
          data-testid="codex-pending-disabled"
          className="mt-1 text-xs text-amber-700 dark:text-amber-400"
        >
          {disabledCopy(request.disabledReasonCode)}
        </p>
      )}

      {isInput &&
        operable &&
        request.questions.map((q) => {
          const sel = picked[q.id] ?? [];
          return (
            <div key={q.id} data-testid={`codex-q-${q.id}`} className="mt-2">
              <p className="text-xs font-medium">
                {q.header ? `${q.header}｜` : ""}
                {q.question}
              </p>
              <div className="mt-1 flex flex-col gap-1">
                {q.options.map((o, i) => (
                  <label key={`${o.label}-${i}`} className="flex items-start gap-1 text-xs">
                    <input
                      type="checkbox"
                      data-testid={`codex-q-${q.id}-opt-${i}`}
                      checked={sel.includes(o.label)}
                      disabled={disabled}
                      onChange={() => {
                        // 选项点击即选（再点取消——answers 是标签数组，支持多项）
                        setPicked((prev) => {
                          const cur = prev[q.id] ?? [];
                          return {
                            ...prev,
                            [q.id]: cur.includes(o.label)
                              ? cur.filter((x) => x !== o.label)
                              : [...cur, o.label],
                          };
                        });
                      }}
                    />
                    <span>
                      {o.label}
                      {o.description && (
                        <span className="text-[var(--mut)]">（{o.description}）</span>
                      )}
                    </span>
                  </label>
                ))}
                {/* isOther=true 才有自由作答；isSecret 用 password 不回显。
                    isOther=false 只能选项——绝不渲染自由文本入口（服务端同样拒绝） */}
                {q.isOther && (
                  <input
                    type={q.isSecret ? "password" : "text"}
                    data-testid={`codex-q-${q.id}-free`}
                    aria-label={i18n.t("mobile.codex.pending.freeAria")}
                    placeholder={i18n.t("mobile.codex.pending.freePlaceholder")}
                    value={freeTexts[q.id] ?? ""}
                    disabled={disabled}
                    autoComplete={q.isSecret ? "new-password" : "off"}
                    maxLength={10000}
                    onChange={(e) =>
                      setFreeTexts((prev) => ({
                        ...prev,
                        [q.id]: e.target.value.slice(0, 10000),
                      }))
                    }
                    className="rounded border border-[var(--cb)] bg-transparent px-2 py-1 text-xs focus:ring-2 focus:ring-[var(--btnp)] focus:outline-none"
                  />
                )}
              </div>
            </div>
          );
        })}

      {/* 未答全提示（isInput 且可操作时） */}
      {isInput && operable && unanswered.length > 0 && (
        <p
          data-testid="codex-pending-incomplete"
          className="mt-2 text-xs text-rose-700 dark:text-rose-300"
        >
          {i18n.t("mobile.codex.pending.incomplete", { n: unanswered.length })}
        </p>
      )}
      {writeLocked && operable && (
        <p
          data-testid="codex-pending-writelocked"
          className="mt-1 text-xs text-amber-700 dark:text-amber-400"
        >
          {i18n.t("mobile.codex.pending.writeLocked")}
        </p>
      )}

      {/* 动作区：command/permissions 两按钮；user_input 提交；unsupported 无按钮 */}
      {operable && (
        <div className="mt-2 flex flex-wrap items-center gap-2">
          {isInput ? (
            <button
              type="button"
              data-testid="codex-pending-submit"
              disabled={disabled || !canSubmit}
              onClick={() => {
                void respond("answer");
              }}
              className="rounded-full bg-emerald-500/15 px-3 py-0.5 text-xs text-emerald-800 disabled:opacity-40 dark:bg-emerald-400/15 dark:text-emerald-300"
            >
              {i18n.t("mobile.codex.pending.submit")}
            </button>
          ) : (
            <>
              <button
                type="button"
                data-testid="codex-pending-approve"
                disabled={disabled}
                onClick={() => {
                  void respond("approve");
                }}
                className="rounded-full bg-emerald-500/15 px-3 py-0.5 text-xs text-emerald-800 disabled:opacity-40 dark:bg-emerald-400/15 dark:text-emerald-300"
              >
                {i18n.t("mobile.codex.pending.approve")}
              </button>
              <button
                type="button"
                data-testid="codex-pending-reject"
                disabled={disabled}
                onClick={() => {
                  void respond("reject");
                }}
                className="rounded-full bg-rose-500/15 px-3 py-0.5 text-xs text-rose-800 disabled:opacity-40 dark:bg-rose-400/15 dark:text-rose-300"
              >
                {i18n.t("mobile.codex.pending.reject")}
              </button>
            </>
          )}
        </div>
      )}

      {/* 底注：回执分诊 / 错误。unknown 文案指向 Desktop 核对——本组件**没有**
          任何「重试」按钮（S5 红线，PendingRequestCard.test 逐字锁定）。
          R12：核对中/结果未知（网络异常后的只读核对）也在此如实呈现 */}
      {(receipt !== null || error !== null || verifying || outcomeUnknown) && (
        <p
          data-testid="codex-pending-note"
          className={`mt-1 text-xs ${
            error !== null
              ? "text-rose-700 dark:text-rose-300"
              : receipt?.delivery === "unknown_delivery" || verifying || outcomeUnknown
                ? "text-amber-700 dark:text-amber-400"
                : "text-slate-600 dark:text-slate-300"
          }`}
        >
          {error !== null
            ? error
            : receipt !== null
              ? receiptCopy(receipt)
              : verifying
                ? i18n.t("mobile.codex.pending.verifying")
                : i18n.t("mobile.codex.pending.unknownDelivery")}
        </p>
      )}
    </InteractiveCard>
  );
}
