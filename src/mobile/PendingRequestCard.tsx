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
  codexCapabilityTier,
  fetchCodexAction,
  newActionId,
  probeReasonErrorKey,
  probeReasonStage,
  respondCodexPending,
  type ActionReceipt,
  type CodexCapabilityTier,
  type ControlAvailability,
  type PendingView,
  type ProbeStatus,
  type QuestionView,
} from "./codexIpcApi";
import { ApiError } from "./api";

// ============================================================
// 能力横幅（服务端下发档位，前端只渲染不推断——spec F8）
// ============================================================

/** 三态徽标配色：已验证=绿、兼容探测（实验）=琥珀、未解锁=中性灰。
 *  配色与文案一起按态走——徽标的**颜色**也是信息，不能三态同色。 */
const TIER_CLASS: Record<CodexCapabilityTier, string> = {
  verified: "rounded-full bg-emerald-500/15 px-2 py-0.5 text-emerald-800 dark:text-emerald-300",
  compat: "rounded-full bg-amber-500/15 px-2 py-0.5 text-amber-800 dark:text-amber-300",
  locked: "rounded-full bg-slate-500/15 px-2 py-0.5 text-slate-700 dark:text-slate-300",
};

/**
 * 探测结论 → 展示文案（`failed` / `inconclusive` 必给原因）。
 *
 * 机器码 → 顶层 `errors` 段文案（动态码 `probe_failed:P2` 按**前缀**取基线文案，
 * stage 另附在文案尾）；词表外的码**不猜**——落通用「原因未知」。
 */
function probeReasonCopy(probe: ProbeStatus | undefined): string | null {
  if (probe === undefined) return null;
  if (probe.state !== "failed" && probe.state !== "inconclusive" && probe.state !== "idle") {
    return null;
  }
  const key = probeReasonErrorKey(probe.reasonCode);
  if (key === null) {
    // 词表外的码（failed / inconclusive）→ 通用「原因未知」（不猜具体原因）；
    // `idle` 且无码 = 探测事实**取不到**（不是「没通过」）→ 什么都不说（不猜）。
    return probe.state === "idle" ? null : i18n.t("mobile.codex.probe.reasonUnknown");
  }
  const base = i18n.t(key);
  const stage = probeReasonStage(probe.reasonCode) ?? probe.stage;
  if (stage === null) return base;
  return `${base}${i18n.t("mobile.codex.probe.stage", { stage })}`;
}

/**
 * 能力横幅：**三态徽标** + 服务端档位明细 + 探测事实（#172/T9b）。
 *
 * - 三态取自唯一映射源 `codexCapabilityTier()`（四档 → 三态），文案/配色按态走；
 * - 探测在飞（`probeStatus.state === "probing"`）时**只出骨架**：结论未定，此刻给出
 *   任何档位定论（含「未解锁」）都是抢跑——骨架只说「正在探测」；
 * - 用户可**取消等待**：取消 = 客户端放弃等待（收起骨架、记住「已取消等待」，
 *   不再反复显示骨架）。服务端探测本身有 15s 硬上限（`probe/runner.rs` 的
 *   `TOTAL_BUDGET`）自行结束——**不伪造服务端 abort**，界面如实写清这层语义；
 * - `failed` / `inconclusive`：横幅给出**原因**（机器码 → `errors` 段文案 + stage）。
 *
 * data-availability / data-tier / data-stale / data-probe-state 供测试与上游分诊
 * （文案断言在 PendingRequestCard.test 固定 zh 后逐字断言）。
 */
