// ZCode 待处理卡（R3）：问答=蓝色交互卡（逐题分页 + 本地暂存 + 一次提交），
// 审批=红色交互卡（只放行一次性允许/拒绝 + 只读命令正文）。
//
// 为什么从 `ZcodeControlPanel.tsx` 抽出来：面板同时承担连接/发送/队列/档位，卡内的
// 分页、草稿、受理锁与回执身份校验逻辑自成一体，混在一起无法独立锁定行为。
//
// 纪律（与 R3 简报 / 既有 T4–T8 合同一致）：
// - 一个 interaction 的多题**一次只显示一题**，答案只在本卡内存暂存；进入确认页并
//   点「提交」才发**一次** `ControlAction::Answer`（answers 按原始题序、原始题干、
//   原始 option value）。未确认前零 answer POST（`autoResolution` 的 snooze 除外，
//   那是既有合同：首次有效编辑补发一次）。
// - 未答完整不提交：任一题既无选中项也无自由文本 → 提交禁用并就地指名缺哪题。
// - 受理锁按 **interaction 身份 + 代际**：宿主旁路 revision 推进不丢草稿、也不解锁
//   已受理的重复提交；interaction 被替换/撤销或 generation 变化 → 草稿与锁定一并
//   清空；迟到回执（旧身份）绝不写到新卡上。
// - 写面硬门 `gateReason`、能力矩阵、同帧双击闸、`settled` 锁定全部保留：阅读面照常，
//   作答面禁用并给本地化原因（码只进 `data-reason-code`）。
// - 敏感作答（`sensitive`）只存本次挂载的内存：失焦清可见值，确认页只说「已填写」，
//   绝不落 localStorage/sessionStorage。
// - 审批只渲染宿主 kind 为 `allowOnce`/`deny` 的选项；`fullAccess`（含宿主给的
//   `custom` 特权项）、`allowAlways`/`allowSession` 一律不渲染——常驻授权与完全访问
//   不在本轮范围。
// - `detail`（官方 `payload.detail`，Bash 即命令正文）默认折叠、展开只读；它不参与
//   任何提交动作。
// - 不可代答的 userInput 子型（服务端**显式** `answerable=false`，如 plan_approval /
//   MCP elicitation）渲染**只读卡**：只有原因 +「打开官方页面」，不渲染选项与提交键——
//   绝不「选完才被 `needs_evidence` 拒」。是否可答由服务端与应答闸共用的同一纯判定给出，
//   前端不另判；字段**缺失**时按旧口径兼容回退（见 [`isPendingAnswerable`）。
// - C1 计划批准（服务端 `planApproval=true`）：标题/tab 用「计划批准」，只读原因用专用
//   文案（说明这是计划批准、需去官方页面批准或继续修改），保留「打开官方页面」；
//   其余只读面同上（零选项/零提交/零 snooze/零 resolve）。C2 原生批准仍 BLOCKED。
// - `detailTruncated=true`：正文不是全貌，卡片就地说明并**撤掉「允许一次」**（拒绝与
//   官方页面保留）。截断真值由服务端投影给出，前端不按字符串长度猜。
import { useEffect, useRef, useState } from "react";
import i18n from "@/i18n";
import {
  isZcodeErrorCode,
  type ActionReceipt,
  type CapabilityFlag,
  type CapabilityView,
  type ControlAction,
  type PendingView,
  type QuestionView,
  type ZcodeErrorCode,
} from "@/lib/api/zcodeControl";
import InteractiveCard from "./InteractiveCard";

