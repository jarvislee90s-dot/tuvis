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
import InteractiveCard, { toneTokens } from "./InteractiveCard";
import i18n from "@/i18n";
import {
  codexCapabilityTier,
  isCodexPreDeliveryRefusal,
  newActionId,
  probeReasonErrorKey,
  probeReasonStage,
  respondCodexPending,
  verifyCodexAction,
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
  sendAttemptable,
}: {
  availability: ControlAvailability;
  stale: boolean;
  /** #172/T9a：探测只读事实。老后端不下发 → undefined（按「无探测信息」处理）。 */
  probeStatus?: ProbeStatus | undefined;
  /**
   * S5：发送静态门（send-info 展示面单点，SessionDetail 经 MessageComposer 下发）
   * 判定的「当前可尝试发送」。语义裁决（设计 §3.4）：底层 read_only 只代表已取证
   * 能力，须与当前可尝试的产品操作区分——`true` 时横幅**不得**宣称「暂不能远程
   * 发送」，改走自动适配尝试中文案（不宣称已解锁全部能力）；`false`/`undefined`
   * （静态门拒绝 / 未取得——旧宿主或拉取失败）维持原文案，不猜。
   */
  sendAttemptable?: boolean | undefined;
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
            {/* S5：read_only + 静态门放行 → 不宣称「暂不能远程发送」（§3.4：底层
                read_only 只代表已取证能力，与当前可尝试的操作区分）；其余维持原档位文案 */}
            {availability === "read_only" && sendAttemptable === true
              ? i18n.t("mobile.codex.availability.readOnlyAttemptable")
              : i18n.t(`mobile.codex.availability.${availability}`)}
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
/** 该题的有效答策（缺省保守单值；旧载荷不多选）。 */
function answerPolicyOf(q: QuestionView) {
  return q.answerPolicy ?? { allowMultiple: false, maxAnswers: 1, allowFreeText: q.isOther };
}

/** 一题归一化后的答案数组（H13：单值下自由文本**替代**选项；多值下文本与选项
 *  一起计入上限，前后端同一数量规则）。 */
function answerLabels(q: QuestionView, picked: string[], free: string): string[] {
  const policy = answerPolicyOf(q);
  const f = free.trim();
  if (!f) return [...picked];
  const multi = policy.allowMultiple && policy.maxAnswers > 1;
  if (!multi) return [f];
  return [...picked, f];
}

export default function PendingRequestCard({
  sessionId,
  request,
  onReceipt,
  writeLocked = false,
}: PendingRequestCardProps) {
  /** N11：跨重挂的原动作锚（只读核对用；不恢复旧写资格）。 */
  const anchorStorageKey = `codex:pending:${sessionId}:${request.requestKey}:${request.contextToken}`;
  const [busy, setBusy] = useState(false);
  const [receipt, setReceipt] = useState<ActionReceipt | null>(null);
  const [error, setError] = useState<string | null>(null);
  /** B04：原生多题单卡逐题填写（同一次 pending 请求内）。 */
  const [questionIndex, setQuestionIndex] = useState(0);
  /** 逐题作答态（qid → 已勾选项 / 自由文本草稿） */
  const [picked, setPicked] = useState<Record<string, string[]>>({});
  const [freeTexts, setFreeTexts] = useState<Record<string, string>>({});
  /** N14：换上下文后旧非 secret 稿**不自动生效**，需用户显式恢复确认。 */
  const [staleDraft, setStaleDraft] = useState<{
    picked: Record<string, string[]>;
    free: Record<string, string>;
  } | null>(null);
  const draftVersionRef = useRef(0);
  const draftsRef = useRef<{
    picked: Record<string, string[]>;
    freeTexts: Record<string, string>;
  }>({ picked: {}, freeTexts: {} });
  /** 记录各题在编辑时的 secret 属性（防 qid 复用把旧 secret 变普通题后回显）。 */
  const secretFlagsRef = useRef<Record<string, boolean>>({});

  // N10/N14：草稿版本（提交时冻结；旧回执不得清掉版本已变的新稿）。
  useEffect(() => {
    draftsRef.current = { picked, freeTexts };
    draftVersionRef.current += 1;
  }, [picked, freeTexts]);
  /** R12：结果核对中（提交网络异常后的只读核对在途）——动作锁定，防借新 ID 双投 */
  const [verifying, setVerifying] = useState(false);
  /** R12：核对完成但结果未知（查不到/未终态）——保持锁定（S5：绝不自动重试），
   *  等新投影换代（token 轮换）重置 */
  const [outcomeUnknown, setOutcomeUnknown] = useState(false);
  /** R12：本次提交的 actionId——从「提交发出」保持到「结果核对完成」。网络异常
   *  后不换 ID 不解锁重投，改用 verifyCodexAction 只读核对原 action。 */
  const submitActionIdRef = useRef<string | null>(null);
  /** 渲染用的"是否有可用原动作锚"（ref 不能参与渲染）。 */
  const [hasActionAnchor, setHasActionAnchor] = useState(false);
  /** R12：上下文代际守卫——{requestKey, contextToken} 换代即 +1。旧代际的在途
   *  Promise（resolve/异常/核对结果）落地时比对代际，过期一律丢弃。 */
  const generationRef = useRef(0);
  /** H11：本地截止（从点击起算；POST 挂起也在 30 秒结束转圈）。
   *  每轮提交拥有自己的 timer（代际号）；旧轮 finalize 不得清新轮截止。 */
  const deadlineRef = useRef<{ gen: number; timer: number } | null>(null);
  const submitGenRef = useRef(0);
  const DEADLINE_MS = 30_000;
  const clearMyDeadline = (gen: number) => {
    if (deadlineRef.current?.gen === gen) {
      window.clearTimeout(deadlineRef.current.timer);
      deadlineRef.current = null;
    }
  };

  // **上下文换代防线**：requestKey 不变但 contextToken 变化（owner 换代 / 请求
  // 消失重现）= 服务端判定旧上下文作废——旧草稿、旧回执、旧错误一律清空，
  // 绝不把旧态灌进新 token 的应答里（不误答）。requestKey 变化由父组件换 key
  // 重挂兜底，这里覆盖「同 key 新 token」的路径。R12：换代同时推进代际并作废
  // 旧提交 ID（旧代际在途响应此后不得回写新卡）。
  useEffect(() => {
    generationRef.current += 1;
    submitActionIdRef.current = null;
    setHasActionAnchor(false);
    // N11：本上下文持久化的原动作锚（离开重进仍可继续只读核对；不恢复写资格）。
    const anchor = window.sessionStorage.getItem(anchorStorageKey);
    if (anchor) {
      submitActionIdRef.current = anchor;
      setHasActionAnchor(true);
      setOutcomeUnknown(true);
    } else {
      setOutcomeUnknown(false);
    }
    // N14：旧稿按原题 secret 属性隔离——secret 一律丢弃；非 secret 仅存为待确认。
    const prev = draftsRef.current;
    const secretIds = Object.keys(secretFlagsRef.current).filter(
      (id) => secretFlagsRef.current[id]
    );
    const stalePicked: Record<string, string[]> = {};
    const staleFree: Record<string, string> = {};
    let hasStale = false;
    for (const [id, labels] of Object.entries(prev.picked)) {
      if (secretIds.includes(id)) continue;
      if (labels.length > 0) {
        stalePicked[id] = labels;
        hasStale = true;
      }
    }
    for (const [id, text] of Object.entries(prev.freeTexts)) {
      if (secretIds.includes(id)) continue;
      if (text.trim().length > 0) {
        staleFree[id] = text;
        hasStale = true;
      }
    }
    setStaleDraft(hasStale ? { picked: stalePicked, free: staleFree } : null);
    setPicked({});
    setFreeTexts({});
    setReceipt(null);
    setError(null);
    setBusy(false);
    setVerifying(false);
    setQuestionIndex(0);
    if (deadlineRef.current !== null) {
      window.clearTimeout(deadlineRef.current.timer);
      deadlineRef.current = null;
    }
  }, [request.requestKey, request.contextToken, anchorStorageKey]);

  useEffect(() => {
    return () => {
      if (deadlineRef.current !== null) window.clearTimeout(deadlineRef.current.timer);
    };
  }, []);

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
    ? (request.questions ?? []).filter(
        (q) => answerLabels(q, picked[q.id] ?? [], freeTexts[q.id] ?? "").length === 0
      )
    : [];
  const canSubmit = isInput && unanswered.length === 0;
  const questions = request.questions ?? [];
  const multiQuestion = isInput && questions.length > 1;
  const questionCount = questions.length;
  const safeIndex = Math.min(questionIndex, Math.max(0, questionCount - 1));
  const visibleQuestions = multiQuestion ? [questions[safeIndex]] : questions;
  const currentAnswered =
    !multiQuestion ||
    answerLabels(
      questions[safeIndex],
      picked[questions[safeIndex]?.id] ?? [],
      freeTexts[questions[safeIndex]?.id] ?? ""
    ).length > 0;

  // 已回执锁定：unknown/accepted/rejected 后动作全部禁用（防双答——unknown 绝不
  // 重发原动作）。not_sent 是保守判定的零投递：保留按钮（再按 = 新 actionId，
  // 零投递基础上重试安全）。R12：核对中/结果未知同样锁定（提交结果没核对清楚
  // 之前，任何再次提交都是双投窗口）。
  const locked = receipt !== null && receipt.delivery !== "not_sent";
  const disabled = busy || locked || verifying || outcomeUnknown || writeLocked || !operable;

  /** H11：只清 secret 题稿（防泄漏）；非 secret 稿在结果未确认前保留待复核。 */
  const clearSecretDrafts = () => {
    const secretIds = new Set(request.questions.filter((q) => q.isSecret).map((q) => q.id));
    if (secretIds.size === 0) return;
    setPicked((prev) =>
      Object.fromEntries(Object.entries(prev).filter(([id]) => !secretIds.has(id)))
    );
    setFreeTexts((prev) =>
      Object.fromEntries(Object.entries(prev).filter(([id]) => !secretIds.has(id)))
    );
  };

  /** N10/N11：按**提交拥有者**退休/清稿——旧轮回执不得动新轮锚、busy、deadline、稿。 */
  const retireAnchor = (submit: number, gen: number) => {
    if (submitGenRef.current !== submit || generationRef.current !== gen) return;
    submitActionIdRef.current = null;
    setHasActionAnchor(false);
    window.sessionStorage.removeItem(anchorStorageKey);
  };

  const clearDraftsOwned = (
    submit: number,
    gen: number,
    version: number,
    mode: "all" | "secret"
  ) => {
    if (
      submitGenRef.current !== submit ||
      generationRef.current !== gen ||
      draftVersionRef.current !== version
    ) {
      return; // 旧回执/稿已改：不清新稿
    }
    if (mode === "all") {
      setPicked({});
      setFreeTexts({});
      return;
    }
    clearSecretDrafts();
  };

  const respond = async (action: "approve" | "reject" | "cancel" | "answer") => {
    // R12：核对中/结果未知/已回执锁定一律拦截（answer 也不例外——锁定就是锁定，
    // 提交按钮自身的 disabled 只剩 UX 层意义）
    if (busy || verifying || outcomeUnknown || locked) return;
    if (disabled && action !== "answer") return;
    setBusy(true);
    setError(null);
    const myGen = generationRef.current;
    const submitGen = ++submitGenRef.current;
    const draftVersion = draftVersionRef.current;
    if (deadlineRef.current !== null) window.clearTimeout(deadlineRef.current.timer);
    deadlineRef.current = {
      gen: submitGen,
      timer: window.setTimeout(() => {
        // H11：到点停止转圈并保持锁定（结果未知），绝不自动重发。
        if (submitGenRef.current !== submitGen) return;
        setBusy(false);
        setOutcomeUnknown(true);
      }, DEADLINE_MS),
    };
    const startedAt = Date.now();
    const stale = () => generationRef.current !== myGen || submitGenRef.current !== submitGen;
    // R12：一次用户动作一个 ID；网络异常后**沿同 ID**核对（不换 ID 重发）
    if (submitActionIdRef.current === null) {
      submitActionIdRef.current = newActionId();
      setHasActionAnchor(true);
      window.sessionStorage.setItem(anchorStorageKey, submitActionIdRef.current);
    }
    const actionId = submitActionIdRef.current;
    try {
      const r = await respondCodexPending({
        sessionId,
        actionId,
        requestKey: request.requestKey,
        contextToken: request.contextToken,
        action,
        // H11：如实透传剩余预算（客户端点击起算；服务端再夹上限）。
        frontBudgetMs: Math.max(0, DEADLINE_MS - (Date.now() - startedAt)),
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
      if (stale()) return; // 旧轮/旧 token 的响应：不回写、不清新稿、不动新锚
      clearMyDeadline(submitGen);
      const unconfirmed =
        r.delivery === "accepted" &&
        (r.reasonCode === "decision_unconfirmed" || r.reasonCode === "accepted_unbound");
      setReceipt(r);
      if (r.delivery === "rejected" || (r.delivery === "accepted" && !unconfirmed)) {
        // N11：只有**权威解决**才退休原锚；受理未确认保留锚与核对入口。
        retireAnchor(submitGen, myGen);
      }
      if (r.delivery === "unknown_delivery" || unconfirmed) setOutcomeUnknown(true);
      // N10/N14：清稿须核冻结 draftVersion（旧回执不得清新稿）。
      if (r.delivery === "rejected") {
        clearDraftsOwned(submitGen, myGen, draftVersion, "all");
      } else if (r.delivery === "accepted" && !unconfirmed) {
        clearDraftsOwned(submitGen, myGen, draftVersion, "secret");
      }
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
        clearMyDeadline(submitGen);
        retireAnchor(submitGen, myGen);
        setError(
          e.data?.error === "stale_context"
            ? i18n.t("mobile.codex.pending.staleContext")
            : i18n.t("mobile.codex.pending.failed", { message: "action_conflict" })
        );
        return;
      }
      if (e instanceof ApiError && isCodexPreDeliveryRefusal(e)) {
        // C5/H10：**可靠的投递前拒绝**（按机器码，不只看状态码）——零投递可判，
        // 结束本轮 timer、ID 让位，解锁可再按（新动作新 ID）。
        clearMyDeadline(submitGen);
        retireAnchor(submitGen, myGen);
        const reason = typeof e.data?.reason === "string" ? e.data.reason : e.message;
        setError(i18n.t("mobile.codex.pending.failed", { message: reason }));
        return;
      }
      // 其余一切（5xx/代理错误/无响应可读）= **提交结果未知**：代理可在服务端已
      // 处理后返回 504——不换 ID、不解锁重投（新 ID 重按 = 双投窗口，S5），改用
      // fetchCodexAction 只读核对原 action，按查询结果呈现（R12/C5）。
      setVerifying(true);
      const controller = new AbortController();
      const verifyTimer = window.setTimeout(() => controller.abort(), 10_000);
      try {
        // H11：真正的只读核对端点（消费迟回执／保证投影新鲜；≤10s、零发送）。
        const view = await verifyCodexAction(actionId, controller.signal);
        if (stale()) return;
        if (view.outcome === "delivered") {
          retireAnchor(submitGen, myGen);
          const confirmed: ActionReceipt = {
            actionId,
            delivery: "accepted",
            route: "desktop_ipc",
            turnId: view.turnId,
            envelopeId: null,
            reasonCode: view.reasonCode,
          };
          setReceipt(confirmed);
          clearDraftsOwned(submitGen, myGen, draftVersion, "secret");
          onReceipt?.(confirmed);
        } else if (view.outcome === "accepted_unbound") {
          // N11：受理未确认——保留锚与核对入口，不清整表。
          const confirmed: ActionReceipt = {
            actionId,
            delivery: "accepted",
            route: "desktop_ipc",
            turnId: view.turnId,
            envelopeId: null,
            reasonCode: view.reasonCode,
          };
          setReceipt(confirmed);
          setOutcomeUnknown(true);
          onReceipt?.(confirmed);
        } else if (view.outcome === "not_delivered" && view.stage === "settled") {
          retireAnchor(submitGen, myGen);
          const confirmed: ActionReceipt = {
            actionId,
            delivery: "not_sent",
            route: "none",
            turnId: null,
            envelopeId: null,
            reasonCode: view.reasonCode,
          };
          setReceipt(confirmed);
          onReceipt?.(confirmed);
        } else {
          // unresolved / 仍在途：结果未知——保留 ID/草稿，可再次核对（不重发）。
          setOutcomeUnknown(true);
        }
      } catch {
        if (stale()) return;
        // 核对超时/失败：结果未知——保留 ID/草稿，可再次核对。
        setOutcomeUnknown(true);
      } finally {
        window.clearTimeout(verifyTimer);
        if (!stale()) setVerifying(false);
        clearMyDeadline(submitGen);
      }
    } finally {
      if (!stale()) setBusy(false);
    }
  };

  /** H11：Unknown 后**再次只读核对**（不重发、不换 ID；≤10 秒有界）。 */
  const reverify = async () => {
    const actionId = submitActionIdRef.current;
    if (!actionId || verifying) return;
    setVerifying(true);
    const myGen = generationRef.current;
    const mySubmit = ++submitGenRef.current;
    const draftVersion = draftVersionRef.current;
    const stale = () => generationRef.current !== myGen || submitGenRef.current !== mySubmit;
    const controller = new AbortController();
    const timer = window.setTimeout(() => controller.abort(), 10_000);
    try {
      const view = await verifyCodexAction(actionId, controller.signal);
      if (stale()) return;
      if (view.outcome === "delivered") {
        retireAnchor(mySubmit, myGen);
        const confirmed: ActionReceipt = {
          actionId,
          delivery: "accepted",
          route: "desktop_ipc",
          turnId: view.turnId,
          envelopeId: null,
          reasonCode: view.reasonCode,
        };
        setReceipt(confirmed);
        setOutcomeUnknown(false);
        clearDraftsOwned(mySubmit, myGen, draftVersion, "secret");
        onReceipt?.(confirmed);
      } else if (view.outcome === "accepted_unbound") {
        // N11：受理未确认 → 保留锚与核对入口（不宣称已解决、不清整表）。
        const confirmed: ActionReceipt = {
          actionId,
          delivery: "accepted",
          route: "desktop_ipc",
          turnId: view.turnId,
          envelopeId: null,
          reasonCode: view.reasonCode,
        };
        setReceipt(confirmed);
        setOutcomeUnknown(true);
        onReceipt?.(confirmed);
      } else if (view.outcome === "not_delivered" && view.stage === "settled") {
        retireAnchor(mySubmit, myGen);
        const confirmed: ActionReceipt = {
          actionId,
          delivery: "not_sent",
          route: "none",
          turnId: null,
          envelopeId: null,
          reasonCode: view.reasonCode,
        };
        setReceipt(confirmed);
        setOutcomeUnknown(false);
        onReceipt?.(confirmed);
      } else {
        setOutcomeUnknown(true);
      }
    } catch {
      if (!stale()) setOutcomeUnknown(true);
    } finally {
      window.clearTimeout(timer);
      if (!stale()) setVerifying(false);
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
  const toneStyle = toneTokens(tone);

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
      {staleDraft && operable && !locked && (
        <div
          data-testid="codex-pending-stale-draft"
          className="mt-1 flex items-center justify-between gap-2 rounded-lg bg-amber-500/10 px-2 py-1 text-xs text-amber-800 dark:text-amber-300"
        >
          <span>{i18n.t("mobile.codex.pending.staleDraft")}</span>
          <button
            type="button"
            data-testid="codex-pending-restore-draft"
            onClick={() => {
              // N14：只恢复**当前仍存在且非 secret** 的题；旧 secret 永不回填。
              const ids = new Set(request.questions.map((q) => q.id));
              const secret = new Set(request.questions.filter((q) => q.isSecret).map((q) => q.id));
              setPicked((prev) => {
                const next = { ...prev };
                for (const [id, labels] of Object.entries(staleDraft.picked)) {
                  if (ids.has(id) && !secret.has(id)) next[id] = labels;
                }
                return next;
              });
              setFreeTexts((prev) => {
                const next = { ...prev };
                for (const [id, text] of Object.entries(staleDraft.free)) {
                  if (ids.has(id) && !secret.has(id)) next[id] = text;
                }
                return next;
              });
              setStaleDraft(null);
            }}
            className="underline"
          >
            {i18n.t("mobile.codex.pending.restoreDraft")}
          </button>
        </div>
      )}

      {!operable && (
        <p
          data-testid="codex-pending-disabled"
          className="mt-1 text-xs text-amber-700 dark:text-amber-400"
        >
          {disabledCopy(request.disabledReasonCode)}
        </p>
      )}

      {multiQuestion && operable && (
        <p data-testid="codex-pending-progress" className="mt-2 text-xs text-[var(--mut)]">
          {i18n.t("mobile.codex.pending.progress", {
            current: safeIndex + 1,
            total: questionCount,
          })}
        </p>
      )}
      {isInput &&
        operable &&
        visibleQuestions.map((q) => {
          const sel = picked[q.id] ?? [];
          const policy = answerPolicyOf(q);
          const multi = policy.allowMultiple && policy.maxAnswers > 1;
          const hasText = (freeTexts[q.id] ?? "").trim().length > 0;
          const atLimit = multi && sel.length + (hasText ? 1 : 0) >= policy.maxAnswers;
          return (
            <div key={q.id} data-testid={`codex-q-${q.id}`} className="mt-2">
              <p className="text-xs font-medium">
                {q.header ? `${q.header}｜` : ""}
                {q.question}
              </p>
              <div className="mt-1 flex flex-col gap-1">
                {q.options.map((o, i) => (
                  <label
                    key={`${o.label}-${i}`}
                    className={`flex items-start gap-2 rounded-lg px-2.5 py-1.5 text-xs ${toneStyle.action}`}
                  >
                    <span
                      aria-hidden="true"
                      className={`mt-0.5 inline-flex h-4 w-4 shrink-0 items-center justify-center rounded text-[10px] font-semibold ${toneStyle.badge} ${toneStyle.title}`}
                    >
                      {i + 1}
                    </span>
                    <input
                      type={multi ? "checkbox" : "radio"}
                      name={`codex-q-${q.id}`}
                      data-testid={`codex-q-${q.id}-opt-${i}`}
                      checked={sel.includes(o.label)}
                      disabled={disabled}
                      onChange={() => {
                        // R14：单值策略 = 选中即替换；多值策略 = 切换且不超上限。
                        secretFlagsRef.current[q.id] = q.isSecret;
                        setPicked((prev) => {
                          const cur = prev[q.id] ?? [];
                          if (sel.includes(o.label)) {
                            // 单值不允许"取消到空"（radio 语义）；多值可取消
                            return multi
                              ? { ...prev, [q.id]: cur.filter((x) => x !== o.label) }
                              : { ...prev, [q.id]: cur };
                          }
                          if (!multi) {
                            // H13：单值 = 选项与文本互斥（选项替换文本）。
                            setFreeTexts((prev) => ({ ...prev, [q.id]: "" }));
                            return { ...prev, [q.id]: [o.label] };
                          }
                          if (cur.length + (hasText ? 1 : 0) >= policy.maxAnswers) return prev;
                          return { ...prev, [q.id]: [...cur, o.label] };
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
                {policy.allowFreeText && (
                  <input
                    type={q.isSecret ? "password" : "text"}
                    data-testid={`codex-q-${q.id}-free`}
                    aria-label={i18n.t("mobile.codex.pending.freeAria")}
                    placeholder={i18n.t("mobile.codex.pending.freePlaceholder")}
                    value={freeTexts[q.id] ?? ""}
                    disabled={disabled}
                    autoComplete={q.isSecret ? "new-password" : "off"}
                    maxLength={10000}
                    onChange={(e) => {
                      secretFlagsRef.current[q.id] = q.isSecret;
                      const next = e.target.value.slice(0, 10000);
                      setPicked((prev) => {
                        // H13：单值下输入文本 = 替代已选选项；多值下已到上限时
                        // 不允许再新增文本（空稿也不占用名额）。
                        if (!multi) {
                          return next.trim().length > 0 ? { ...prev, [q.id]: [] } : prev;
                        }
                        if (next.trim().length > 0 && !hasText && atLimit) return prev;
                        return prev;
                      });
                      setFreeTexts((prev) => {
                        if (multi && next.trim().length > 0 && !hasText && atLimit) {
                          return prev;
                        }
                        return { ...prev, [q.id]: next };
                      });
                    }}
                    className="rounded border border-[var(--cb)] bg-transparent px-2 py-1 text-xs focus:ring-2 focus:ring-[var(--btnp)] focus:outline-none"
                  />
                )}
              </div>
            </div>
          );
        })}

      {/* 未答全提示（isInput 且可操作时） */}
      {isInput &&
        operable &&
        unanswered.length > 0 &&
        (!multiQuestion || safeIndex === questionCount - 1) && (
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
            <>
              {multiQuestion && safeIndex > 0 && (
                <button
                  type="button"
                  data-testid="codex-pending-prev"
                  disabled={disabled || busy || verifying}
                  onClick={() => setQuestionIndex((value) => Math.max(0, value - 1))}
                  className="rounded-full border border-[var(--bd)] px-3 py-0.5 text-xs"
                >
                  {i18n.t("mobile.codex.pending.prev")}
                </button>
              )}
              {multiQuestion && safeIndex < questionCount - 1 ? (
                <button
                  type="button"
                  data-testid="codex-pending-next"
                  disabled={disabled || !currentAnswered}
                  onClick={() =>
                    setQuestionIndex((value) => Math.min(questionCount - 1, value + 1))
                  }
                  className="rounded-full bg-[var(--ac)] px-3 py-0.5 text-xs text-white disabled:opacity-40"
                >
                  {i18n.t("mobile.codex.pending.next")}
                </button>
              ) : (
                <button
                  type="button"
                  data-testid="codex-pending-submit"
                  disabled={disabled || !canSubmit}
                  onClick={() => {
                    void respond("answer");
                  }}
                  className="rounded-full bg-emerald-500/15 px-3 py-0.5 text-xs text-emerald-800 disabled:opacity-40 dark:bg-emerald-400/15 dark:text-emerald-300"
                >
                  {multiQuestion
                    ? i18n.t("mobile.codex.pending.submitAll", { count: questionCount })
                    : i18n.t("mobile.codex.pending.submit")}
                </button>
              )}
              {multiQuestion && (
                <span className="text-[10px] text-[var(--mut)]">
                  {i18n.t("mobile.codex.pending.groupHint")}
                </span>
              )}
            </>
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
                    // R08：终止本轮必须发 cancel（后端 Reject→decline、Cancel→
                    // interrupt 语义）；发 reject 只会拒绝请求、回合继续。
                    void respond("cancel");
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
      {outcomeUnknown && !verifying && hasActionAnchor && (
        <button
          type="button"
          data-testid="codex-pending-verify"
          onClick={() => void reverify()}
          className="mt-1 text-xs text-[var(--mut)] underline"
        >
          {i18n.t("mobile.codex.pending.verify")}
        </button>
      )}
    </InteractiveCard>
  );
}