export function CodexAvailabilityBanner({
  availability,
  stale,
  probeStatus,
}: {
  availability: ControlAvailability;
  stale: boolean;
  /** #172/T9a：探测只读事实。老后端不下发 → undefined（按「无探测信息」处理）。 */
  probeStatus?: ProbeStatus | undefined;
}) {
  const tier = codexCapabilityTier(availability);
  const probeState = probeStatus?.state ?? null;
  const probing = probeState === "probing";
  /** 用户已取消等待（客户端放弃等待；服务端探测仍在 15s 硬上限内自行结束） */
  const [waitCancelled, setWaitCancelled] = useState(false);
  // 探测离开 probing → 复位「已取消等待」记忆（下一次进入 probing 又是新的一次等待）
  useEffect(() => {
    if (!probing) setWaitCancelled(false);
  }, [probing]);

  const showSkeleton = probing && !waitCancelled;
  const reason = probeReasonCopy(probeStatus);

  return (
    <div
      data-testid="codex-availability-banner"
      data-availability={availability}
      data-tier={tier}
      data-stale={stale ? "true" : "false"}
      data-probe-state={probeState ?? "none"}
      className="flex flex-wrap items-center gap-2 rounded-lg bg-[var(--btnp)]/10 px-3 py-2 text-xs text-[var(--tx)]"
    >
      {showSkeleton ? (
        <div data-testid="codex-probe-skeleton" className="flex flex-wrap items-center gap-2">
          <span data-testid="codex-probe-skeleton-label">
            {i18n.t("mobile.codex.probe.probing")}
          </span>
          {/* 骨架块：占位条（**不是**结论——此刻没有结论可写） */}
          <span aria-hidden className="h-2 w-16 animate-pulse rounded bg-[var(--mut)]/30" />
          <span aria-hidden className="h-2 w-10 animate-pulse rounded bg-[var(--mut)]/30" />
          <button
            type="button"
            data-testid="codex-probe-cancel"
            onClick={() => setWaitCancelled(true)}
            className="rounded-full bg-[var(--btnp)]/15 px-2 py-0.5"
          >
            {i18n.t("mobile.codex.probe.cancelWait")}
          </button>
        </div>
      ) : waitCancelled && probing ? (
        <span
          data-testid="codex-probe-wait-cancelled"
          className="text-amber-700 dark:text-amber-400"
        >
          {i18n.t("mobile.codex.probe.waitCancelled")}
        </span>
      ) : (
        <>
          <span data-testid="codex-availability-label">
            {i18n.t(`mobile.codex.availability.${availability}`)}
          </span>
          <span data-testid="codex-tier-badge" data-tier={tier} className={TIER_CLASS[tier]}>
            {i18n.t(`mobile.codex.tier.${tier}`)}
          </span>
        </>
      )}
      {reason !== null && (
        <p data-testid="codex-probe-reason" className="w-full text-amber-700 dark:text-amber-400">
          {reason}
        </p>
      )}
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
  // #172/T7：受理但 5s 内没看到 pending 从快照消失 → 如实说「未确认」（不是成功）
  if (r.delivery === "accepted" && r.reasonCode === "decision_unconfirmed") {
    return i18n.t("mobile.codex.pending.decisionUnconfirmed");
  }
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
 *  双投窗口）。
 *
 *  码单来源 = POST /codex/pending-response **登记前**的确定性拒绝（零投递）：
 *  - 请求体 malformed / deny_unknown_fields / 必填字段缺失（400 bad_request）；
 *  - 答案限额校验（validate_answers：empty_answers / too_many_questions /
 *    too_many_answers / answer_too_long / answers_too_large——全在登记前拒绝，
 *    2026-10-19 复审补入：缺了会被误归「结果未知」→ fetchCodexAction 必 404 →
 *    永久锁卡，而真实语义是「可重试的确定性拒绝」）；
 *  - 动作词与 pending 类别不相容（409 action_mismatch——登记前复核，同上补入）；
 *  - IPC 能力关（503 capability_unavailable——`!hub.ipc_available()`，登记之前
 *    即拒绝、零投递，2026-10-19 复审补入：缺了会被误归「结果未知」→ 同上锁卡）；
 *  - 登记后、派发前的校验拒绝（answers_mismatch / answer_not_option / invalid_id）；
 *  - 设备门禁（403）/ 会话不在快照（404）。
 *
 *  `no_action` 是防御位：POST /codex/pending-response 不会发该码；但既有端点里
 *  GET /codex/action（查询面）、POST /codex/queue/retract（撤回面）与
 *  POST /codex/queue/force-continue（人工解除面，404 = 不存在/他人动作）都会发
 *  （2026-10-19 复审修正：原注释称「只有 GET 会发」已漂移）。留着防未来端点复用
 *  该码时静默漏进「结果未知」臂（多算一位只多一次解锁误判面，漏算才是锁卡事故）。 */
const PRE_DELIVERY_ERROR_CODES = new Set([
  "bad_request", // malformed / deny_unknown_fields（登记前拒绝）
  "answers_mismatch", // 答案限额校验（登记前拒绝）
  "answer_not_option",
  "invalid_id",
  "forbidden", // 设备门禁（gate 层，登记前拒绝）
  "no_session", // 会话不在快照（无任何投递）
  "no_action", // 防御位：pending-response 不发；查询/人工解除端点会发（见上方块注释）
  "capability_unavailable", // IPC 能力关（503，登记前确定性零投递）
  "empty_answers", // ↓ validate_answers 五码：答案限额（登记前拒绝）
  "too_many_questions",
  "too_many_answers",
  "answer_too_long",
  "answers_too_large",
  "action_mismatch", // 动作词与 pending 类别不相容（409，登记前复核）
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
  const isApproval =
    request.kind === "command_approval" ||
    request.kind === "file_change_approval" ||
    request.kind === "permissions_approval";
  const operable = request.actionable && (isInput || isApproval);

  // #172/T7：按钮只按服务端给的 allowedDecisions 渲染（前端不做策略判断）。
  // 命令/文件审批的老回执没有该字段 → 退回「批准 + 拒绝」两键。
  const decisions = request.allowedDecisions ?? [];
  const canAccept = decisions.includes("accept");
  const canDecline = decisions.includes("decline");
  const canCancel = decisions.includes("cancel");
  const showApprove = canAccept || decisions.length === 0;
  const showDecline = canDecline || decisions.length === 0;
  const showCancel = canCancel;

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
      : request.kind === "file_change_approval"
        ? "mobile.codex.pending.titleFileChange"
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
      {/* 文件变更审批：服务端只给「变更条数」这类安全摘要（真机参数形态未取证） */}
      {request.fileSummary && (
        <p data-testid="codex-pending-files" className="mt-1 text-xs break-words">
          {i18n.t("mobile.codex.pending.fileChanges", { n: request.fileSummary })}
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
              {showApprove && (
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
              )}
              {showDecline && (
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
              )}
              {/* 未知 build（无真机审批证据）只给「终止本轮」：cancel 语义 = 拒绝并立即
                  中断，与服务端 allowedDecisions 一一对应 */}
              {showCancel && (
                <button
                  type="button"
                  data-testid="codex-pending-cancel-turn"
                  disabled={disabled}
                  onClick={() => {
                    void respond("reject");
                  }}
                  className="rounded-full bg-rose-500/15 px-3 py-0.5 text-xs text-rose-800 disabled:opacity-40 dark:bg-rose-400/15 dark:text-rose-300"
                >
                  {i18n.t("mobile.codex.pending.cancelTurn")}
                </button>
              )}
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