/**
 * 冻结原因码 → 本地化文案键（全码覆盖；`Record<ZcodeErrorCode, string>` 保证漏码
 * 在编译期报错）。码本身只进 `data-reason-code`，用户只看到这里的本地化文案。
 *
 * 词表放在本模块而不是面板：面板反过来消费本卡（`ZcodeControlPanel` → 本模块），
 * 词表若留在面板会形成循环 import。面板仍按原名 re-export，既有导入方不受影响。
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
  queue_refresh_required: "mobile.zcodeControl.reasonQueueRefreshRequired",
  // 队列仍有未人工核对的结果：先核对再继续/插队（绝不假成功）。
  queue_review_required: "mobile.zcodeControl.reasonQueueReviewRequired",
  attachment_missing: "mobile.zcodeControl.reasonAttachmentMissing",
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
export const STATUS_KEYS: Record<string, string> = {
  accepted: "mobile.zcodeControl.statusAccepted",
  confirmed: "mobile.zcodeControl.statusConfirmed",
  noop: "mobile.zcodeControl.statusNoop",
  rejected: "mobile.zcodeControl.statusRejected",
  send_failed: "mobile.zcodeControl.statusSendFailed",
  unknown: "mobile.zcodeControl.statusUnknown",
};

export interface PendingCardProps {
  pending: PendingView;
  sessionId: string;
  generation: number;
  capabilities: CapabilityView;
  /** 写面硬门原因（paired + ready）；null = 门开。陈旧投影绝不可点。 */
  gateReason: string | null;
  /** 全局动作在途（hook.submitting）：在途期间作答/审批面一并禁用。 */
  submitting: boolean;
  /**
   * 宿主会话投影修订号。**不再驱动解锁**（R3）：旁路 revision 推进既不丢草稿也不
   * 放开已受理的重复提交；解锁只由 interaction 身份/代际变化触发。
   */
  revision: number | null;
  submit: (action: ControlAction) => Promise<ActionReceipt>;
  /**
   * 「打开官方页面」入口（可选）：只读卡与截断审批卡需要有地方去核对全貌。
   *
   * 由面板传入已保存的官方链接（链接未知时不传 → 不挂入口，绝不伪造 URL）；
   * 卡片自己只调用回调，不拼链接、不代点官方页面。
   */
  onOpenOfficial?: () => void;
}

/**
 * 把宿主 `interactionId` 编码成可安全用作 DOM `id` / `aria-controls` /
 * `aria-labelledby` 的 IDREF 片段。
 *
 * 为什么需要：宿主 `interactionId` 是**外部输入**。直接内插时若含空格、`#`、`.` 等
 * 字符，IDREF 会按空格断词（`aria-controls="a b"` 指向两个不存在的 id），CSS 选择器
 * 也会误解析，读屏器的 tab↔tabpanel 关系随之失效。
 *
 * 规则：保留 `[A-Za-z0-9_-]`，其余字符逐字替换为 `-`，再附一段 32 位 FNV-1a 短哈希
 * （base36）——替换会丢信息（`"a b"` 与 `"a-b"` 得到同一 slug），哈希保证两个不同的
 * 宿主 id 仍得到不同的 DOM id。
 *
 * 只用于 DOM `id` 系属性；`data-interaction-id` 等**数据面**仍保留宿主原值。
 */
export function pendingDomId(prefix: string, interactionId: string): string {
  return `${prefix}-${interactionId.replace(/[^A-Za-z0-9_-]/g, "-")}-${domIdHash(interactionId)}`;
}

/** 32 位 FNV-1a → 定长 base36（7 字符）；纯函数、无依赖，仅用于防撞后缀。 */
function domIdHash(value: string): string {
  let hash = 0x811c9dc5;
  for (let index = 0; index < value.length; index += 1) {
    hash ^= value.charCodeAt(index);
    hash = Math.imul(hash, 0x01000193) >>> 0;
  }
  return hash.toString(36).padStart(7, "0").slice(-7);
}

/**
 * 问答卡是否**可远程作答**（三分语义；不是第二套权威规则）。
 *
 * - `answerable === false` → 服务端**显式**说不可答（plan_approval / MCP elicitation 等
 *   子型）→ 只读卡（F4 的安全目标：用户不会「选完才被 `needs_evidence` 拒」）。
 * - `answerable === true` → 可答。
 * - `answerable === undefined`（**字段缺失**：旧服务端 / 增量 merge 未带该字段）→
 *   回退**旧口径**判定：`kind==="userInput"` ∧ `toolName==="AskUserQuestion"` ∧
 *   `schemaInteraction` 合法（缺省或恰为 `AskUserQuestion`）⇒ 可答，否则只读。
 *
 * 兼容回退的理由：缺字段只说明服务端**未声明**，不等于「不可答」——若一律按不可答
 * 处理，同代 SSE 新推来的可答卡会被误判成只读（U7 回归）。但回退**只**在字段缺失时
 * 生效，服务端一旦**显式**给出 `false`，一律以服务端为准（绝不覆盖 F4 的安全目标）。
 *
 * 权威仍在服务端：Rust `pending::answerable_parts` 与应答闸共用同一纯判定，前端这里
 * 只是「字段没到」时的兼容分支，不另建一套权威规则。
 */
export function isPendingAnswerable(pending: PendingView): boolean {
  if (pending.answerable !== undefined) return pending.answerable;
  // 旧服务端/增量 merge 的帧可能连 `schemaInteraction` 都没有 → 显式放宽可空性。
  const schemaInteraction: string | null | undefined = pending.schemaInteraction;
  return (
    pending.kind === "userInput" &&
    pending.toolName === "AskUserQuestion" &&
    (schemaInteraction === null ||
      schemaInteraction === undefined ||
      schemaInteraction === "AskUserQuestion")
  );
}

/**
 * 本卡是否为**计划批准**（服务端 C1 只读标识；字段缺失 = 旧服务端/旧载荷 ⇒ false）。
 *
 * 为什么只消费字段、不在前端重推判据：计划批准的判据必须与服务端投影同源
 * （Rust `pending::plan_approval_parts`），否则会出现「服务端说是普通子型、前端说是
 * 计划批准」的文案漂移。这里**不**读 `toolName`/`schemaInteraction` 重推。
 *
 * `true` 时卡片显示“计划批准”专用面：服务端卡形状精确为唯一 `approve` 且
 * `single_select` 能力开闸时，提供一次原生批准；否则只读并说明原因。
 */
export function isPendingPlanApproval(pending: PendingView): boolean {
  return pending.planApproval === true;
}

/** 单张 pending 卡：只读呈现 + 宿主原值回传；关闭的能力面禁用并说明原因。 */
function ZcodePendingCard({
  pending,
  sessionId,
  generation,
  capabilities,
  gateReason,
  submitting,
  submit,
  onOpenOfficial,
}: PendingCardProps) {
  /** 每题的宿主选项原值集合：单选恒 0/1 个，多选按点击顺序累加（toggle 增删）。 */
  const [picked, setPicked] = useState<Record<string, string[]>>({});
  const [freeAnswers, setFreeAnswers] = useState<Record<string, string>>({});
  /** 当前显示第几题（0 基）；确认页由 `confirming` 表达，不改题序。 */
  const [step, setStep] = useState(0);
  /** 是否处于「确认全部答案」页：只有这里可以提交。 */
  const [confirming, setConfirming] = useState(false);
  /** 命令正文（`detail`）展开态：纯展示，不参与任何提交。 */
  const [detailOpen, setDetailOpen] = useState(false);
  /** 计划批准的「继续修改」意见；只存在本卡内存，换卡即清空。 */
  const [planFeedback, setPlanFeedback] = useState("");
  const snoozedRef = useRef(false);
  /** 同帧双击闸：一次用户动作只允许一个在途 POST（actionId 不重复生成）。 */
  const inFlightRef = useRef(false);
  /**
   * 敏感作答的即时值：失焦只清可见态，本次输入仍留在内存里以便提交/确认页判断
   * 「已填写」（只在本组件内存，不落 localStorage/sessionStorage）。
   */
  const [keptFree, setKeptFree] = useState<Record<string, string>>({});
  /**
   * 本卡已受理/已生效的锁定：只由 **interaction 身份/代际变化** 解除（R3）——
   * 宿主旁路 revision 推进不解锁，同一份答案绝不换新 actionId 重发。
   */
  const [settledState, setSettledState] = useState<"accepted" | "confirmed" | null>(null);
  const settledRef = useRef(false);
  /** snooze POST 在途：提交禁用并显示「正在暂停计时」（不静默吞掉用户点击）。 */
  const [snoozeInFlight, setSnoozeInFlight] = useState(false);

  /** 本卡身份 = 代际 + interaction：回执落地前必须仍是同一个身份。 */
  const identity = `${generation}:${pending.interactionId}`;
  const identityRef = useRef(identity);
  useEffect(() => {
    identityRef.current = identity;
  }, [identity]);

  // 换卡（interaction 被替换）或换代：草稿、翻页位置、展开态、受理锁一并清空。
  // 旁路 revision 推进不在此列——那只是宿主投影在动，用户的输入必须留着。
  //
  // 首跑必须跳过：挂载时的本地状态本来就是各 state 的初值（`picked={}`、`step=0`、
  // 未受理……），没有任何东西需要重置；而 passive effect 是在首帧**提交之后**才 flush 的，
  // 无条件首跑会把「首帧到 effect flush 之间」用户已经点下的选中态/输入抹掉，表现为
  // 「第一次点击没反应，再点一次才生效」。所以只记「已应用的身份键」（初始化为当前键），
  // 键相同直接 return，键真的变了才 reset。
  const appliedIdentityRef = useRef(identity);
  useEffect(() => {
    if (appliedIdentityRef.current === identity) return;
    appliedIdentityRef.current = identity;
    settledRef.current = false;
    inFlightRef.current = false;
    snoozedRef.current = false;
    setSettledState(null);
    setSnoozeInFlight(false);
    setPicked({});
    setFreeAnswers({});
    setKeptFree({});
    setStep(0);
    setConfirming(false);
    setDetailOpen(false);
    setPlanFeedback("");
  }, [identity]);

  /**
   * 一次动作一个在途槽：同帧重入/受理后重入一律丢弃，不产生第二个 actionId。
   * 回执只在**身份未变**时落地：旧 interaction/旧代际的迟到结果绝不污染新卡。
   */
  const submitOnce = (action: ControlAction, onSettled?: () => void): boolean => {
    if (inFlightRef.current || settledRef.current) return false;
    inFlightRef.current = true;
    const token = identityRef.current;
    void submit(action)
      .then((receipt) => {
        if (identityRef.current !== token) return;
        // snooze 只暂停计时，不锁卡；作答/审批的受理/生效才进入锁定态。
        if (action.kind === "snooze") return;
        if (receipt.state === "accepted" || receipt.state === "confirmed") {
          settledRef.current = true;
          setSettledState(receipt.state);
        }
      })
      .catch(() => {})
      .finally(() => {
        // 换卡后的迟到收尾：状态已由上面的重置 effect 归零，这里不再动。
        if (identityRef.current !== token) return;
        inFlightRef.current = false;
        onSettled?.();
      });
    return true;
  };

  const isPermission = pending.kind === "permission";
  const isUserInput = pending.kind === "userInput";
  /** 服务端 C1 只读标识（字段缺失 = 旧服务端 ⇒ false，不按 toolName 猜）。 */
  const planApproval = isPendingPlanApproval(pending);
  /** 计划批准只读面：标识只在 userInput 卡上消费（标题/原因/只读分支同一判据）。 */
  const planApprovalReadonly = isUserInput && planApproval;
  /**
   * 只读问答卡：宿主把 `plan_approval` / MCP elicitation 等子型也投影成 `userInput`，
   * 但它们**不可远程代答**（服务端用与 `build_user_answer` 同一个纯判定给出
   * `answerable=false`）。此处绝不渲染选项/提交，从结构上消除「选完才被 needs_evidence 拒」。
   * 字段**缺失**时按旧口径兼容回退（见 [`isPendingAnswerable`]），不把「未声明」当「不可答」。
   * 计划批准标识 `true` 时恒为只读（C1 没有批准动作，即便载荷自相矛盾也不放开）。
   */
  const readOnlyInput = isUserInput && (planApproval || !isPendingAnswerable(pending));
  /** 审批正文被服务端按上限截断：看不到全貌 → 不提供盲批（允许一次）。 */
  const detailTruncated = isPermission && pending.detailTruncated;
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
  const capabilityBlocked: CapabilityFlag | null = readOnlyInput
    ? // 只读卡不谈「能力缺失」：原因是类型本身不可代答，不是某一项旗标被关。
      null
    : isPermission
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
  /** 计划批准 C2：必须恰有一个 question + 一个宿主原值 approve 选项。 */
  const planApprovalShapeOk =
    pending.questions.length === 1 &&
    pending.questions[0].options.length === 1 &&
    pending.questions[0].options[0].value === "approve";
  const planApprovalBlockedReason = planApprovalReadonly
    ? gateReason !== null
      ? gateReason
      : !capabilities.singleSelect.enabled
        ? (capabilities.singleSelect.reasonCode ?? "needs_evidence")
        : !planApprovalShapeOk
          ? "invalid_action"
          : null
    : null;
  const canApprovePlan =
    planApprovalReadonly &&
    planApprovalBlockedReason === null &&
    settledState === null &&
    !submitting;
  /** 反馈与批准共享同一 interaction：只改 answer content，不另开动作。 */
  const planFeedbackBlockedReason = planApprovalReadonly
    ? gateReason !== null
      ? gateReason
      : !capabilities.singleSelect.enabled
        ? (capabilities.singleSelect.reasonCode ?? "needs_evidence")
        : !capabilities.freeText.enabled
          ? (capabilities.freeText.reasonCode ?? "needs_evidence")
          : !planApprovalShapeOk
            ? "invalid_action"
            : null
    : null;
  const canFeedbackPlan =
    planApprovalReadonly &&
    planFeedbackBlockedReason === null &&
    planFeedback.trim().length > 0 &&
    settledState === null &&
    !submitting;
  // 按码如实呈现：每个冻结原因码都有独立 zh/en 文案（表外码才用中性兜底）；
  // 仅当服务端未给原因码时退回「该作答形态暂未开放」的上下文说明。
  const blockedText =
    blocked?.reasonCode != null
      ? zcodeReasonText(blocked.reasonCode)
      : i18n.t("mobile.zcodeControl.pendingBlocked");

  const total = pending.questions.length;
  // 宿主同身份换题（题目数变少）时不让题序越界；不猜内容，只夹取范围。
  const current = Math.min(step, Math.max(total - 1, 0));
  const question = pending.questions[current] ?? null;

  /** 单题的当前答案：自由文本优先（敏感值取失焦后的内存副本），否则取选中项。 */
  const answerOf = (item: QuestionView): { values: string[]; freeText: string | null } => {
    const visible = freeAnswers[item.questionText] ?? "";
    const kept = pending.sensitive ? (keptFree[item.questionText] ?? "") : "";
    const free = visible.length > 0 ? visible : kept;
    if (free.length > 0) return { values: [], freeText: free };
    return { values: picked[item.questionText] ?? [], freeText: null };
  };
  /** 未作答的题序（0 基）：既无选中项也无自由文本。 */
  const missing = pending.questions
    .map((item, index) =>
      answerOf(item).freeText === null && answerOf(item).values.length === 0 ? index : -1
    )
    .filter((index) => index >= 0);

  /** 确认页的一句答案（只读展示；提交值另取原始 value）。 */
  const describeAnswer = (item: QuestionView): string => {
    const answer = answerOf(item);
    if (answer.freeText !== null) {
      // 敏感值连确认页也不回显明文（只说已填写）。
      return pending.sensitive
        ? i18n.t("mobile.zcodeControl.pendingSensitiveHidden")
        : answer.freeText;
    }
    if (answer.values.length === 0) return i18n.t("mobile.zcodeControl.pendingAnswerEmpty");
    // 分隔符走已入库文案键（zh「、」/ en ", "），与 pendingMissing 的列表口径一致。
    return answer.values
      .map((value) => item.options.find((option) => option.value === value)?.label ?? value)
      .join(i18n.t("mobile.zcodeControl.pendingListSeparator"));
  };

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

  /**
   * 最终提交：只在确认页、答完整、门开且未受理时发**一次** answer。
   * answers 按原始题序，题干与选项 value 都是宿主原值。
   */
  const submitAnswer = () => {
    // 空题卡（宿主形状异常/上游漏拒）绝不上行 `answers: []`：空答案 ≠ 用户同意继续。
    if (pending.questions.length === 0) return;
    if (blocked !== null || submitting || settledRef.current || inFlightRef.current) return;
    if (!confirming || missing.length > 0) return;
    const answers = pending.questions.map((item) => {
      const answer = answerOf(item);
      return {
        questionText: item.questionText,
        selectedValues: answer.freeText !== null ? [] : answer.values,
        freeText: answer.freeText,
      };
    });
    submitOnce({
      kind: "answer",
      sessionId,
      generation,
      interactionId: pending.interactionId,
      answers,
    });
  };

  const answerDisabled = blocked !== null || submitting || settledState !== null;
  const submitDisabled = answerDisabled || snoozeInFlight || !confirming || missing.length > 0;
  /** 命令详情锚点：同一页可能有多张卡，id 必须带 interaction 身份（且经 DOM 安全编码）。 */
  const detailId = pendingDomId("zcode-pending-detail", pending.interactionId);

  /** 「打开官方页面」入口：只在面板提供了已保存链接时出现（绝不伪造 URL）。 */
  const officialEntry =
    onOpenOfficial === undefined ? null : (
      <button
        type="button"
        data-testid="zcode-pending-open-official"
        onClick={onOpenOfficial}
        className="mt-1 rounded-full border border-[var(--cb)] px-3 py-1 text-xs text-[var(--mut)]"
      >
        {i18n.t("mobile.composer.zcodeOpenOfficial")}
      </button>
    );

  const footer = (
    <>
      {readOnlyInput && planApprovalReadonly && (
        <>
          {planApprovalBlockedReason !== null && (
            <p
              data-testid="zcode-pending-plan-approval-reason"
              data-reason-code={planApprovalBlockedReason}
              className="mt-1 text-xs text-[var(--mut)]"
            >
              {zcodeReasonText(planApprovalBlockedReason)}
            </p>
          )}
          {canApprovePlan && (
            <button
              type="button"
              data-testid="zcode-plan-approval-approve"
              disabled={submitting}
              onClick={() => {
                if (planApprovalBlockedReason !== null || submitting || settledRef.current) return;
                submitOnce({
                  kind: "planApproval",
                  sessionId,
                  generation,
                  interactionId: pending.interactionId,
                  decision: "approve",
                });
              }}
              className="mt-2 rounded-full bg-[var(--btnp)]/10 px-4 py-1 text-sm text-[var(--tx)] disabled:opacity-40"
            >
              {i18n.t("mobile.zcodeControl.pendingPlanApprovalApprove")}
            </button>
          )}
          {planApprovalReadonly && pending.questions[0]?.freeTextAllowed === true && (
            <div className="mt-3">
              <textarea
                data-testid="zcode-plan-approval-feedback"
                value={planFeedback}
                maxLength={4096}
                rows={2}
                placeholder={i18n.t("mobile.zcodeControl.pendingPlanApprovalFeedbackPlaceholder")}
                onChange={(event) => setPlanFeedback(event.target.value)}
                disabled={submitting || settledState !== null}
                className="w-full rounded-xl border border-[var(--cb)] bg-transparent px-3 py-2 text-sm text-[var(--tx)] disabled:opacity-40"
              />
              <button
                type="button"
                data-testid="zcode-plan-approval-feedback-submit"
                disabled={!canFeedbackPlan}
                onClick={() => {
                  if (planFeedbackBlockedReason !== null || submitting || settledRef.current)
                    return;
                  const text = planFeedback.trim();
                  if (!text) return;
                  submitOnce({
                    kind: "planApproval",
                    sessionId,
                    generation,
                    interactionId: pending.interactionId,
                    decision: "feedback",
                    feedback: text,
                  });
                }}
                className="mt-2 rounded-full border border-[var(--cb)] px-4 py-1 text-sm text-[var(--tx)] disabled:opacity-40"
              >
                {i18n.t("mobile.zcodeControl.pendingPlanApprovalFeedbackSubmit")}
              </button>
              {planFeedbackBlockedReason !== null && (
                <p
                  data-testid="zcode-pending-plan-feedback-reason"
                  data-reason-code={planFeedbackBlockedReason}
                  className="mt-1 text-xs text-[var(--mut)]"
                >
                  {zcodeReasonText(planFeedbackBlockedReason)}
                </p>
              )}
            </div>
          )}
          {settledState !== null && (
            <p
              data-testid="zcode-pending-plan-approval-notice"
              data-state={settledState}
              className="mt-1 text-xs text-[var(--mut)]"
            >
              {i18n.t(STATUS_KEYS[settledState])}
            </p>
          )}
          {officialEntry}
        </>
      )}
      {readOnlyInput && !planApprovalReadonly && (
        <>
          <p
            data-testid="zcode-pending-readonly-reason"
            data-readonly-kind="unsupported"
            className="mt-1 text-xs text-[var(--mut)]"
          >
            {i18n.t("mobile.zcodeControl.pendingReadonlyReason")}
          </p>
          {officialEntry}
        </>
      )}
      {!readOnlyInput && blocked !== null && (
        <p
          data-testid="zcode-pending-reason"
          data-reason-code={blocked.reasonCode ?? "unknown"}
          className="mt-1 text-xs text-[var(--mut)]"
        >
          {blockedText}
        </p>
      )}
      {!readOnlyInput && blocked === null && settledState !== null && (
        <p
          data-testid="zcode-pending-notice"
          data-state={settledState}
          className="mt-1 text-xs text-[var(--mut)]"
        >
          {i18n.t(STATUS_KEYS[settledState])}
        </p>
      )}
      {!readOnlyInput && blocked === null && settledState === null && snoozeInFlight && (
        <p
          data-testid="zcode-pending-notice"
          data-state="snoozing"
          className="mt-1 text-xs text-[var(--mut)]"
        >
          {i18n.t("mobile.zcodeControl.pendingSnoozing")}
        </p>
      )}
      {/* 正文被截断：必须在选项上方就说清「不是全貌」，且只留拒绝/官方页面。 */}
      {detailTruncated && (
        <>
          <p
            data-testid="zcode-pending-detail-truncated"
            className="mt-1 text-xs text-[var(--mut)]"
          >
            {i18n.t("mobile.zcodeControl.pendingDetailTruncated")}
          </p>
          {officialEntry}
        </>
      )}
      {!readOnlyInput && isUserInput && confirming && missing.length > 0 && (
        <p data-testid="zcode-pending-missing" className="mt-1 text-xs text-[var(--mut)]">
          {i18n.t("mobile.zcodeControl.pendingMissing", {
            list: missing
              .map((index) =>
                i18n.t("mobile.zcodeControl.pendingMissingItem", { index: index + 1 })
              )
              .join(i18n.t("mobile.zcodeControl.pendingListSeparator")),
          })}
        </p>
      )}
      {!readOnlyInput && isUserInput && (
        <div className="mt-2 flex items-center gap-2">
          {confirming ? (
            <button
              type="button"
              data-testid="zcode-pending-review-back"
              onClick={() => setConfirming(false)}
              className="rounded-full border border-[var(--cb)] px-3 py-1 text-sm text-[var(--tx)]"
            >
              {i18n.t("mobile.zcodeControl.pendingReviewBack")}
            </button>
          ) : (
            <>
              <button
                type="button"
                data-testid="zcode-pending-prev"
                disabled={current === 0}
                onClick={() => setStep(Math.max(current - 1, 0))}
                className="rounded-full border border-[var(--cb)] px-3 py-1 text-sm text-[var(--tx)] disabled:opacity-40"
              >
                {i18n.t("mobile.zcodeControl.pendingPrev")}
              </button>
              <button
                type="button"
                data-testid="zcode-pending-next"
                onClick={() => {
                  if (current < total - 1) setStep(current + 1);
                  else setConfirming(true);
                }}
                className="rounded-full border border-[var(--cb)] px-3 py-1 text-sm text-[var(--tx)]"
              >
                {i18n.t(
                  current < total - 1
                    ? "mobile.zcodeControl.pendingNext"
                    : "mobile.zcodeControl.pendingReview"
                )}
              </button>
            </>
          )}
          <button
            type="button"
            data-testid="zcode-pending-submit"
            disabled={submitDisabled}
            onClick={submitAnswer}
            className="rounded-full bg-[var(--btnp)]/10 px-4 py-1 text-sm text-[var(--tx)] disabled:opacity-40"
          >
            {i18n.t("mobile.zcodeControl.submit")}
          </button>
        </div>
      )}
    </>
  );

  return (
    <div
      data-testid="zcode-pending-card"
      data-interaction-id={pending.interactionId}
      data-kind={pending.kind}
      data-plan-approval={planApprovalReadonly ? "true" : "false"}
    >
      <InteractiveCard
        tone={isPermission ? "approve" : "question"}
        mode={pending.kind}
        testId="zcode-pending-card-body"
        title={i18n.t(
          isPermission
            ? "mobile.zcodeControl.pendingPermissionTitle"
            : planApprovalReadonly
              ? "mobile.zcodeControl.pendingPlanApprovalTitle"
              : "mobile.zcodeControl.pendingQuestionTitle"
        )}
        footer={footer}
      >
        {pending.toolName !== null && (
          <p className="text-xs text-[var(--mut)]">{pending.toolName}</p>
        )}
        {pending.summary !== null && <p className="text-sm text-[var(--tx)]">{pending.summary}</p>}
        {/* 命令正文（官方 payload.detail）：默认折叠，展开只读，不参与提交。 */}
        {(isPermission || planApprovalReadonly) && pending.detail !== null && (
          <div className="mt-1">
            <button
              type="button"
              data-testid="zcode-pending-detail-toggle"
              aria-expanded={detailOpen}
              // aria-controls 只在正文真的渲染时输出：折叠状态下指向未渲染元素
              // 属 ARIA 违规（悬空引用）。
              aria-controls={detailOpen ? detailId : undefined}
              onClick={() => setDetailOpen((open) => !open)}
              className="text-xs text-[var(--mut)] underline"
            >
              {i18n.t(
                detailOpen
                  ? "mobile.zcodeControl.pendingDetailHide"
                  : "mobile.zcodeControl.pendingDetailShow"
              )}
            </button>
            {detailOpen && (
              <pre
                id={detailId}
                data-testid="zcode-pending-detail"
                aria-label={i18n.t("mobile.zcodeControl.pendingDetailAria")}
                className="mt-1 max-h-48 overflow-auto rounded-lg border border-[var(--cb)] px-2 py-1 text-xs break-all whitespace-pre-wrap text-[var(--tx)]"
              >
                {pending.detail}
              </pre>
            )}
          </div>
        )}
        {isPermission && permissionOptions.length > 0 && (
          <div
            data-testid="zcode-permission-options"
            data-layout="vertical"
            className="mt-1 flex flex-col gap-1"
          >
            {permissionOptions.map((option) => (
              <button
                key={option.optionId}
                type="button"
                data-testid={`zcode-permission-option-${option.optionId}`}
                // 截断正文撤掉「允许一次」：看不到全貌就绝不提供盲批；「拒绝」仍可用。
                disabled={answerDisabled || (detailTruncated && option.kind === "allowOnce")}
                onClick={() => {
                  if (blocked !== null || submitting || settledRef.current) return;
                  if (detailTruncated && option.kind === "allowOnce") return;
                  submitOnce({
                    kind: "permission",
                    sessionId,
                    generation,
                    interactionId: pending.interactionId,
                    optionId: option.optionId,
                  });
                }}
                className="w-full rounded-lg border border-[var(--cb)] px-3 py-1 text-left text-sm text-[var(--tx)] disabled:opacity-40"
              >
                {option.label}
              </button>
            ))}
          </div>
        )}
        {isUserInput && readOnlyInput && pending.questions.length > 0 && (
          // 只读问答子型：只展示问了什么，不给任何可提交控件。
          <ul data-testid="zcode-pending-readonly-questions" className="mt-1 flex flex-col gap-1">
            {pending.questions.map((item, index) => (
              <li key={`${index}-${item.questionText}`} className="text-sm text-[var(--tx)]">
                {item.questionText}
              </li>
            ))}
          </ul>
        )}
        {isUserInput && !readOnlyInput && confirming && (
          <div data-testid="zcode-pending-confirm" className="mt-1">
            <p className="text-sm font-semibold text-[var(--tx)]">
              {i18n.t("mobile.zcodeControl.pendingReviewTitle")}
            </p>
            <ol className="mt-1 flex flex-col gap-1">
              {pending.questions.map((item, index) => (
                <li
                  key={`${index}-${item.questionText}`}
                  data-testid={`zcode-pending-confirm-item-${index}`}
                  className="text-sm text-[var(--tx)]"
                >
                  <p>{item.questionText}</p>
                  <p
                    data-testid={`zcode-pending-confirm-answer-${index}`}
                    className="text-xs text-[var(--mut)]"
                  >
                    {describeAnswer(item)}
                  </p>
                </li>
              ))}
            </ol>
          </div>
        )}
        {isUserInput && !readOnlyInput && !confirming && question !== null && (
          <div className="mt-1">
            <p data-testid="zcode-pending-step" className="text-xs text-[var(--mut)]">
              {i18n.t("mobile.zcodeControl.pendingStep", {
                current: current + 1,
                total,
              })}
            </p>
            <p className="text-sm text-[var(--tx)]">{question.header}</p>
            <p className="text-sm text-[var(--tx)]">{question.questionText}</p>
            {question.options.length > 0 && (
              <div
                data-testid="zcode-answer-options"
                data-layout="vertical"
                className="mt-1 flex flex-col gap-1"
              >
                {question.options.map((option, index) => {
                  const flag = flagFor(question);
                  const selected = picked[question.questionText]?.includes(option.value) === true;
                  return (
                    <button
                      key={option.value}
                      type="button"
                      data-testid={`zcode-pending-option-${index}`}
                      data-value={option.value}
                      aria-pressed={selected}
                      disabled={answerDisabled || !flag.enabled}
                      onClick={() => {
                        if (blocked !== null || submitting || settledRef.current || !flag.enabled) {
                          return;
                        }
                        setPicked((prev) => {
                          const values = prev[question.questionText] ?? [];
                          // 多选 = toggle（增删，保留点击顺序）；单选 = 替换为本次点击项。
                          const next = question.multiSelect
                            ? values.includes(option.value)
                              ? values.filter((value) => value !== option.value)
                              : [...values, option.value]
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
                        setKeptFree((prev) => {
                          if (!(question.questionText in prev)) return prev;
                          const next = { ...prev };
                          delete next[question.questionText];
                          return next;
                        });
                        snoozeOnce();
                      }}
                      className={`w-full rounded-lg border px-3 py-2 text-left text-sm text-[var(--tx)] disabled:opacity-40 ${
                        selected ? "border-[var(--btnp)] bg-[var(--btnp)]/10" : "border-[var(--cb)]"
                      }`}
                    >
                      {option.label}
                    </button>
                  );
                })}
              </div>
            )}
            {question.freeTextAllowed && (
              <input
                type={pending.sensitive ? "password" : "text"}
                data-testid={pending.sensitive ? "zcode-pending-sensitive-input" : undefined}
                value={freeAnswers[question.questionText] ?? ""}
                disabled={answerDisabled || !capabilities.freeText.enabled}
                onChange={(event) => {
                  if (blocked !== null || !capabilities.freeText.enabled) return;
                  const value = event.target.value;
                  // 敏感作答只存本次挂载的组件状态，不落任何草稿缓存。
                  if (pending.sensitive) {
                    setKeptFree((prev) => {
                      const next = { ...prev };
                      if (value.length > 0) next[question.questionText] = value;
                      else delete next[question.questionText];
                      return next;
                    });
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
                  // 敏感输入失焦即清可见值（防肩窥/留屏）；提交/确认仍取 keptFree。
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
        )}
      </InteractiveCard>
    </div>
  );
}

export { ZcodePendingCard };
export default ZcodePendingCard;
