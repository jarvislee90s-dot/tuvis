// OpenCode 原生交互区（计划任务 9/10）：原生视图 + 待答表单与审批 + 原生 inbox + 原生发送。
//
// 任务 10 追加的两条入口（都只走 `POST /opencode/actions`，**不复用**旧终端审批/问答端点）：
// - 待答表单：挂 `OpenCodeQuestionGroupCard`（组内草稿、必答校验、冻结提交都在那张卡里）；
//   能力 `answer_group` 未放行时**不挂可写卡**，只留只读说明与受控原因词；
// - 审批：`decide` 只发 `once`/`reject`，且只发后端在 `decisions` 里列出的词；**点击决定本身
//   就是明确确认**（不加统一二次确认对话框）。结论只以原生投影为准，不判断是哪一端先完成。
//
// 与旧终端写链的边界（宪法「动作已发但结果不明，不切到按键路径重发」）：
// - 本组件**只**走 `POST /opencode/actions`（唯一原生写入口）；不调用任何旧终端注入端点
//   （session-send / approve / question-answer / mode-switch 一律零调用）；
// - 旧写控件（QuestionCard / ApproveCard / ModeBar）在原生路由下**不挂载**（由
//   SessionDetail 按路由门控），这里只**如实说明**本批未接入的能力，不用旧按键顶替；
// - `unconfirmed` / `dispatching`（以及网络层未知）只给**核对**（只读 verify），
//   绝不出现「重发」按钮；`sessionStorage` 只有两类恢复锚，值都只含 id：
//   发送锚（单个 actionId）与动作锚列表（`{actionId,kind,targetId,at}`），**不存答案/正文/附件/绑定凭据**。
//
// N4 拆分（已受理 ≠ 结果未知）：
// - `acknowledged` **且带精确 nativeId**（后端 send_outcome/verify_send 在精确 inbox id
//   命中时返回，优先复用）= **已证受理**：本条不再算「结果未知」——单一发送锚解除
//   （Unknown 锚只保护它自己那条），转成锚列表里的 `send` 行（「已受理」区），
//   每行默认只保留**只读核对**入口；**当前有效绑定上的已证受理允许新意图**
//   （忙态可显式插队，新动作 ID）。缺 nativeId 的 acknowledged 一律保守按未知处理。
// - M3（P1-c）补口：已受理行的锁定代际经账本宣告死亡（`runtimeDead` 呈现面：登记
//   文件 + 进程事实无法证明「行代际 = 当前活代际」，与 confirm-unknown 权威闸同一
//   判定惯例）⇒ 该行补挂既有「确认终态未知并解锁重选」出口（普通与 steer 一致）；
//   **非死代际只有核对按钮**。确认后行转 `unknown_confirmed` 归档态：锚行移除、
//   未决谓词解除、重选可用，全程零重投；权威闸（fail-closed 409）仍在后端端点。
// - `unconfirmed` / `dispatching` / 网络层未知 ⇒ 维持该发送锚的保护（锁发送，
//   出口只有核对 / 明确放弃 / 确认终态未知）；恢复出来的旧锚在读到账本证据前保守处理。
//
// 状态机（点击发送 → 收敛）：
//   idle → sending（正文/附件在点击帧冻结）→
//     confirmed | settled        → 已确认/已定案（清空「点击时那一份」冻结稿）
//     acknowledged+nativeId      → 已证受理（回执「OpenCode 已受理，等待处理」；
//                                  冻结稿同 queued 语义消费；不锁新发送）
//     acknowledged（无 nativeId）| dispatching | unconfirmed → 未确认（保留草稿与恢复锚；
//                                  发送键**禁用**，出口：只读核对 / 明确放弃本次核对）
//     not_sent | not_accepted    → 零写出（保留草稿，可编辑后重新发送 = 新动作）
//     受控 4xx/409               → 零写出，按受控码文案（恢复锚清除）
//     网络层异常                 → 结果不明（与未确认同路：锚保留 + 禁用发送 + 两个出口）
//
// 「不确定就锁住写」的理由：若未确认时仍可点发送，同一段正文会以**新 actionId** 再发一次
// ——后端按 actionId 去重挡不住它，等于把「不重发」的文案写在了一个可重发的按钮旁边。
//
// 锁写的判据是**锚本身**（`unresolved = anchor !== null`），不是回执种类：核对失败只是一次
// 「没读到」，它既不解除锁、也不能把出口块藏起来（R-1）。
import {
  useCallback,
  useEffect,
  useLayoutEffect,
  useMemo,
  useRef,
  useState,
  type ReactNode,
} from "react";
import { createPortal } from "react-dom";
import { ChevronDown, ChevronRight, CircleHelp, Plus } from "lucide-react";
import i18n from "@/i18n";
import { collapsedLabel, isProcessKind, type NativeProcessFold } from "./message-fold";
import {
  ApiError,
  fetchFile,
  uploadAttachment,
  type FilePayload,
  type SessionMessage,
} from "./api";
import { formatBytes } from "./board-logic";
import { useMessageRenderers } from "./message-render";
import useOpenCodeNative from "./useOpenCodeNative";
import OpenCodeQuestionGroupCard from "./OpenCodeQuestionGroupCard";
import OpenCodeNativeQueue from "./OpenCodeNativeQueue";
import {
  ACTION_ANCHOR_CHANGED_EVENT,
  ANCHOR_AUTO_REREAD_MS,
  addActionAnchor,
  hasUnresolvedAction,
  rememberActionBinding,
  readActionAnchors,
  removeActionAnchor,
  mergeServerActionAnchors,
  type OpenCodeActionAnchor,
  type OpenCodeRecoveryRow,
} from "./opencodeActionAnchors";
import { fetchOpenCodeSessionActions, type OpenCodeActionListEntry } from "./opencodeNativeApi";
import {
  clientMessageIdOf,
  enterOpenCodeNative,
  fetchFrozenOpenCodePendingCount,
  fetchNativeFormResult,
  fetchOpenCodeAgents,
  fetchOpenCodeView,
  isAbortError,
  isConclusiveState,
  isDefiniteRefusal,
  BINDING_DEAD_RESELECT_CODES,
  isOwnershipRefusalCode,
  nativeErrorCode,
  nativeErrorKey,
  newNativeActionId,
  NATIVE_ATTACHMENT_MAX_BYTES,
  OC_RESPONSE_INVALID_MARKER,
  confirmOpenCodeActionUnknown,
  previewOpenCodeCandidate,
  readOpenCodeAction,
  returnOpenCodeRouteToLegacy,
  submitOpenCodeAction,
  OPENCODE_STOP_POST_TIMEOUT_MS,
  OPENCODE_RESELECT_POST_TIMEOUT_MS,
  readOpenCodeReselectReceipt,
  newOpenCodeCreateRequestId,
  unverifiableActionsOf,
  viewErrorRouteVersion,
  verifyOpenCodeActionWithTimeout,
  type ActionView,
  type DeliveryKind,
  type FormResultStatus,
  type InboxView,
  type NativeFileView,
  type NativeAgentView,
  type NativeFormResult,
  type PendingView,
  type PublicNativeView,
} from "./opencodeNativeApi";

/** 恢复锚的 sessionStorage 键前缀：值**只有** actionId（绝不含正文/答案/令牌）。 */
const ANCHOR_PREFIX = "tuvis.opencodeNative.action.";

function anchorKey(sessionId: string): string {
  return `${ANCHOR_PREFIX}${sessionId}`;
}

/** 表单 id 的 sessionStorage 键前缀：值**只有** formId 数组（R5b：刷新后按 id 重读
 *  官方结果；**不存答案**——答案每次现读，不落任何持久存储）。 */
const FORM_IDS_PREFIX = "tuvis.opencodeNative.formIds.";

/** 最多记住多少个 formId（刷新恢复锚的容量上限；超出丢最旧的）。 */
const MAX_TRACKED_FORM_IDS = 10;

function formIdsKey(sessionId: string, binding: string): string {
  return `${FORM_IDS_PREFIX}${sessionId}.${encodeURIComponent(binding)}`;
}

/** 读取当前目标记住的表单 id；旧版无目标归属的索引保留但不猜测归属。 */
function readTrackedFormIds(sessionId: string, binding: string): string[] {
  try {
    const raw = globalThis.sessionStorage?.getItem(formIdsKey(sessionId, binding));
    if (raw === null || raw === undefined) return [];
    const parsed: unknown = JSON.parse(raw);
    if (!Array.isArray(parsed)) return [];
    return parsed
      .filter((id): id is string => typeof id === "string" && id.trim() !== "")
      .slice(-MAX_TRACKED_FORM_IDS);
  } catch {
    return [];
  }
}

/** 只把 formId 写进 sessionStorage；配额/隐私模式失败只影响刷新后的重读。 */
function writeTrackedFormIds(sessionId: string, binding: string, ids: string[]): void {
  try {
    globalThis.sessionStorage?.setItem(
      formIdsKey(sessionId, binding),
      JSON.stringify(ids.slice(-MAX_TRACKED_FORM_IDS))
    );
  } catch {
    /* 不是正确性依赖：本次结果仍由内存态展示 */
  }
}

/** 一条表单结果的内存态：加载中 / 已读到 / 未读到（未读到时 id 保留，可再次尝试）。 */
type FormResultState =
  { kind: "loading" } | { kind: "ok"; result: NativeFormResult } | { kind: "error" };

/** 原生窗口的正文渲染参数（模块级常量：不随渲染新建，避免 renderers 每次重建）。 */
const NO_FILES: Set<string> = new Set();
const NOOP_OPEN_FILE = (_path: string): void => {};

/** 读取本会话的恢复锚（无则 null；sessionStorage 不可用也按无处理）。 */
function readAnchor(sessionId: string): string | null {
  try {
    return globalThis.sessionStorage?.getItem(anchorKey(sessionId)) ?? null;
  } catch {
    return null;
  }
}

function writeAnchor(sessionId: string, actionId: string | null): void {
  try {
    const key = anchorKey(sessionId);
    if (actionId === null) globalThis.sessionStorage?.removeItem(key);
    else globalThis.sessionStorage?.setItem(key, actionId);
  } catch {
    /* 隐私模式/配额：锚不是正确性依赖，失败仅退化为「刷新后无核对入口」 */
  }
}

/** POSIX shell 单引号；重连命令里有目录，路径可含空格/元字符。 */
function shellQuote(value: string): string {
  return `'${value.replace(/'/g, "'\\''")}'`;
}

/** 面板可复制的桌面重连命令：密码在桌面本机临时读取，不写入页面/数据库。 */
function desktopReconnectCommand(view: PublicNativeView): string {
  return [
    `OPENCODE_PASSWORD="$(python3 -c 'import json,os;print(json.load(open(os.path.expanduser("~/.local/state/opencode/service.json")))["password"])')"`,
    "opencode",
    "--server",
    shellQuote(view.serviceUrl ?? ""),
    "--session",
    shellQuote(view.sessionId),
    shellQuote(view.directory ?? ""),
  ].join(" ");
}

/** 受控码 → 用户文案。三级回落，**任何一级都不上屏机器串**：
 *  1) 已登记的受控码 → 本地化文案；
 *  2) 后端错误体的 `reason`（固定文案，可能已是中文）→ 原样展示；
 *  3) 都没有 → 通用句（不拼状态码、不拼 endpoint）。 */
function errorCopy(e: ApiError): string {
  if (e.status === null) return i18n.t("mobile.openCodeNative.errors.transportFailed");
  const key = nativeErrorKey(nativeErrorCode(e));
  if (key !== null) return i18n.t(key);
  const reason = e.data?.reason;
  if (typeof reason === "string" && reason.trim() !== "") return reason;
  return i18n.t("mobile.openCodeNative.errors.generic");
}

/** 发送回执（互斥槽：只保留最近一次用户动作的结论）。
 *  `unknownConfirmed`（patch3 D2）= 用户已显式确认「终态未知、不再等待」：不是「没发出」，
 *  只是本端停止等待、可重新选择目标。
 *  `acknowledged`（N4）= **已证受理**（精确 nativeId 命中）：与「结果未知」不同事实，
 *  不锁新发送；持久核对入口在锚列表的 send 行（回执槽只是最近一次的短注）。 */
type Receipt =
  | { kind: "queued" }
  | { kind: "acknowledged" }
  | { kind: "confirmed" }
  | { kind: "settled" }
  | {
      kind: "unknown";
      actionId: string;
      /** 服务端受控码（网络层异常为 null） */ code?: string | null;
    }
  | { kind: "notSent"; code: string | null }
  | { kind: "notAccepted"; code: string | null }
  /** G4 patch7：受控拒绝带原码（只用于选「下一步」出口——绑定死亡族挂重选入口；
   *  文案仍走 errorCopy 的本地化短语，机器串不上屏）。 */
  | { kind: "rejected"; text: string; code?: string | null }
  | { kind: "abandoned" }
  | { kind: "unknownConfirmed" };

/** 一次发送的冻结意图：正文 + **本次实际带上**的附件 id（M8：只清这些，
 *  发送在途新加的附件不属于本次意图，必须留下）。 */
interface SendIntent {
  draftRevision: number;
  text: string;
  attachmentIds: string[];
}

/** 审批点击后的本行结论（requestId → 回执）：即时反馈，权威结论仍看原生投影。 */
type DecisionReceipt =
  | { kind: "submitted" }
  | { kind: "settled" }
  | { kind: "unknown"; actionId: string; code: string | null }
  | { kind: "rejected"; text: string; code?: string | null };

/** 允许的审批决定词：T7 未完成前只展示 once/reject；always 永久授权不开放。 */
const DECISION_WORDS = ["once", "reject"] as const;
type DecisionWord = (typeof DECISION_WORDS)[number];

/** 附件上传失败的**受控码**：只用于选本地化文案；后端原文/机器串绝不上屏。
 *  `upload_gate_refused` = 上传口被归属写门提前拒绝（H03：过门失败零本地落盘），
 *  实际文案按 {@link NativeAttachment.refusalCode} 映射门码专属短语。 */
type NativeAttachmentErrorCode =
  | "too_large"
  | "message_limit"
  | "upload_rejected"
  | "upload_network"
  | "upload_failed"
  | "upload_interrupted"
  | "upload_gate_refused";

/** 待发附件（原生路径）：只保留**已落盘**的绝对路径（后端要求裸绝对路径，自己转 file://）。 */
interface NativeAttachment {
  id: string;
  name: string;
  status: "uploading" | "ready" | "failed";
  path?: string;
  /** 失败原因的受控码（预检超限/空回执/网络/服务端）：渲染本地化文案。 */
  errorCode?: NativeAttachmentErrorCode;
  /** 上传被写门拒绝时的**门受控码**（OC_GATE_* / OC_INSTANCE_*）：chip 据此映射专属文案。 */
  refusalCode?: string;
  /** 上传成功但本地排除规则未确认追加：如实展示，附件照常可用。 */
  excludeWarning?: string;
}

/** 受控码 → i18n 键：失败行与禁发提示共用同一套文案。 */
function attachmentErrorKey(code: NativeAttachmentErrorCode): string {
  switch (code) {
    case "message_limit":
      return "mobile.openCodeNative.composer.attachmentFileLimit";
    case "too_large":
      return "mobile.openCodeNative.composer.attachmentTooLarge";
    case "upload_rejected":
      return "mobile.openCodeNative.composer.attachmentUploadRejected";
    case "upload_network":
      return "mobile.openCodeNative.composer.attachmentUploadNetwork";
    case "upload_failed":
      return "mobile.openCodeNative.composer.attachmentUploadFailed";
    case "upload_interrupted":
      return "mobile.openCodeNative.composer.attachmentUploadInterrupted";
    case "upload_gate_refused":
      // 门拒绝的实际文案来自 refusalCode 的映射（nativeErrorKey → 门码专属短语）；
      // 这一项只作穷尽性兜底，正常路径不会取到。
      return "mobile.openCodeNative.composer.attachmentUploadFailed";
  }
}

/** 上传异常 → 失败分类（+ 门拒绝时的门受控码）。消息只做**分类**，原文永不渲染。
 *  `status === null` = 传输层异常（XHR onerror / abort）；`too_large` 沿用既有上限文案。
 *  H03 上传口过门 = 提前失败：XHR 非 2xx 的受控码落在 ApiError.message（错误体不进
 *  `data`），形如受控码且属「确定零写出」的 message 视为门拒绝——chip 呈现门拒绝原因，
 *  不冒充「服务端未接受」。 */
function attachmentFailure(e: unknown): {
  code: NativeAttachmentErrorCode;
  refusalCode?: string;
} {
  if (!(e instanceof ApiError) || e.status === null) return { code: "upload_network" };
  if (e.message === "too_large") return { code: "too_large" };
  if (/^OC_[A-Z0-9_]+$/.test(e.message) && isDefiniteRefusal(e.message))
    return { code: "upload_gate_refused", refusalCode: e.message };
  return { code: "upload_failed" };
}

/** 面板「退回旧输入区」的原因（评审 Blocker 1）：认领被拒或不可恢复错误。 */
export interface NativeDeclineInfo {
  /** 受控码：`legacy_pending`（认领被旧队列挡住）或 `OC_*` 不可恢复码；null = 无 */
  code: string | null;
  /** 后端给的固定文案（若有） */
  reason: string | null;
}

export interface OpenCodeNativePanelProps {
  sessionId: string;
  initialBindingToken?: string | null;
  initialRouteVersion?: number;
  conversationTarget?: HTMLElement | null;
  onSnapshot?: (view: PublicNativeView) => void;
  showConversation?: boolean;
  processFold?: NativeProcessFold;
  /** 后端下发的路由说明（send-info 的 interactionRouteReason，终端侧原文，不翻译） */
  routeReason?: string | null;
  allowLegacy?: boolean;
  /** 交回旧路径成功后通知上层重拉 send-info（路由翻转的真实来源仍是服务端）。
   *  `abandoned` = 本次显式 force 作废的「代际已不可核对」动作条数（普通交回为 0）。 */
  onRouteChanged?: (info: {
    abandoned: number;
    bindingToken: string;
    routeVersion: number;
  }) => void;
  /** **候选路由被拒**（`GET /opencode/view` 返回 `route:"legacy"`，或不可恢复错误）：
   *  上层必须立刻退回旧输入区，并只展示一次原因。新回调，不改变旧调用方行为
   *  （未传 = 保持面板 + 现有错误/重试 UI）。 */
  onDeclined?: (info: NativeDeclineInfo) => void;
}

/** **不可恢复**的受控码：这些错误说明该会话不可能进入原生（平台/工具/会话不存在），
 *  面板应直接退回旧输入区。传输类/超时类**不在**此列——那些只是「这次没读到」，
 *  留在面板里重试即可（把网络抖动当成「不能原生」会让 UI 反复闪回旧输入区）。 */
const NON_RECOVERABLE_CODES: ReadonlySet<string> = new Set([
  "OC_PLATFORM_UNVERIFIED",
  "not_opencode",
  "no_session",
]);

/** 附件草稿与上传预算的跨重挂存储（按绑定 / 按会话两个键）。
 *
 * 为什么必须放在模块级：split ↔ 正文 ↔ 全屏 的切换会**卸载重建**面板，
 * 组件内 `useRef` 会随之丢失，导致「切一下布局附件就没了」（rebase 回归发现）。
 * 只存本端上传结果与预算计数，不含正文/凭据。 */
const attachmentDraftsStore = new Map<
  string,
  { items: NativeAttachment[]; budget: Map<string, number> }
>();

/** 测试隔离：模块级存储在用例之间必须清空，否则上一个用例的附件会串到下一个。
 *  生产运行不得调用（与 `resetActionAnchorMemoryForTests` 同款纪律）。 */
export function resetAttachmentDraftsForTests(): void {
  if (import.meta.env.MODE !== "test") return;
  attachmentDraftsStore.clear();
}

export default function OpenCodeNativePanel({
  sessionId,
  initialBindingToken,
  initialRouteVersion,
  conversationTarget,
  onSnapshot,
  showConversation = true,
  processFold,
  routeReason = null,
  allowLegacy = true,
  onRouteChanged,
  onDeclined,
}: OpenCodeNativePanelProps) {
  const { view, error, readFailed, refresh, reselect } = useOpenCodeNative(
    sessionId,
    true,
    onSnapshot,
    initialBindingToken,
    initialRouteVersion
  );
  const readBinding = view?.sessionId === sessionId ? view.bindingToken : null;

  const readBindingRef = useRef(readBinding);
  const readRouteVersion = view?.sessionId === sessionId ? view.routeVersion : undefined;
  const sessionIdRef = useRef(sessionId);
  // 影子 ref 一律在提交后同步（react-hooks/refs：渲染期禁写 ref）；消费方全部位于
  // effect / 事件回调（提交后时点），同步时点后移不改变其读取语义。
  useEffect(() => {
    readBindingRef.current = readBinding;
    sessionIdRef.current = sessionId;
  });
  /** 退回旧输入区只报一次：认领被拒（`route:"legacy"`）或不可恢复错误。 */
  const declinedRef = useRef(false);
  useEffect(() => {
    if (declinedRef.current) return;
    if (view !== null) {
      if (view.route !== "legacy" || view.routeReasonCode === "OC_INSTANCE_OWNERSHIP_UNVERIFIED")
        return;
      declinedRef.current = true;
      onDeclined?.({ code: view.routeReasonCode ?? null, reason: null });
      return;
    }
    if (error === null) return;
    const code = nativeErrorCode(error);
    if (code === null || !NON_RECOVERABLE_CODES.has(code)) return;
    declinedRef.current = true;
    onDeclined?.({ code, reason: errorCopy(error) });
  }, [view, error, onDeclined]);
  const [draft, setDraft] = useState("");
  const draftRevision = useRef(0);
  // 同一帧多次选文件也同步占预算；移除或确认发送后才释放。
  const attachmentBudget = useRef(new Map<string, number>());
  const attachmentControllers = useRef(new Map<string, AbortController>());
  const attachmentEpoch = useRef(0);
  const [attachments, setAttachments] = useState<NativeAttachment[]>([]);
  const attachmentSnapshot = useRef(attachments);
  useEffect(() => {
    attachmentSnapshot.current = attachments;
  });
  const attachmentDrafts = useRef(
    new Map<string, { items: NativeAttachment[]; budget: Map<string, number> }>()
  );
  const [sending, setSending] = useState(false);
  /** 本地派发暂停态（权威源 = outbox 快照，由 OpenCodeNativeQueue 上报）。
   *  2026-11-22 用户口径：暂停不再有独立框，只在发送键上体现（禁用 + 简短文案）。 */
  const [outboxPaused, setOutboxPaused] = useState(false);
  const [stopping, setStopping] = useState(false);
  type StopReceipt =
    | { kind: "interrupted" }
    | { kind: "idle" }
    | { kind: "unknown" }
    | { kind: "error"; reason: string }
    /** H09 parked O2：核对收敛 not_sent 后回执槽同步「已核对：未发出」短注
     *  （消除「停止失败」与锚行「不会重发」并存的初读矛盾）。 */
    | { kind: "verifiedNotSent" };
  const [stopReceipt, setStopReceipt] = useState<StopReceipt | null>(null);
  const stopActionEpochRef = useRef(0);
  const stopReceiptActionRef = useRef<string | null>(null);
  const stoppingRef = useRef(false);
  const [agentView, setAgentView] = useState<NativeAgentView | null>(null);
  const [agentChoice, setAgentChoice] = useState("");
  const agentCurrentRef = useRef<string | null>(null);
  const agentFetchSeqRef = useRef(0);
  const agentActionEpochRef = useRef(0);
  const [reconnectCopied, setReconnectCopied] = useState(false);
  const [reselecting, setReselecting] = useState(false);
  const reselectingRef = useRef(false);
  const returningRef = useRef(false);
  const forceBindingRef = useRef<string | null>(null);
  const forceVersionRef = useRef<number | null>(null);
  const forceActionIdsRef = useRef<string[] | null>(null);
  const [reselectError, setReselectError] = useState(false);
  const [reselectSuperseded, setReselectSuperseded] = useState(false);
  const [reselectUnconfirmed, setReselectUnconfirmed] = useState(false);
  /** J5：重选链超时后的三态呈现（已提交=正式目标 / 未提交=明确重试前提 / 暂未知）。 */
  const [reselectNotCommitted, setReselectNotCommitted] = useState(false);
  /** 最近一次预览确认的候选（丢回执恢复用）：结果未知时据此做只读路由核对。 */
  const lastCandidateRef = useRef<{
    sessionId: string;
    bindingToken: string;
    requestId: string;
  } | null>(null);
  const reselectCheckRef = useRef<string | null>(null);
  const [agentSwitching, setAgentSwitching] = useState(false);
  const [agentNotice, setAgentNotice] = useState<string | null>(null);
  const [agentError, setAgentError] = useState(false);
  /** 切换已定案、但随后读当前模式失败：只提示「重新读取」，不冒充切换失败。 */
  const [agentReadStale, setAgentReadStale] = useState(false);
  const [receipt, setReceipt] = useState<Receipt | null>(null);
  const [verifying, setVerifying] = useState(false);
  const [retractingId, setRetractingId] = useState<string | null>(null);
  /** inbox 读回结论（itemId → "retracted" | "stillThere"）：**只有读回才产生**。 */
  const [retractNotes, setRetractNotes] = useState<Record<string, "retracted" | "stillThere">>({});
  /** 审批结论（requestId → 回执）：点击即提交，本行随即显示如实结果。 */
  const [decisionNotes, setDecisionNotes] = useState<Record<string, DecisionReceipt>>({});
  /** 审批在途（requestId → 正在发的决定词）：单飞 + 按钮禁用。 */
  const [deciding, setDeciding] = useState<Record<string, DecisionWord>>({});
  /** 审批只读核对在途（requestId）：单飞，避免连点产生一串只读请求。 */
  const [decisionVerifying, setDecisionVerifying] = useState<string | null>(null);
  const [returning, setReturning] = useState(false);
  const [returnError, setReturnError] = useState<string | null>(null);
  /** 待用户确认的强制交回：后端报告的「代际已不可核对」动作条数（0/null = 无）。 */
  const [forceOffer, setForceOffer] = useState<number | null>(null);
  /** 核对失败时拿到的**受控码**（**与回执槽分开存**）：核对失败不是一次「动作结论」，
   *  它只是「这次核对没读到」——回执槽与锚都不该因此改变（R-1）。
   *  存码而不是存文案：发送语义的错误句（如「本次未写出」）绝不能拿来当核对失败的标题。 */
  const [verifyErrorCode, setVerifyErrorCode] = useState<string | null>(null);
  const [verifyFailed, setVerifyFailed] = useState(false);
  /** 核对 404：只说明「当前没查到」（首次派发仍可能在 reserve 之前）。它**不是**
   *  零写出结论——锚、发送锁与 Unknown 一律保留，只能再核对或明确放弃。 */
  const [verifyMissing, setVerifyMissing] = useState(false);
  const [showMessages, setShowMessages] = useState(false);
  /** R3：连接详情按需展开（路由公告/服务标识/重连命令/兼容与能力详情都收在这里）。
   *  正常态默认收起；展开状态是纯本地 UI 状态，不随视图刷新翻转。 */
  const [connectionDetailsOpen, setConnectionDetailsOpen] = useState(false);
  /** 原生 inbox（排队/插队待消费项）默认折叠：正常流转不占聊天页，展开状态纯本地。 */
  const [inboxOpen, setInboxOpen] = useState(false);
  /** D（2026-11-22 用户口径）：附件提示改为「+」旁小问号按需展开（不再常驻一行说明）。 */
  const [attachHintOpen, setAttachHintOpen] = useState(false);
  /** R5c：正在预览的原生附件（只含后端下发的受控 previewPath；无它不显示按钮）。 */
  const [attachmentPreview, setAttachmentPreview] = useState<{
    name: string;
    path: string;
  } | null>(null);
  /** 上次未收敛的动作锚（sessionStorage 恢复）：只用来给「核对」入口 */
  const [anchor, setAnchor] = useState<string | null>(() => readAnchor(sessionId));
  /** answer/decide 的恢复锚（G3）：与 `pending` 卡片生命周期解耦的「未确认动作」区。
   *  `sessionId` 随行存一份：换会话的首帧就不显示上一会话的行（不等 effect）。 */
  const [anchorRows, setAnchorRows] = useState<{
    sessionId: string;
    rows: OpenCodeRecoveryRow[];
  }>(() => ({ sessionId, rows: [] }));
  /** 每行的核对提示（"failed" = 超时/网络失败；"notFound" = 当前没查到，结果仍未确认）。 */
  const [anchorChecks, setAnchorChecks] = useState<Record<string, "failed" | "notFound">>({});
  /** 恢复锚核对在途（actionId）：单飞 + 按钮禁用。 */
  const [anchorVerifying, setAnchorVerifying] = useState<string | null>(null);
  /** M3（P1-c）：已受理（send 行）的**死代际呈现面**（actionId 集合）。
   *  来源只有账本宣告（`ActionView.runtimeDead === true`：登记文件 + 进程事实无法
   *  证明「行代际 = 当前活代际」）——读不到（404/网络失败）与字段缺席（未判定）
   *  都**不**进入本集合：出口只在死亡可证时在场，权威闸仍在 confirm-unknown 端点。 */
  const [deadActionIds, setDeadActionIds] = useState<ReadonlySet<string>>(() => new Set());
  const fileInputRef = useRef<HTMLInputElement>(null);
  /** 发送代际：迟到的回执不得覆盖新动作的槽位/清掉新草稿 */
  const sendEpochRef = useRef(0);
  const sendBindingEpochRef = useRef(0);
  /** 仅在本页发起时记录发送目标；恢复自存储的锚按未知目标保守处理。 */
  const sendAnchorBindingRef = useRef<string | null>(null);
  /** 发送在途闸（**ref 而非 state**）：同一渲染帧内的连点两次都进 handleSend 时，
   *  state 闭包里的 `sending` 仍是旧值——ref 同步写，第二次必然被挡下。 */
  const sendingRef = useRef(false);
  /** 在途投递的中止器（输入区就地停止键）：只中止**本端等待**，不改写服务端已发生的事实。
   *  中止后按「结果未确认」处理（保留恢复锚与发送锁，绝不宣称零写出）。 */
  const sendAbortRef = useRef<AbortController | null>(null);
  /** 核对 / 撤回同款同步在途闸（M9/M10）：一帧内两次点击不得发出两个请求 */
  const verifyingRef = useRef(false);
  const retractingRef = useRef(false);
  const retractEpochRef = useRef(0);
  const agentSwitchingRef = useRef(false);
  // 仅在内存记录本页动作的目标；恢复的旧锚目标未知，继续保守阻止重发。
  // ref = 权威（事件回调内读写）；下方镜像 state 仅供渲染期读（react-hooks/refs：
  // 渲染期不得读 ref）。两处写点（clear 会话切换 / set 撤回前）同帧同步镜像。
  const retractAnchorBindingsRef = useRef<Map<string, string>>(new Map());
  const [retractAnchorBindingsView, setRetractAnchorBindingsView] = useState<Map<string, string>>(
    () => new Map()
  );
  /** 审批：同一待决项一次只发一个决定（ref 同步闸），迟到回执按序号作废。 */
  const decidingRef = useRef<Set<string>>(new Set());
  const decisionSeqRef = useRef(0);
  const decisionSeqByItemRef = useRef<Record<string, number>>({});
  const decisionVerifyingRef = useRef<Set<string>>(new Set());
  /** 审批的代际（换会话 +1）：只读核对的迟到结果按它作废。 */
  const decisionEpochRef = useRef(0);
  /** 恢复锚：单飞闸（actionId 集合）、重读代际、会话代际（迟到核对不落到新会话）。 */
  const anchorVerifyingRef = useRef<Set<string>>(new Set());
  const anchorEpochRef = useRef(0);
  const anchorSessionEpochRef = useRef(0);
  const actionListSuccessEpochRef = useRef(0);
  /** J2（无本地锚发现）：服务端按会话的动作清单缓存（syncActionAnchors 合并去重；
   *  fetch 失败→空数组诚实退化，核对入口不因此消失）。**按会话隔离 + 代际先验**
   *  （用户复审 JB-3：缓存写入前先核对请求代际——A 的迟到清单不得混入 B）。 */
  const [actionListErrorSession, setActionListErrorSession] = useState<string | null>(null);
  const concludedActionIds = useRef(new Set<string>());
  const serverActionRowsRef = useRef<{
    sessionId: string;
    rows: OpenCodeActionListEntry[];
  }>({ sessionId: "", rows: [] });
  /** 原生表单结果（R5b）：按 formId 的内存态；答案只在内存，**永不写 sessionStorage**。 */
  const [formResults, setFormResults] = useState<Record<string, FormResultState>>({});
  /** 本会话已记住的 formId（sessionStorage 只存 id；见 FORM_IDS_PREFIX）。 */
  const trackedFormIdsRef = useRef<string[]>([]);
  /** 上一次视图里的 pending 表单 id：用于判定「pending 消失」（不是用它代替结果）。 */
  const pendingFormIdsRef = useRef<Set<string>>(new Set());
  /** 首次视图已处理过：挂载/换会话时对已不在 pending 的记住 id 拉一次结果。 */
  const formResultsSeededRef = useRef(false);
  /** 结果读取单飞闸（formId 集合）：同一 formId 同时最多一个 GET。 */
  const formResultInFlightRef = useRef<Set<string>>(new Set());
  /** 结果读取代际（换会话或绑定 +1）：迟到响应不落到新目标。 */
  const formResultEpochRef = useRef(0);
  const resultBindingRef = useRef({
    sessionId,
    binding: null as string | null,
    version: readRouteVersion,
  });

  useLayoutEffect(() => {
    const previous = resultBindingRef.current;
    const bindingChanged = previous.sessionId !== sessionId || previous.binding !== readBinding;
    if (bindingChanged || previous.version !== readRouteVersion) {
      sendBindingEpochRef.current += 1;
      stopActionEpochRef.current += 1;
      stopReceiptActionRef.current = null;
      stoppingRef.current = false;
      setStopping(false);
      setStopReceipt(null);
    }
    if (bindingChanged) {
      attachmentEpoch.current += 1;
      if (previous.sessionId === sessionId && previous.binding) {
        const snapshotItems: NativeAttachment[] = attachmentSnapshot.current.map((item) =>
          item.status === "uploading"
            ? { ...item, status: "failed", path: undefined, errorCode: "upload_interrupted" }
            : item
        );
        attachmentDrafts.current.set(previous.binding, {
          items: snapshotItems,
          budget: new Map(attachmentBudget.current),
        });
      } else if (previous.sessionId !== sessionId) {
        attachmentDrafts.current.clear();
      }
      for (const controller of attachmentControllers.current.values()) controller.abort();
      attachmentControllers.current.clear();
      // 只按**当前绑定**恢复：绑定一变（换服务/换标签页）就不恢复旧附件，
      // 否则 A 的附件会被带进 B（隔离测试抓到过）。布局重挂首帧无绑定时不恢复，
      // 属已知缺口（见 progress.md），不用会话兜底换隔离性。
      const restored = readBinding ? attachmentDrafts.current.get(readBinding) : undefined;
      attachmentBudget.current = new Map(restored?.budget ?? []);
      setAttachments(restored?.items ?? []);
    }
    if (bindingChanged || previous.version !== readRouteVersion) {
      formResultEpochRef.current += 1;
      formResultInFlightRef.current.clear();
      trackedFormIdsRef.current = readBinding ? readTrackedFormIds(sessionId, readBinding) : [];
      pendingFormIdsRef.current = new Set();
      formResultsSeededRef.current = false;
      setFormResults({});
      agentFetchSeqRef.current += 1;
      agentActionEpochRef.current += 1;
      agentCurrentRef.current = null;
      setAgentView(null);
      setAgentChoice("");
      agentSwitchingRef.current = false;
      setAgentSwitching(false);
      setAgentNotice(null);
      setAgentError(false);
      setReceipt(null);
      returningRef.current = false;
      setReturning(false);
      setForceOffer(null);
      forceBindingRef.current = null;
      forceVersionRef.current = null;
      forceActionIdsRef.current = null;
      retractEpochRef.current += 1;
      retractingRef.current = false;
      setRetractingId(null);
      setRetractNotes({});
      decisionEpochRef.current += 1;
      decisionSeqRef.current += 1;
      decisionSeqByItemRef.current = {};
      decidingRef.current.clear();
      decisionVerifyingRef.current.clear();
      setDecisionNotes({});
      setDeciding({});
      setDecisionVerifying(null);
    }
    resultBindingRef.current = { sessionId, binding: readBinding, version: readRouteVersion };
  }, [sessionId, readBinding, readRouteVersion]);

  useEffect(
    () => () => {
      sendEpochRef.current += 1;
      sendBindingEpochRef.current += 1;
      attachmentEpoch.current += 1;
      for (const controller of attachmentControllers.current.values()) controller.abort();
      attachmentControllers.current.clear();
    },
    []
  );

  // 换会话：清草稿、附件、回执与恢复锚视图（锚按会话键存，换回来仍是各自的那一个）
  useEffect(() => {
    setConnectionDetailsOpen(false);
    setDraft("");
    draftRevision.current += 1;
    attachmentBudget.current.clear();
    setAttachments([]);
    setReceipt(null);
    setRetractNotes({});
    setReturnError(null);
    setVerifyErrorCode(null);
    setVerifyFailed(false);
    setVerifyMissing(false);
    setAnchor(readAnchor(sessionId));
    sendAnchorBindingRef.current = null;
    retractAnchorBindingsRef.current.clear();
    setRetractAnchorBindingsView(new Map());
    sendEpochRef.current += 1; // 旧会话的在途回执作废
    // m-1：三个在途闸与对应展示态一起复位（只复位 sending 会在换会话后把
    // 「核对中…/撤回中…」永久挂在界面上，并且 ref 仍为 true 会挡住新会话的核对）
    sendingRef.current = false;
    verifyingRef.current = false;
    retractingRef.current = false;
    setSending(false);
    setVerifying(false);
    setRetractingId(null);
    // 审批结论是**会话内事实**：换会话后不得把上一会话的「已提交」带到新会话
    decisionSeqRef.current += 1;
    decisionEpochRef.current += 1;
    decisionSeqByItemRef.current = {};
    decidingRef.current.clear();
    decisionVerifyingRef.current.clear();
    setDecisionNotes({});
    setDeciding({});
    setDecisionVerifying(null);
    // 表单结果同样是会话内事实：换会话清空内存态，只从**本会话自己**的 formId 锚恢复。
    // 必须在这里按新会话重新载入锚（不能留空）：否则首帧「已不在 pending」的记住 id
    // 没有候选，已答结果就永远不发起回读（实机复验发现：结果读到了却不归位）。
    formResultEpochRef.current += 1;
    formResultInFlightRef.current.clear();
    trackedFormIdsRef.current = readBinding ? readTrackedFormIds(sessionId, readBinding) : [];
    pendingFormIdsRef.current = new Set();
    formResultsSeededRef.current = false;
    setFormResults({});
  }, [sessionId]);

  /** 按 formId 只读回读官方结果（R5b）：答案只进内存，绝不写 sessionStorage/动作账本。
   *  读不到只标记「结果未读到」，formId 保留（刷新或下次定案还能再试）。 */
  const loadFormResult = useCallback(
    (formId: string) => {
      if (formId === "" || !readBinding || formResultInFlightRef.current.has(formId)) return;
      const epoch = formResultEpochRef.current;
      formResultInFlightRef.current.add(formId);
      setFormResults((prev) => ({ ...prev, [formId]: { kind: "loading" } }));
      void fetchNativeFormResult(sessionId, formId, undefined, readBinding)
        .then((result) => {
          if (formResultEpochRef.current !== epoch || readBindingRef.current !== readBinding)
            return;
          setFormResults((prev) => ({ ...prev, [formId]: { kind: "ok", result } }));
        })
        .catch(() => {
          if (formResultEpochRef.current !== epoch || readBindingRef.current !== readBinding)
            return;
          setFormResults((prev) => ({ ...prev, [formId]: { kind: "error" } }));
        })
        .finally(() => {
          if (formResultEpochRef.current === epoch) formResultInFlightRef.current.delete(formId);
        });
    },
    [sessionId, readBinding]
  );

  /** 卡片定案回调（confirmed/settled/409）：确保 formId 已记住，并立刻回读官方结果。 */
  const handleFormSettled = useCallback(
    (formId: string) => {
      if (formId === "") return;
      if (!trackedFormIdsRef.current.includes(formId)) {
        const next = [...trackedFormIdsRef.current, formId].slice(-MAX_TRACKED_FORM_IDS);
        trackedFormIdsRef.current = next;
        if (readBinding) writeTrackedFormIds(sessionId, readBinding, next);
      }
      loadFormResult(formId);
    },
    [loadFormResult, sessionId, readBinding]
  );

  // 视图到达/刷新：收集 pending 表单 id（只存 id）；对「刚消失」与「挂载时已不在 pending」
  // 的记住 id 拉官方结果。**不以 pending 消失代替结果**：这里只是发起只读回读。
  useEffect(() => {
    // 视图与当前会话必须同源：换会话的首帧里 hook 还持有上一会话的视图，直接忽略，
    // 否则会把上一会话的表单 id 记进新会话（跨会话串结果）。
    if (view === null || view.sessionId !== sessionId) return;
    const pendingIds = view.pending
      .filter((item): item is Extract<PendingView, { kind: "form" }> => item.kind === "form")
      .map((item) => item.id);
    const pending = new Set(pendingIds);
    // 首帧的 ref 可能还是空的（layout effect 尚未按本绑定载入，或视图先于绑定到达）：
    // 直接从本会话的存储锚取一次并合并，否则「已不在 pending」的已答表单永远不会回读
    // （实机复验发现：formId 记着，却一次 GET 都不发，结果无处归位）。
    const persisted = readBinding ? readTrackedFormIds(sessionId, readBinding) : [];
    let tracked = Array.from(new Set([...persisted, ...trackedFormIdsRef.current]));
    for (const id of pendingIds) {
      if (!tracked.includes(id)) tracked = [...tracked, id];
    }
    tracked = tracked.slice(-MAX_TRACKED_FORM_IDS);
    trackedFormIdsRef.current = tracked;
    if (readBinding) writeTrackedFormIds(sessionId, readBinding, tracked);

    const vanished = tracked.filter((id) => pendingFormIdsRef.current.has(id) && !pending.has(id));
    const initial = !formResultsSeededRef.current;
    formResultsSeededRef.current = true;
    pendingFormIdsRef.current = pending;
    const candidates = new Set(vanished);
    if (initial) {
      // 刷新后恢复：本会话记住过的 id，只要当前不在 pending 就按 id 重读一次。
      for (const id of tracked) {
        if (!pending.has(id)) candidates.add(id);
      }
    }
    for (const id of candidates) loadFormResult(id);
  }, [loadFormResult, readBinding, sessionId, view]);

  const removeConcludedAnchor = useCallback((targetSession: string, actionId: string) => {
    if (targetSession === sessionIdRef.current) concludedActionIds.current.add(actionId);
    removeActionAnchor(targetSession, actionId);
  }, []);

  /** 恢复锚 → 「未确认动作」列表：读本会话锚，逐条读当前状态，**只有结论性状态**才剪除。
   *  与 `view.pending` 完全解耦：卡片卸载、pending 消失、页面重挂都从这里恢复。
   *  锚模块在增删时派发同页事件，用于即时刷新；挂载/换会话时也主动同步一次。
   *
   *  **刚落的锚不自动读**：写锚发生在 `POST /actions` 之前，派发窗口内
   *  `GET /actions/{id}` 可能必然 404；跳过年轻锚只是省一次读。404 在任何锚龄下都
   *  只表示「当前没查到」——绝不删锚，行与只读核对入口一律保留。 */
  const syncActionAnchors = useCallback(() => {
    const stored = readActionAnchors(sessionId);
    const epoch = (anchorEpochRef.current += 1);
    const previousListEpoch = actionListSuccessEpochRef.current;
    const now = Date.now();
    // J2（无本地锚发现）：合并服务端清单——**代际先验再写缓存**（JB-3：请求回包
    // 先核对请求代际，A 的迟到清单不得写入 B 的缓存），且**按会话隔离重建**（每次
    // 用当前会话的本地锚与本次服务端清单重算，不做追加式合并——迟到行自然失效）。
    const cachedForSession =
      serverActionRowsRef.current.sessionId === sessionId
        ? serverActionRowsRef.current.rows.filter(
            (row) => !concludedActionIds.current.has(row.actionId)
          )
        : [];
    setAnchorRows({
      sessionId,
      rows: mergeServerActionAnchors(stored, cachedForSession),
    });
    void fetchOpenCodeSessionActions(sessionId)
      .then((server) => {
        if (anchorEpochRef.current !== epoch) return;
        actionListSuccessEpochRef.current = epoch;
        setActionListErrorSession(null);
        const filtered = server.filter((row) => {
          const terminal =
            row.state === "confirmed" ||
            row.state === "settled" ||
            row.state === "not_accepted" ||
            row.state === "unknown_confirmed" ||
            (row.state === "not_sent" && Boolean(row.reasonCode));
          if (terminal) concludedActionIds.current.add(row.actionId);
          return !concludedActionIds.current.has(row.actionId);
        });
        const previous =
          serverActionRowsRef.current.sessionId === sessionId
            ? serverActionRowsRef.current.rows
            : [];
        serverActionRowsRef.current = { sessionId, rows: filtered };
        const local = readActionAnchors(sessionId).filter(
          (row) => !concludedActionIds.current.has(row.actionId)
        );
        setAnchorRows({ sessionId, rows: mergeServerActionAnchors(local, filtered) });
        setDeadActionIds((prev) => {
          const next = new Set(prev);
          for (const row of previous) next.delete(row.actionId);
          for (const row of local) next.delete(row.actionId);
          for (const row of filtered) {
            if (row.runtimeDead === true) next.add(row.actionId);
            else next.delete(row.actionId);
          }
          return next;
        });
      })
      .catch(() => {
        if (anchorEpochRef.current === epoch) setActionListErrorSession(sessionId);
      });
    for (const item of stored) {
      if (now - item.at < ANCHOR_AUTO_REREAD_MS) continue; // 年轻锚：不自动 GET（仅省读）
      void readOpenCodeAction(item.actionId)
        .then((action) => {
          if (anchorEpochRef.current !== epoch) return;
          // patch6 O-M9-2 ②：恢复重读同时刷新 M3 死代际呈现面。「尚未接管」面
          // （view 认领 409，路由行 active 指向死亡代际）没有投影换代键可触发
          // 重derive——出口必须由账本宣告（`runtimeDead`，与 confirm-unknown 权威
          // 闸同一 process_fact 判定惯例）直接驱动；缺席 = 未判定 ≠ 死亡：不猜，
          // fail-closed 不变（权威闸仍在 confirm-unknown 端点）。
          if (
            actionListSuccessEpochRef.current === previousListEpoch &&
            !serverActionRowsRef.current.rows.some((row) => row.actionId === action.actionId)
          ) {
            setDeadActionIds((prev) => {
              const next = new Set(prev);
              if (action.runtimeDead === true) next.add(action.actionId);
              else next.delete(action.actionId);
              return next;
            });
          }
          if (
            isConclusiveState(action.state) &&
            (action.state !== "not_sent" || action.reasonCode)
          ) {
            removeConcludedAnchor(sessionId, action.actionId); // 事件会再同步一次
            setAnchorRows((prev) =>
              prev.sessionId === sessionId
                ? { ...prev, rows: prev.rows.filter((row) => row.actionId !== action.actionId) }
                : prev
            );
          }
          // 非结论性状态（unconfirmed/dispatching/…）：保持行与只读核对入口
        })
        .catch(() => {
          if (anchorEpochRef.current !== epoch) return;
          // 404 与其余失败都只是「这次没读到」：锚与行都保留，核对按钮仍可点
        });
    }
  }, [sessionId, removeConcludedAnchor]);

  /** M3（P1-c）：已受理行的死代际重derive触发键 = 当前服务代际身份（serviceId + pid）。
   *  投影换代（原服务死亡/被替换）⇒ 键变化 ⇒ 按 action ID 重读账本一次，刷新呈现面。
   *  挂载首帧不重读（挂载面的恢复同步由 `syncActionAnchors` 的既有自动重读承担，
   *  避免同一锚一帧双读）；投影更新但代际未变 ⇒ 键不变 ⇒ 零额外读。 */
  const deadGenerationKeyRef = useRef<string | null>(null);
  useEffect(() => {
    anchorSessionEpochRef.current += 1;
    concludedActionIds.current.clear();
    setActionListErrorSession(null);
    setAnchorChecks({});
    setAnchorVerifying(null);
    anchorVerifyingRef.current.clear();
    setDeadActionIds(new Set());
    deadGenerationKeyRef.current = null;
    syncActionAnchors();
    const onAnchorChanged = (event: Event): void => {
      const detail = (event as CustomEvent<{ sessionId?: string }>).detail;
      if (detail?.sessionId !== undefined && detail.sessionId !== sessionId) return;
      syncActionAnchors();
    };
    globalThis.addEventListener?.(ACTION_ANCHOR_CHANGED_EVENT, onAnchorChanged);
    return () => globalThis.removeEventListener?.(ACTION_ANCHOR_CHANGED_EVENT, onAnchorChanged);
  }, [sessionId, syncActionAnchors]);

  const runtimeIdentityKey = view?.bindingToken
    ? `${sessionId}:${view.serviceId ?? ""}:${view.servicePid ?? 0}:${view.bindingToken}`
    : null;
  useEffect(() => {
    if (!runtimeIdentityKey || deadGenerationKeyRef.current === runtimeIdentityKey) return;
    const firstArrival = deadGenerationKeyRef.current === null;
    deadGenerationKeyRef.current = runtimeIdentityKey;
    if (firstArrival) return; // 首帧交给既有恢复同步，不双读
    syncActionAnchors();
  }, [runtimeIdentityKey, syncActionAnchors]);

  const uploading = attachments.some((a) => a.status === "uploading");
  /** 失败附件 = 锁写判据（F4）：发送若此刻放行，`attachmentPaths` 会悄悄少掉这一项，
   *  用户以为带上了、实际只发正文。只有移除或重选（新条目）后才恢复。 */
  const uploadFailed = attachments.some((a) => a.status === "failed");
  const readyPaths = useMemo(
    () => attachments.filter((a) => a.status === "ready" && a.path).map((a) => a.path as string),
    [attachments]
  );
  /** 能力表「缺键 = 未判定 = 不可用」（与后端 `require_capability` 同口径）：
   *  只有显式 `allowed === true` 才放行；缺席/拒绝一律按不可用处理。 */
  const owned = view?.route === "opencode_native";
  const readOnlyNative = view?.routeReasonCode === "OC_INSTANCE_OWNERSHIP_UNVERIFIED";
  const bindingRequired = readOnlyNative || (owned && view?.bindingRequired === true);
  /** 后端要求绑定时保持只读；桌面进程观察不能替代实例归属证据。 */
  const identityRefused =
    bindingRequired && view?.routeReasonCode === "OC_INSTANCE_OWNERSHIP_UNVERIFIED";
  /** 只认后端扫描到的桌面进程参数；前端按钮不能替代桌面重连；
   *  且 desktopBound **不得**覆盖身份拒绝（identityRefused ⇒ 恒锁）。 */
  const bindingBound = !bindingRequired;
  /** I2：读取失败 ⇒ 手里的 view 只是「最后成功视图」的旧快照，不是当前可写视图。
   *  所有**新**原生写入口（发送/答题/审批/停止/撤回/模式）一律锁；已有动作的
   *  只读核对不受影响；失败原因与「重新读取」入口只挂在会话级错误块（R3 一处解释）。
   *  恢复（下一次成功读取清掉 error）后自动回到正常态，草稿/锚/题卡全程不动。 */
  const bindingScope = JSON.stringify([sessionId, readBinding, view?.routeVersion]);
  const [bindingRefusal, setBindingRefusal] = useState<{ scope: string; code: string } | null>(
    null
  );
  const currentRefusal = bindingRefusal?.scope === bindingScope ? bindingRefusal.code : null;
  const bindingRefused = currentRefusal === "OC_BINDING_STALE";
  const recordBindingRefusal = useCallback(
    (code: string | null) => {
      const normalized = code?.trim().toUpperCase();
      if (normalized && BINDING_DEAD_RESELECT_CODES.has(normalized)) {
        setBindingRefusal({ scope: bindingScope, code: normalized });
      }
    },
    [bindingScope]
  );
  const writeGateOpen = bindingBound && !readFailed && !bindingRefused;
  useEffect(() => {
    if ((!owned && !readOnlyNative) || !readBinding) {
      setAgentView(null);
      agentCurrentRef.current = null;
      return;
    }
    let alive = true;
    const seq = (agentFetchSeqRef.current += 1);
    void fetchOpenCodeAgents(sessionId, undefined, readBinding)
      .then((next) => {
        if (!alive || seq !== agentFetchSeqRef.current) return;
        setAgentView(next);
        const nextCurrent = next.current ?? next.options[0]?.id ?? "";
        const previousCurrent = agentCurrentRef.current;
        agentCurrentRef.current = next.current;
        // 正式当前值变化时纠正；用户手动选中的未提交候选不被覆盖。
        setAgentChoice((prev) => (prev === "" || prev === previousCurrent ? nextCurrent : prev));
      })
      .catch(() => {
        if (alive) setAgentView(null);
      });
    return () => {
      alive = false;
    };
  }, [owned, readOnlyNative, sessionId, view?.revision, readBinding]);
  const sendAllowed = owned && view !== null && view.capabilities.send?.allowed === true;
  /** 2026-11-22 用户口径：运行中发送键**就地**变停止键（不再有常驻插队按钮，
   *  也不再有暂停框）。会话级停止能力/写门/未决动作单飞闸统一由下方 `stopAvailable`
   *  判定——渲染与点击共用同一个判据，避免出现「可见但点不动」的停止键。 */
  const retractAllowed = owned && view !== null && view.capabilities.retract?.allowed === true;
  /** 发送能力的受控原因词（H05：门码在此出现时必须走本地化文案，机器串不上屏）。 */
  const sendReasonCode = view?.capabilities.send?.reasonCode ?? null;
  /** R3/H05 会话级绑定门的在场条件（原内联表达式提为单点）：门在场且原因是归属族时，
   *  发送阻断行不得重复解释同一事实——同一阻断同屏只允许一处说明。 */
  const bindingGateVisible =
    bindingRequired &&
    !bindingBound &&
    (identityRefused || (view?.serviceUrl !== undefined && view?.directory !== undefined));
  const sendBlockedDuplicatedByGate = bindingGateVisible && isOwnershipRefusalCode(sendReasonCode);
  /** R3-slim：健康会话不挂任何连接诊断外框。只有「确实有诊断价值」时才出现紧凑入口：
   *  绑定未确认（含身份拒绝）、未认领/被拒（可重选）、或存在能力告警与交回入口。
   *  正常会话打开后无公告、无空详情框。 */
  const hasDiagnostics =
    bindingRequired ||
    !owned ||
    (view?.routeReasonCode !== undefined && view?.routeReasonCode !== null) ||
    routeReason !== null ||
    allowLegacy;
  /** 发送阻断行的文案（单点）：受控码先走本地化映射（含归属族门码专属短语）；
   *  缺原因词 = 能力整键缺席 → 通用句；未收录的能力词原样展示（便于排障，不编文案）。 */
  const sendBlockedCopy = (() => {
    const mapped = nativeErrorKey(sendReasonCode);
    if (mapped !== null) return i18n.t(mapped);
    if (sendReasonCode === null)
      return i18n.t("mobile.openCodeNative.composer.capabilityBlockedGeneric");
    return i18n.t("mobile.openCodeNative.composer.capabilityBlocked", { reason: sendReasonCode });
  })();
  /** 视图层「归属未证」族阻断码 → 队列写控件（重绑/恢复派发是真写）禁用：
   *  路由级码优先（既有 B1 语义），能力表带归属/门族码时同样传导。
   *  本地安全操作（暂停/撤回）不受影响；既有未决动作的只读核对不受影响。 */
  const viewWriteBlockedCode = isOwnershipRefusalCode(view?.routeReasonCode)
    ? (view?.routeReasonCode ?? null)
    : !sendAllowed && isOwnershipRefusalCode(sendReasonCode)
      ? sendReasonCode
      : null;
  /** 整组答题**挂卡**判据：缺键 = 未判定 = 不可写（只留只读说明与受控原因词）。
   *  R3 复审 F1：绑定阻断**不**在挂卡判据里——同一表单的题卡保持挂载（草稿与题进度
   *  都在卡内状态，卸载重建会丢答案），只经 `writeBlocked` 锁写。 */
  const answerCardMounted =
    view !== null &&
    ((owned && view.capabilities.answer_group?.allowed === true) || readOnlyNative);
  /** 原生表单结果（R5b）：按官方 `metadata.tool` 归位到原对话。
   *
   *  关联判据（全部满足才挂，任何一条不满足都不挂——绝不把结果挂到另一组题上）：
   *  ① 结果带 `toolMessageId`，且该值命中本次投影的某条 assistant 记录（`messageIds`）；
   *  ② 该记录里**恰好一个** `question` 工具调用（多于一个时无法区分是哪一问，保守放弃）；
   *  ③ 结果 `sessionId` = 当前会话。
   *  以「消息 seq」为键：同一记录的 question 行渲染在该记录第一条消息下方。 */
  const questionResultsBySeq = (() => {
    const map = new Map<string, FormResultState[]>();
    if (view === null || view.sessionId !== sessionId) return map;
    const ids = view.messageIds ?? {};
    const callIds = view.toolCallIds ?? {};
    // 正式答案（随消息落盘）优先：表单作答后上游会删除表单，回读必然 404。
    // 这份记录刷新/历史/换会话后依旧可读，因此是结果展示的主来源。
    const official = view.questionAnswers ?? {};
    // ① 先登记「本次窗口里确实存在的 question 调用」为合法归位键（空桶）。
    //    只登记键、不写内容——这一轮若把已有内容重置成空数组，正式答案就会被抹掉
    //    （实机复验发现：数据全对、DOM 一行不显示，根因就是这里的顺序）。
    for (const message of view.messages) {
      if (message.kind !== "tool-call" || message.toolName !== "question") continue;
      const callId = callIds[String(message.seq)];
      const messageId = ids[String(message.seq)];
      if (!callId || !messageId) continue;
      map.set(`${messageId}:${callId}`, []);
    }
    // ② 正式答案（随消息落盘）优先：表单作答后上游会删除表单，回读必然 404。
    //    这份记录刷新/历史/换会话后依旧可读，因此是结果展示的主来源。
    for (const message of view.messages) {
      if (message.kind !== "tool-call" || message.toolName !== "question") continue;
      const callId = callIds[String(message.seq)];
      const messageId = ids[String(message.seq)];
      const answers = official[String(message.seq)];
      if (!callId || !messageId || !answers || answers.length === 0) continue;
      map.set(`${messageId}:${callId}`, [
        {
          kind: "ok",
          result: {
            formId: "",
            sessionId,
            status: "answered",
            answers: {},
            readableAnswers: answers,
          },
        },
      ]);
    }
    for (const [formId, state] of Object.entries(formResults)) {
      if (state.kind !== "ok" || state.result.formId !== formId) continue;
      if (state.result.sessionId !== sessionId) continue;
      const messageId = state.result.toolMessageId;
      const callId = state.result.toolCallId;
      if (!messageId || !callId) continue;
      const key = `${messageId}:${callId}`;
      const bucket = map.get(key);
      if (bucket === undefined) continue; // 该调用不在本次窗口 / 不是 question：不挂
      map.set(key, [...bucket, state]);
    }
    return map;
  })();
  /** **未收敛的恢复锚 = 锁写判据**（R-1）：判据只看锚本身。
   *
   *  锚的生命周期是唯一真相：点击发送即落锚；只有**终态结论**（confirmed / settled /
   *  not_sent / not_accepted）或用户**显式放弃**才清锚。核对失败（网络/5xx）与 404
   *  都**不清锚**——404 只表示「当前没查到」（首发仍可能在 reserve 之前），不是结论。
   *
   *  早期实现用 `receipt.kind === "unknown"` 参与判定：核对一失败就把回执改成
   *  `rejected`，于是锚还活着却既不算「未收敛」（发送键被放回）又被渲染门挡住
   *  （面板不可见）——正是 F2 要关掉的隐式重发路径。 */
  const unresolved = anchor !== null;
  const canSend =
    writeGateOpen &&
    sendAllowed &&
    !sending &&
    !uploading &&
    !uploadFailed &&
    !unresolved &&
    view !== null &&
    (draft.trim().length > 0 || readyPaths.length > 0);

  /** **已证受理**（N4）：`acknowledged` 且带精确 nativeId（后端在精确 inbox id 命中时
   *  才返回该形态）。缺 nativeId 的 acknowledged 一律保守按「结果未知」处理。 */
  const isProvenAcknowledged = (action: ActionView): boolean =>
    action.state === "acknowledged" &&
    typeof action.nativeId === "string" &&
    action.nativeId.trim() !== "";

  /** 结论落地（发送与核对共用）：按状态收敛回执槽 + 恢复锚；**永不重发**。
   *  `intent` 只来自「发送」那一帧（核对路径为 null ⇒ 绝不动草稿/附件）：
   *  - **只有 confirmed / settled** 才清草稿，且只在草稿仍是点击时那一份时才清；
   *  - 附件同理只清**属于本次意图**的那些（发送在途新加的附件保留，M8）。
   *
   *  N4：已证受理（acknowledged+nativeId）解除**本条**的 Unknown 发送锚
   *  （锚只保护它自己那条），转成锚列表的 `send` 行（只读核对入口）；
   *  已证受理在当前有效绑定上允许新意图，冻结稿按「已接收的那份」消费。 */
  /** 冻结稿身份（actionId → 本页发起的那一次发送）：核对可能在发送回执之后才拿到结论，
   *  那时必须按**原 action ID** 消费原稿与原附件——否则已确认收到的正文仍留在输入区，
   *  用户一点发送就会用新 action ID 再投一次（D-1 禁止的重复发送）。只在内存，不落盘。 */
  const pendingIntentsRef = useRef(new Map<string, SendIntent>());

  const settle = useCallback(
    (actionId: string, action: ActionView, intent: SendIntent | null) => {
      const currentAnchor = readAnchor(sessionId);
      if (currentAnchor !== null && currentAnchor !== actionId) return;
      const conclusive = isConclusiveState(action.state) || action.state === "queued";
      const provenAcknowledged = isProvenAcknowledged(action);
      if (conclusive) {
        writeAnchor(sessionId, null);
        setAnchor((current) => (current === actionId ? null : current));
      } else if (provenAcknowledged) {
        // 已证受理：解除 Unknown 锚（不株连、也不被株连），send 锚行进场（只含 id）
        writeAnchor(sessionId, null);
        setAnchor((current) => (current === actionId ? null : current));
        addActionAnchor(sessionId, { actionId, kind: "send", targetId: sessionId });
      } else {
        writeAnchor(sessionId, actionId);
        setAnchor(actionId);
      }
      switch (action.state) {
        case "queued":
          setReceipt({ kind: "queued" });
          break;
        case "acknowledged":
          if (provenAcknowledged) setReceipt({ kind: "acknowledged" });
          else setReceipt({ kind: "unknown", actionId }); // 缺精确 id：保守按未知
          break;
        case "confirmed":
          setReceipt({ kind: "confirmed" });
          break;
        case "settled":
          setReceipt({ kind: "settled" });
          break;
        case "not_sent":
          setReceipt({ kind: "notSent", code: action.reasonCode ?? null });
          break;
        case "not_accepted":
          setReceipt({ kind: "notAccepted", code: action.reasonCode ?? null });
          break;
        case "unknown_confirmed":
          // patch3 D2：用户显式确认过终态未知的归档态（核对收敛回读命中时如实呈现）
          setReceipt({ kind: "unknownConfirmed" });
          break;
        default:
          // dispatching / unconfirmed：结果未确认，都要核对，不重发
          setReceipt({ kind: "unknown", actionId });
          break;
      }
      // 内容冻结：queued / 已证受理 = 已持久接收，或 confirmed / settled 时消费本次冻结稿。
      // `intent` 为 null（只读核对路径）时按 actionId 取回原冻结意图——核对证实「已收到」
      // 与发送回执证实同等效力：正文已被接收，就不能再作为新意图发一次。
      const frozen = intent ?? pendingIntentsRef.current.get(actionId) ?? null;
      if (
        frozen !== null &&
        (action.state === "queued" ||
          action.state === "confirmed" ||
          action.state === "settled" ||
          provenAcknowledged)
      ) {
        if (draftRevision.current === frozen.draftRevision) setDraft("");
        for (const id of frozen.attachmentIds) attachmentBudget.current.delete(id);
        const sent = new Set(frozen.attachmentIds);
        setAttachments((prev) => prev.filter((a) => !sent.has(a.id)));
        pendingIntentsRef.current.delete(actionId);
      } else if (conclusive && frozen !== null) {
        // 已证零写出（not_sent / not_accepted）：正文从未被接收，草稿必须留在输入区，
        // 但冻结身份到此结束（下一次发送是全新 action ID）。
        pendingIntentsRef.current.delete(actionId);
      }
    },
    [sessionId]
  );

  /** 旧目标回执只收敛原 actionId，不修改当前目标的回执、草稿或附件。
   *  N4：已证受理同样解除旧锚并留下 send 核对行（锚在旧目标自己的会话里）。 */
  const settleDetachedSend = useCallback(
    (actionId: string, action: ActionView) => {
      const provenAcknowledged = isProvenAcknowledged(action);
      if (
        (isConclusiveState(action.state) || action.state === "queued" || provenAcknowledged) &&
        readAnchor(sessionId) === actionId
      ) {
        writeAnchor(sessionId, null);
        if (sessionIdRef.current === sessionId)
          setAnchor((current) => (current === actionId ? null : current));
        if (provenAcknowledged)
          addActionAnchor(sessionId, { actionId, kind: "send", targetId: sessionId });
      }
    },
    [sessionId]
  );

  /** 发送（唯一写入口；一次用户动作 = 一个 actionId = 最多一次派发）。
   *  投递方式（queue/steer）是**本次点击的参数**而非驻留 state（H08 D3）：回调里不存在
   *  可过期的投递闭包，UI 上的动作与载荷逐字一致；插队没有「选择」，只有「点击」。 */
  const handleSend = useCallback(
    async (delivery: DeliveryKind) => {
      if (!canSend || view === null || sendingRef.current) return;
      sendingRef.current = true;
      const frozenText = draft;
      const frozenPaths = readyPaths.slice();
      const intent: SendIntent = {
        draftRevision: draftRevision.current,
        text: frozenText,
        attachmentIds: attachments.filter((a) => a.status === "ready").map((a) => a.id),
      };
      const actionId = newNativeActionId();
      const actionBinding = view.bindingToken;
      const bindingEpoch = sendBindingEpochRef.current;
      const abortController = new AbortController();
      sendAbortRef.current = abortController;
      sendAnchorBindingRef.current = actionBinding;
      const myEpoch = (sendEpochRef.current += 1);
      setSending(true);
      setReceipt(null);
      setVerifyFailed(false);
      setVerifyMissing(false);
      setVerifyErrorCode(null);
      // 冻结稿身份只在内存（actionId → 正文/附件 id）：核对拿到结论时据此消费原稿。
      pendingIntentsRef.current.set(actionId, intent);
      // 恢复锚：点击即落，只有 actionId（绝不写正文/答案）
      writeAnchor(sessionId, actionId);
      setAnchor(actionId);
      try {
        const action = await submitOpenCodeAction(
          {
            actionId,
            sessionId,
            bindingToken: view.bindingToken,
            operation: {
              kind: "send",
              text: frozenText,
              attachmentPaths: frozenPaths,
              delivery,
              clientMessageId: clientMessageIdOf(actionId),
            },
          },
          undefined,
          abortController.signal
        );
        if (sendEpochRef.current !== myEpoch) return; // 迟到回执：不覆盖新动作
        if (
          readBindingRef.current !== actionBinding ||
          sendBindingEpochRef.current !== bindingEpoch
        ) {
          settleDetachedSend(actionId, action);
          return;
        }
        settle(actionId, action, intent);
      } catch (e) {
        if (sendEpochRef.current !== myEpoch) return;
        const err = e instanceof ApiError ? e : new ApiError(null, String(e));
        const code = nativeErrorCode(err);
        if (
          readBindingRef.current !== actionBinding ||
          sendBindingEpochRef.current !== bindingEpoch
        ) {
          if (
            err.status !== null &&
            isDefiniteRefusal(code) &&
            readAnchor(sessionId) === actionId
          ) {
            writeAnchor(sessionId, null);
            setAnchor((current) => (current === actionId ? null : current));
          }
          return;
        }
        if (err.status !== null && isDefiniteRefusal(code)) {
          // 派发前受控拒绝（集合一）：后端在写出之前就拒了 —— 确定零写出，锚无意义
          writeAnchor(sessionId, null);
          setAnchor(null);
          // G4 patch7：拒绝码随槽留存——绑定死亡族（F5 实录：钉窗退出后重读重建
          // 不了绑定）据此挂「重新选择当前目标」入口；其余码不挂，文案不变。
          setReceipt({ kind: "rejected", text: errorCopy(err), code });
          recordBindingRefusal(code);
          return;
        }
        // 其余一律按**结局未知**（网络层异常、传输/服务/响应类 409、未收录码）：
        // 服务端账本对这些也记 Unconfirmed，锚必须留着，锁不能解除（R-1①）
        setReceipt({ kind: "unknown", actionId, code });
      } finally {
        if (sendEpochRef.current === myEpoch) {
          sendingRef.current = false;
          setSending(false);
        }
      }
    },
    [
      attachments,
      canSend,
      draft,
      readyPaths,
      recordBindingRefusal,
      sessionId,
      settle,
      settleDetachedSend,
      view,
    ]
  );

  /** 只读核对（**不是重发**）：结果按权威账本收敛。
   *  代际闸（M10）：核对发起后若已开始新的发送/放弃，旧动作的回执不得覆盖新槽位与新锚。 */
  const handleVerify = useCallback(
    async (actionId: string) => {
      if (verifyingRef.current) return;
      verifyingRef.current = true;
      const myEpoch = sendEpochRef.current;
      setVerifying(true);
      try {
        const action = await verifyOpenCodeActionWithTimeout(actionId);
        if (sendEpochRef.current !== myEpoch) return; // 旧动作的核对：作废
        setVerifyFailed(false);
        setVerifyMissing(false);
        setVerifyErrorCode(null);
        if (sendAnchorBindingRef.current !== readBindingRef.current) {
          settleDetachedSend(actionId, action);
        } else {
          settle(actionId, action, null); // 核对永远不动草稿
        }
      } catch (e) {
        if (sendEpochRef.current !== myEpoch) return;
        const err = e instanceof ApiError ? e : new ApiError(null, String(e));
        // 锚与「未确认」一律保留：404 也只表示当前没查到（首次派发可能仍在 reserve
        // 之前，账本行还没建立）。绝不把 404 折成「零写出」而解锁发送。
        const missing = err.status === 404;
        setVerifyMissing(missing);
        setVerifyFailed(!missing);
        setVerifyErrorCode(
          missing ? null : isAbortError(e) ? "OC_TRANSPORT_TIMEOUT" : nativeErrorCode(err)
        );
      } finally {
        verifyingRef.current = false;
        setVerifying(false);
      }
    },
    [settle, settleDetachedSend]
  );

  /** **明确放弃**本次未收敛的核对：丢弃恢复锚、解除锁写、保留草稿。
   *  这是用户显式承担的判断（不是「重发」按钮）——文案必须说清「放弃核对，不会重发」。 */
  const handleAbandon = useCallback(() => {
    if (sendingRef.current) return;
    sendEpochRef.current += 1; // 在途核对的回执作废（不得把「已定案」写回放弃后的槽位）
    writeAnchor(sessionId, null);
    setAnchor(null);
    sendAnchorBindingRef.current = null;
    setVerifyFailed(false);
    setVerifyMissing(false);
    setVerifyErrorCode(null);
    setReceipt({ kind: "abandoned" });
  }, [sessionId]);

  /** 撤回 + **读回**：`DELETE /inbox/{id}` 的 204 不是撤回证据（任务 0 已证），
   *  条目只有在那之后的读回里消失才算撤回。能力未放行时连尝试都不做（F4）。 */
  /** 插队（原生 PATCH）：把已在 OpenCode 队列里的 queue 项改成 steer，由上游在当前回合
   *  交付并打断。**只发一次 PATCH，不重发正文**；回执未知时只读核对、不自动重发。 */
  const handlePromote = useCallback(
    async (itemId: string) => {
      if (view === null || !writeGateOpen) return;
      if (view.capabilities.promote_inbox?.allowed !== true) return;
      if (
        readActionAnchors(sessionId).some(
          (anchor) => anchor.kind === "retract" && anchor.targetId === itemId
        )
      )
        return;
      const actionBinding = view.bindingToken;
      const actionEpoch = retractEpochRef.current;
      const actionId = newNativeActionId();
      retractAnchorBindingsRef.current.set(actionId, actionBinding);
      setRetractAnchorBindingsView(new Map(retractAnchorBindingsRef.current));
      addActionAnchor(sessionId, { actionId, kind: "retract", targetId: itemId });
      try {
        const result = await submitOpenCodeAction({
          actionId,
          sessionId,
          bindingToken: actionBinding,
          operation: { kind: "promote_inbox", itemId },
        });
        // 已受理/已定案 ⇒ 无需保留核对锚；未知/派发中 ⇒ 保留锚，出口是只读核对。
        if (isConclusiveState(result.state) || result.state === "acknowledged") {
          removeConcludedAnchor(sessionId, actionId);
        }
      } catch (error) {
        if (
          error instanceof ApiError &&
          error.status !== null &&
          isDefiniteRefusal(nativeErrorCode(error))
        ) {
          removeConcludedAnchor(sessionId, actionId);
        }
      } finally {
        if (retractEpochRef.current === actionEpoch) refresh();
      }
    },
    [refresh, sessionId, view, writeGateOpen, removeConcludedAnchor]
  );

  const handleRetract = useCallback(
    async (itemId: string) => {
      if (retractingRef.current || view === null || !retractAllowed || !writeGateOpen) return;
      if (
        readActionAnchors(sessionId).some(
          (anchor) =>
            anchor.kind === "retract" &&
            anchor.targetId === itemId &&
            (retractAnchorBindingsRef.current.get(anchor.actionId) ?? view.bindingToken) ===
              view.bindingToken
        )
      )
        return;
      retractingRef.current = true;
      setRetractingId(itemId);
      const actionBinding = view.bindingToken;
      const actionEpoch = retractEpochRef.current;
      const actionId = newNativeActionId();
      retractAnchorBindingsRef.current.set(actionId, actionBinding);
      setRetractAnchorBindingsView(new Map(retractAnchorBindingsRef.current));
      addActionAnchor(sessionId, { actionId, kind: "retract", targetId: itemId });
      let confirmed = false;
      try {
        const result = await submitOpenCodeAction({
          actionId,
          sessionId,
          bindingToken: actionBinding,
          operation: { kind: "retract", itemId },
        });
        if (isConclusiveState(result.state)) removeConcludedAnchor(sessionId, actionId);
        confirmed = result.state === "confirmed";
      } catch (error) {
        if (
          error instanceof ApiError &&
          error.status !== null &&
          isDefiniteRefusal(nativeErrorCode(error))
        ) {
          removeConcludedAnchor(sessionId, actionId);
        }
      }
      try {
        if (retractEpochRef.current !== actionEpoch || readBindingRef.current !== actionBinding)
          return;
        const fresh = await fetchOpenCodeView(sessionId, undefined, actionBinding);
        if (retractEpochRef.current !== actionEpoch || readBindingRef.current !== actionBinding)
          return;
        const stillThere = fresh.inbox.some((item) => item.id === itemId);
        if (stillThere || confirmed) {
          setRetractNotes((prev) => ({
            ...prev,
            [itemId]: stillThere ? "stillThere" : "retracted",
          }));
        }
      } catch {
        // 读回失败只保留原 action ID 的核对入口。
      } finally {
        if (retractEpochRef.current === actionEpoch) {
          retractingRef.current = false;
          setRetractingId(null);
          refresh();
        }
      }
    },
    [refresh, retractAllowed, sessionId, view, writeGateOpen, removeConcludedAnchor]
  );

  /** 审批：点击决定 = 明确确认（不再弹二次确认）。一次点击 = 一个 actionId = 最多一次派发。
   *  结论只描述「已提交 / 已定案 / 未确认」，**从不**判断是哪一端先完成的。 */
  const handleDecide = useCallback(
    async (item: Extract<PendingView, { kind: "permission" }>, decision: DecisionWord) => {
      if (
        view === null ||
        view.route !== "opencode_native" ||
        !writeGateOpen ||
        hasUnresolvedAction(sessionId, "decide", item.id, view.bindingToken)
      )
        return;
      // 能力门（缺键 = 未判定 = 不放行）：判定就地进行，回调依赖表因此只含 view
      const capability = decision === "once" ? "permission_once" : "permission_reject";
      if (view.capabilities[capability]?.allowed !== true) return;
      if (decidingRef.current.has(item.id)) return; // 同步单飞：连点只发一次
      decidingRef.current.add(item.id);
      const seq = (decisionSeqRef.current += 1);
      const myEpoch = decisionEpochRef.current;
      decisionSeqByItemRef.current[item.id] = seq;
      setDeciding((prev) => ({ ...prev, [item.id]: decision }));
      const actionId = newNativeActionId();
      // 恢复锚（G3）：派发前落锚（只含 id）；pending 行消失/页面刷新后仍可从这里核对
      rememberActionBinding(actionId, view.bindingToken);
      addActionAnchor(sessionId, { actionId, kind: "decide", targetId: item.id });
      try {
        const action = await submitOpenCodeAction({
          actionId,
          sessionId,
          bindingToken: view.bindingToken,
          operation: {
            kind: "decide",
            requestId: item.id,
            contextToken: item.context_token,
            decision,
          },
        });
        if (decisionSeqByItemRef.current[item.id] !== seq) {
          if (
            action.state === "not_sent" ||
            action.state === "not_accepted" ||
            isConclusiveState(action.state)
          )
            removeConcludedAnchor(sessionId, actionId);
          return;
        } // 迟到回执只收敛原动作锚
        if (action.state === "not_sent" || action.state === "not_accepted") {
          removeConcludedAnchor(sessionId, actionId); // 零写出：锚无意义
          setDecisionNotes((prev) => ({
            ...prev,
            [item.id]: {
              kind: "rejected",
              text: i18n.t("mobile.openCodeNative.pending.decisionNotSent"),
              // 受控码要留给用户看：否则「没有生效」无法与「网络抖动」区分（评审 M-7）
              code: action.reasonCode ?? null,
            },
          }));
          return;
        }
        if (isConclusiveState(action.state)) {
          removeConcludedAnchor(sessionId, actionId); // 已定案：不再需要核对入口
          setDecisionNotes((prev) => ({ ...prev, [item.id]: { kind: "submitted" } }));
          // 权威结论仍看原生投影（pending 是否消失由服务端读回决定）
          refresh();
          return;
        }
        // acknowledged / dispatching / unconfirmed：结果未确认，只给只读核对（锚保留）
        setDecisionNotes((prev) => ({
          ...prev,
          [item.id]: { kind: "unknown", actionId, code: action.reasonCode ?? null },
        }));
      } catch (e) {
        if (decisionSeqByItemRef.current[item.id] !== seq) return;
        const err = e instanceof ApiError ? e : new ApiError(null, String(e));
        const code = nativeErrorCode(err);
        if (err.status === 409 && code === "OC_REQUEST_SETTLED") {
          // 该审批已在原生侧定案：只呈现原生结果（重读投影），不重发、不归因
          removeConcludedAnchor(sessionId, actionId);
          setDecisionNotes((prev) => ({ ...prev, [item.id]: { kind: "settled" } }));
          refresh();
          return;
        }
        if (err.status !== null && isDefiniteRefusal(code)) {
          // 派发前受控拒绝（含 409 的 OC_ROUTE_CHANGED / OC_ACTION_CONFLICT）：确定零写出
          removeConcludedAnchor(sessionId, actionId);
          setDecisionNotes((prev) => ({
            ...prev,
            [item.id]: { kind: "rejected", text: errorCopy(err) },
          }));
          return;
        }
        // 其余（含传输类 409 / 网络层异常）：结果未确认，只给只读核对（锚保留）
        setDecisionNotes((prev) => ({
          ...prev,
          [item.id]: { kind: "unknown", actionId, code },
        }));
        if (err.status === 409) refresh();
      } finally {
        if (decisionEpochRef.current === myEpoch && decisionSeqByItemRef.current[item.id] === seq) {
          decidingRef.current.delete(item.id);
          setDeciding((prev) => {
            const next = { ...prev };
            delete next[item.id];
            return next;
          });
        }
      }
    },
    [writeGateOpen, refresh, sessionId, view, removeConcludedAnchor]
  );

  /** 审批未确认时的**只读核对**（不是重发）。与提交同款代际闸：换会话后迟到的核对
   *  结果不得写进已清空的回执表，也不得触发一次属于旧会话的 `refresh`。 */
  const handleDecisionVerify = useCallback(
    async (requestId: string, actionId: string) => {
      if (decisionVerifyingRef.current.has(requestId)) return;
      decisionVerifyingRef.current.add(requestId);
      const myEpoch = decisionEpochRef.current;
      setDecisionVerifying(requestId);
      try {
        const action = await verifyOpenCodeActionWithTimeout(actionId);
        if (decisionEpochRef.current !== myEpoch) {
          if (
            action.state === "not_sent" ||
            action.state === "not_accepted" ||
            isConclusiveState(action.state)
          )
            removeConcludedAnchor(sessionId, actionId);
          return;
        } // 迟到核对只收敛原动作锚
        if (action.state === "not_sent" || action.state === "not_accepted") {
          removeConcludedAnchor(sessionId, actionId);
          setDecisionNotes((prev) => {
            const note = prev[requestId];
            if (!note || note.kind !== "unknown" || note.actionId !== actionId) return prev;
            return {
              ...prev,
              [requestId]: {
                kind: "rejected",
                text: i18n.t("mobile.openCodeNative.pending.decisionNotSent"),
                code: action.reasonCode ?? null,
              },
            };
          });
          return;
        }
        if (isConclusiveState(action.state)) {
          removeConcludedAnchor(sessionId, actionId);
          setDecisionNotes((prev) => {
            const note = prev[requestId];
            if (!note || note.kind !== "unknown" || note.actionId !== actionId) return prev;
            return {
              ...prev,
              [requestId]: { kind: action.state === "settled" ? "settled" : "submitted" },
            };
          });
          refresh();
          return;
        }
        // acknowledged / dispatching / unconfirmed：仍无法判定，保留锚与核对入口
        setDecisionNotes((prev) => {
          const note = prev[requestId];
          if (!note || note.kind !== "unknown" || note.actionId !== actionId) return prev;
          return {
            ...prev,
            [requestId]: { kind: "unknown", actionId, code: action.reasonCode ?? null },
          };
        });
      } catch {
        // 核对失败/超时/中止/404 都不是动作结论：锚与「未确认」保持，核对键恢复可点。
        // 404 只说明当前没查到（首次派发可能仍在 reserve 之前），绝不折成零写出。
      } finally {
        if (decisionEpochRef.current === myEpoch) {
          decisionVerifyingRef.current.delete(requestId);
          setDecisionVerifying((cur) => (cur === requestId ? null : cur));
        }
      }
    },
    [refresh, sessionId, removeConcludedAnchor]
  );

  /** 「未确认动作」行的**只读核对**（不是重发）：结论落地才删锚；超时/中止/网络失败
   *  只是「这次没读到」——锚保留、行保留、按钮恢复可点，绝不折成 `not_sent`。
   *
   *  404 同样不是结论（锚龄也不构成证据）：首发可能仍在 reserve 之前、账本行还没建立。
   *  行保留、按钮恢复可点，只提示「尚未查到，结果仍未确认」。 */
  const handleAnchorVerify = useCallback(
    async (item: OpenCodeActionAnchor) => {
      if (anchorVerifyingRef.current.has(item.actionId)) return;
      anchorVerifyingRef.current.add(item.actionId);
      const mySessionEpoch = anchorSessionEpochRef.current;
      const myDecisionEpoch = decisionEpochRef.current;
      const myStopEpoch = stopActionEpochRef.current;
      const listEpoch = actionListSuccessEpochRef.current;
      setAnchorVerifying(item.actionId);
      setAnchorChecks((prev) => {
        const next = { ...prev };
        delete next[item.actionId];
        return next;
      });
      try {
        const action = await verifyOpenCodeActionWithTimeout(item.actionId);
        if (anchorSessionEpochRef.current !== mySessionEpoch) return; // 迟到核对：不落地
        if (isConclusiveState(action.state) && (action.state !== "not_sent" || action.reasonCode)) {
          removeConcludedAnchor(sessionId, item.actionId);
          if (
            item.kind === "stop" &&
            stopActionEpochRef.current === myStopEpoch &&
            stopReceiptActionRef.current === item.actionId
          ) {
            if (action.state === "settled") {
              setStopReceipt({
                kind: action.reasonCode === "already_idle" ? "idle" : "interrupted",
              });
            } else if (action.state === "not_sent" || action.state === "not_accepted") {
              setStopReceipt({ kind: "verifiedNotSent" });
            }
          }
          if (
            item.kind === "decide" &&
            item.targetId &&
            decisionEpochRef.current === myDecisionEpoch
          ) {
            const rejected = action.state === "not_sent" || action.state === "not_accepted";
            setDecisionNotes((prev) => {
              const note = prev[item.targetId!];
              if (!note || note.kind !== "unknown" || note.actionId !== item.actionId) return prev;
              return {
                ...prev,
                [item.targetId!]: rejected
                  ? {
                      kind: "rejected",
                      text: i18n.t("mobile.openCodeNative.pending.decisionNotSent"),
                      code: action.reasonCode ?? null,
                    }
                  : { kind: action.state === "settled" ? "settled" : "submitted" },
              };
            });
            if (!rejected) refresh();
          }
          if (item.kind === "send") {
            // 发送结论落地：刷新一次权威视图/队列，原生 inbox 里的对应项随之收敛
            refresh();
          }
          setAnchorRows((prev) =>
            prev.sessionId === sessionId
              ? { ...prev, rows: prev.rows.filter((row) => row.actionId !== item.actionId) }
              : prev
          );
        }
        // 仍未确认：保留锚与行，允许再核对。
        // 新清单的证据优先于较早发出的单条核对。
        if (item.kind === "send" && actionListSuccessEpochRef.current === listEpoch) {
          if (action.state === "acknowledged" && action.nativeId?.trim()) {
            const ledger = { state: action.state, nativeId: action.nativeId };
            setAnchorRows((prev) =>
              prev.sessionId === sessionId
                ? {
                    ...prev,
                    rows: prev.rows.map((row) =>
                      row.actionId === action.actionId ? { ...row, ledger } : row
                    ),
                  }
                : prev
            );
            const currentList = serverActionRowsRef.current;
            if (currentList.sessionId === sessionId) {
              currentList.rows = currentList.rows.map((row) =>
                row.actionId === action.actionId ? { ...row, ...ledger } : row
              );
            }
          }
          setDeadActionIds((prev) => {
            const next = new Set(prev);
            if (action.runtimeDead === true) next.add(action.actionId);
            else next.delete(action.actionId);
            return next;
          });
        }
      } catch (e) {
        if (anchorSessionEpochRef.current !== mySessionEpoch) return;
        if (e instanceof ApiError && e.status === 404) {
          // 当前没查到 ≠ 零写出：锚与行都保留（老锚也一样），只提示稍后再核对
          setAnchorChecks((prev) => ({ ...prev, [item.actionId]: "notFound" }));
          return;
        }
        // 15s 超时/中止/网络失败：只说明「没读到」，不是动作结论
        setAnchorChecks((prev) => ({ ...prev, [item.actionId]: "failed" }));
      } finally {
        if (anchorSessionEpochRef.current === mySessionEpoch) {
          anchorVerifyingRef.current.delete(item.actionId);
          setAnchorVerifying((cur) => (cur === item.actionId ? null : cur));
        }
      }
    },
    [refresh, sessionId, removeConcludedAnchor]
  );

  /** patch3 D2：「确认终态未知并解锁重选」（H13 证据 §10-D2 的用户出口）。
   *
   *  文案如实（确认框）：该动作的结果最终未知，确认后本端不再等待，可重新选择目标；
   *  已有动作不会自动重发，账本保留「已确认未知」归档记录。
   *
   *  后端闸（fail-closed）：行代际仍可核对（当前活代际）→ 409 `OC_ACTION_STATE`
   *  零改写——前端把拒绝原因如实挂在行内短注，不解锁、不清锚。成功（`unknown_confirmed`
   *  归档态）才执行 `onConfirmed`（发送槽清锚解锁 / 锚行删除）。 */
  const confirmUnknownBusyRef = useRef(false);
  const [confirmUnknownBusy, setConfirmUnknownBusy] = useState<string | null>(null);
  /** 确认出口的拒绝/失败短注：**按 actionId 归行**（多个出口并存时只挂发起那次的那行，
   *  不在所有行上复读同一句——单处如实呈现）。 */
  const [confirmUnknownError, setConfirmUnknownError] = useState<{
    actionId: string;
    text: string;
  } | null>(null);
  const handleConfirmUnknown = useCallback(async (actionId: string, onConfirmed: () => void) => {
    if (confirmUnknownBusyRef.current) return;
    if (!window.confirm(i18n.t("mobile.openCodeNative.confirmUnknownHint"))) return;
    confirmUnknownBusyRef.current = true;
    setConfirmUnknownBusy(actionId);
    setConfirmUnknownError(null);
    const mySessionEpoch = anchorSessionEpochRef.current;
    try {
      const action = await confirmOpenCodeActionUnknown(actionId);
      if (anchorSessionEpochRef.current !== mySessionEpoch) return; // 迟到回执：不落地
      if (action.state === "unknown_confirmed") onConfirmed();
      else
        setConfirmUnknownError({
          actionId,
          text: i18n.t("mobile.openCodeNative.unresolvedActions.confirmUnknownRefused"),
        });
    } catch (e) {
      if (anchorSessionEpochRef.current !== mySessionEpoch) return;
      setConfirmUnknownError({
        actionId,
        text: e instanceof ApiError ? errorCopy(e) : i18n.t("mobile.openCodeNative.errors.generic"),
      });
    } finally {
      confirmUnknownBusyRef.current = false;
      setConfirmUnknownBusy(null);
    }
  }, []);

  /** 发送槽的确认出口：清发送锚 → 解锁发送 + 重选屏障解除；回执槽转「已确认未知」短注。 */
  const handleConfirmUnknownSend = useCallback(
    (actionId: string | null) => {
      if (actionId === null) return;
      void handleConfirmUnknown(actionId, () => {
        writeAnchor(sessionId, null);
        setAnchor((current) => (current === actionId ? null : current));
        setReceipt({ kind: "unknownConfirmed" });
        setVerifyFailed(false);
        setVerifyMissing(false);
        setVerifyErrorCode(null);
        refresh();
      });
    },
    [handleConfirmUnknown, refresh, sessionId]
  );

  /** 「未确认动作」行的确认出口：删锚行（账本保留归档态；若有队列可见面则如实归档）。 */
  const handleConfirmUnknownRow = useCallback(
    (item: OpenCodeActionAnchor) => {
      void handleConfirmUnknown(item.actionId, () => {
        removeConcludedAnchor(sessionId, item.actionId);
        setAnchorRows((prev) =>
          prev.sessionId === sessionId
            ? { ...prev, rows: prev.rows.filter((row) => row.actionId !== item.actionId) }
            : prev
        );
        setAnchorChecks((prev) => {
          const next = { ...prev };
          delete next[item.actionId];
          return next;
        });
        refresh();
      });
    },
    [handleConfirmUnknown, refresh, sessionId, removeConcludedAnchor]
  );

  /** 交回旧路径（显式、幂等；有未决动作时后端拒绝并零变更）。
   *
   *  **两步确认**（评审 Blocker 2）：后端在 409 里带回 `unverifiableActions`（换代前、
   *  再也无法核对的动作条数）。> 0 时**不自动强推**，先展示「作废 + 可能重复发送」
   *  的确认块；用户点确认后才带 `force: true` 再发一次。 */
  const handleReturnLegacy = useCallback(
    async (force = false) => {
      const actionBinding = readBindingRef.current;
      const routeVersion = view?.routeVersion;
      if (!allowLegacy || returningRef.current) return;
      if (
        !actionBinding ||
        typeof routeVersion !== "number" ||
        !Number.isSafeInteger(routeVersion) ||
        routeVersion < 1 ||
        (force &&
          (forceBindingRef.current !== actionBinding ||
            forceVersionRef.current !== routeVersion ||
            !forceActionIdsRef.current))
      ) {
        setReturnError(i18n.t("mobile.openCodeNative.reselectFailed"));
        return;
      }
      const epoch = sendBindingEpochRef.current;
      returningRef.current = true;
      setReturning(true);
      setReturnError(null);
      try {
        const result = await returnOpenCodeRouteToLegacy(
          sessionId,
          actionBinding,
          routeVersion,
          force,
          force ? (forceActionIdsRef.current ?? undefined) : undefined
        );
        if (sessionIdRef.current !== sessionId || sendBindingEpochRef.current !== epoch) return;
        onRouteChanged?.({
          abandoned: result.abandoned,
          bindingToken: result.bindingToken,
          routeVersion: result.routeVersion,
        });
      } catch (e) {
        if (sessionIdRef.current !== sessionId || sendBindingEpochRef.current !== epoch) return;
        const err = e instanceof ApiError ? e : new ApiError(null, String(e));
        const unverifiable = unverifiableActionsOf(err);
        const ids = err.data?.unverifiableActionIds;
        const offerVersion = err.data?.routeVersion;
        if (
          !force &&
          err.data?.forceAvailable === true &&
          unverifiable > 0 &&
          offerVersion === routeVersion &&
          Array.isArray(ids) &&
          ids.length === unverifiable &&
          ids.every((id) => typeof id === "string" && id.length > 0) &&
          new Set(ids).size === ids.length
        ) {
          // 确认仅覆盖服务端本次展示的精确候选集。
          forceBindingRef.current = actionBinding;
          forceVersionRef.current = routeVersion;
          forceActionIdsRef.current = ids as string[];
          setForceOffer(unverifiable);
        } else {
          setReturnError(
            err.data?.reasonCode === OC_RESPONSE_INVALID_MARKER
              ? i18n.t("mobile.openCodeNative.route.returnUnconfirmed")
              : errorCopy(err)
          );
        }
      } finally {
        if (sessionIdRef.current === sessionId && sendBindingEpochRef.current === epoch) {
          returningRef.current = false;
          setReturning(false);
        }
      }
    },
    [allowLegacy, onRouteChanged, sessionId, view?.routeVersion]
  );

  /** 选文件即上传（原生路径同样走既有受控落盘端点，拿到绝对路径后随新端点提交——
   *  `file://` URI 由后端按 action 目标拼装，手机只给受控目录内的绝对路径）。
   *
   *  单文件预算在**上传前**预检：超过 5 MiB 的文件不上传（上传端点自身仍是 20 MiB，
   *  但原生通道的保守预算是 5 MiB——让文件先落盘成功、再在原生写时失败是更差的体验）。 */
  const addFiles = useCallback(
    (files: File[]) => {
      if (!writeGateOpen || !readBinding) return;
      for (const file of files) {
        const id = `${Date.now()}-${Math.random().toString(36).slice(2, 8)}`;
        if (file.size > NATIVE_ATTACHMENT_MAX_BYTES) {
          setAttachments((prev) => [
            ...prev,
            { id, name: file.name, status: "failed", errorCode: "too_large" },
          ]);
          continue;
        }
        const total = [...attachmentBudget.current.values()].reduce((sum, size) => sum + size, 0);
        if (attachmentBudget.current.size >= 10 || total + file.size > 20 * 1024 * 1024) {
          setAttachments((prev) => [
            ...prev,
            { id, name: file.name, status: "failed", errorCode: "message_limit" },
          ]);
          continue;
        }
        attachmentBudget.current.set(id, file.size);
        const controller = new AbortController();
        const epoch = attachmentEpoch.current;
        attachmentControllers.current.set(id, controller);
        setAttachments((prev) => [...prev, { id, name: file.name, status: "uploading" }]);
        void uploadAttachment(sessionId, file, controller.signal, undefined, {
          route: "opencode_native",
          bindingToken: readBinding,
        })
          .then((res) => {
            if (epoch !== attachmentEpoch.current || controller.signal.aborted) return;
            setAttachments((prev) =>
              prev.map((a) =>
                a.id === id
                  ? !res?.path?.trim()
                    ? // 403/空回执/缺 path：上传没有拿到可提交的路径，按失败处理（禁用发送）
                      { ...a, status: "failed", errorCode: "upload_rejected" }
                    : { ...a, status: "ready", path: res.path, excludeWarning: res.excludeWarning }
                  : a
              )
            );
          })
          .catch((e: unknown) => {
            if (epoch !== attachmentEpoch.current || controller.signal.aborted) return;
            const failure = attachmentFailure(e);
            setAttachments((prev) =>
              prev.map((a) =>
                a.id === id
                  ? {
                      ...a,
                      status: "failed",
                      errorCode: failure.code,
                      refusalCode: failure.refusalCode,
                    }
                  : a
              )
            );
          })
          .finally(() => {
            if (attachmentControllers.current.get(id) === controller)
              attachmentControllers.current.delete(id);
          });
      }
    },
    [sessionId, writeGateOpen, readBinding]
  );

  const { renderBody } = useMessageRenderers({
    // 原生窗口里的路径链接不接详情页预览（本批未接）：空路径集 ⇒ 不做链接化，
    // #file: 内链点开是 no-op（如实不误导：正文照常可读，只是不假装能预览）
    openFile: NOOP_OPEN_FILE,
    files: NO_FILES,
  });

  const receiptNode = (() => {
    if (receipt === null) return null;
    switch (receipt.kind) {
      case "queued":
        return (
          <p data-testid="native-receipt-queued" className={RECEIPT_NEUTRAL}>
            {i18n.t("mobile.openCodeNative.receipt.queued")}
          </p>
        );
      case "acknowledged":
        // 2026-11-22 用户裁决：已受理属**正常流转**，不显示常驻回执
        // （「OpenCode 已受理，等待处理。」这条不要）。消息进正式记录后由正文呈现；
        // 需要用户处理的状态才留提示（失败 / 未知 / 未发出）。
        return null;
      case "confirmed":
        return (
          <p data-testid="native-receipt-confirmed" className={RECEIPT_OK}>
            {i18n.t("mobile.openCodeNative.receipt.confirmed")}
          </p>
        );
      case "settled":
        return (
          <p data-testid="native-receipt-settled" className={RECEIPT_OK}>
            {i18n.t("mobile.openCodeNative.receipt.settled")}
          </p>
        );
      case "notSent":
        return (
          <p data-testid="native-receipt-not-sent" className={RECEIPT_NEUTRAL}>
            {i18n.t("mobile.openCodeNative.receipt.notSent")}
            {reasonSuffix(receipt.code)}
          </p>
        );
      case "notAccepted":
        return (
          <p data-testid="native-receipt-not-accepted" className={RECEIPT_NEUTRAL}>
            {i18n.t("mobile.openCodeNative.receipt.notAccepted")}
            {reasonSuffix(receipt.code)}
          </p>
        );
      case "abandoned":
        return (
          <p data-testid="native-receipt-abandoned" className={RECEIPT_NEUTRAL}>
            {i18n.t("mobile.openCodeNative.receipt.abandoned")}
          </p>
        );
      case "unknownConfirmed":
        return (
          <p data-testid="native-receipt-unknown-confirmed" className={RECEIPT_NEUTRAL}>
            {i18n.t("mobile.openCodeNative.receipt.unknownConfirmed")}
          </p>
        );
      case "rejected":
        return (
          <p data-testid="native-receipt-rejected" className={RECEIPT_ERR}>
            {receipt.text}
          </p>
        );
      default:
        return null;
    }
  })();

  // ===== 未收敛块（唯一出口块；可见性只由锚决定，R-1）=====
  // 回执只决定**措辞**与 testid，不决定是否出现。标题永远只描述「结果仍未确认」这一事实
  // ——**绝不复用发送语义的错误句**（「本次未写出」是另一个结论，R-1②）；核对失败的
  // 技术原因只作为**附注**出现，且用中性的原因词，不携带任何写没写出的断言。
  const unresolvedActionId = receipt?.kind === "unknown" ? receipt.actionId : anchor;
  const unresolvedTitle = i18n.t(
    verifyMissing
      ? "mobile.openCodeNative.receipt.verifyNotFound"
      : verifyFailed
        ? "mobile.openCodeNative.receipt.unresolvedVerifyFailed"
        : receipt?.kind === "unknown"
          ? "mobile.openCodeNative.receipt.unconfirmed"
          : "mobile.openCodeNative.receipt.recovery"
  );
  /** 核对失败的原因词（中性）：受控码只作为附注展示，不参与任何结论。
   *  404 没有「原因词」可言——它只说明当前没查到，标题已经说清。 */
  const unresolvedDetail = verifyMissing
    ? null
    : verifyFailed
      ? i18n.t("mobile.openCodeNative.receipt.verifyFailedDetail", {
          detail: verifyCauseCopy(verifyErrorCode),
        })
      : receipt?.kind === "unknown" && receipt.code
        ? i18n.t("mobile.openCodeNative.receipt.unknownCode", { code: receipt.code })
        : null;
  const unresolvedTestId = verifyMissing
    ? "native-verify-not-found"
    : verifyFailed
      ? "native-verify-failed"
      : receipt?.kind === "unknown"
        ? "native-receipt-unconfirmed"
        : receipt === null
          ? "native-recovery"
          : "native-verify-failed";
  const unresolvedVerifyTestId = receipt === null ? "native-recovery-verify" : "native-verify";
  /** 本地 send 锚仅为已证受理而写入。有正式账本时，以最新的状态与原生 ID 为准。 */
  const actionAnchors = anchorRows.sessionId === sessionId ? anchorRows.rows : [];
  const isAcknowledgedSend = (item: OpenCodeRecoveryRow) =>
    item.kind === "send" &&
    (!item.ledger || (item.ledger.state === "acknowledged" && !!item.ledger.nativeId?.trim()));
  /** R3-slim：已证受理属正常流转，**不再单独成列表**（正文里那条消息就是它的呈现）。
   *  但死代际的已受理动作仍需可执行出口（只读核对 / 确认终态未知），因此只把它们并入
   *  「未确认动作」区；活代际的已受理行完全不渲染。 */
  const acknowledgedSends = actionAnchors.filter(isAcknowledgedSend);
  /** 当前锁着输入区的那个发送锚（`unresolvedActionId`）。
   *
   *  它已经有唯一的出口块（`native-receipt-unconfirmed` / `native-recovery` 那一块，
   *  带核对与放弃），因此同一 action ID 的清单行**不再重复成行**——否则同屏出现两个
   *  核对按钮，而清单行那一个核对成功也不会解除发送锁（它走的是另一条收敛路径）。
   *  清单行只在「锚已不在」或「死代际」时补充呈现。 */
  const lockedSendActionId = unresolvedActionId;
  /** 在途（`dispatching` / `queued`）**不进「未确认动作」区**：它只是还没回来，
   *  不是需要用户处理的事实。让它出现在这里会让人以为切换失败（2026-11-22 现场）。
   *  模式切换在途的说明由模式行自己就地呈现（`native-action-inflight`）。
   *  注意：`set_agent` 在途只是暂不列；一旦转成 `unconfirmed`，它会立刻回到本列表
   *  （仍然可核对、仍然不重发）。 */
  const isInFlightRow = (item: OpenCodeRecoveryRow) =>
    item.ledger?.state === "dispatching" || item.ledger?.state === "queued";
  /** 只有**模式切换**的在途行不列（它 1–2 秒就回来，列出来像失败）。
   *  send 的 `dispatching` 仍必须可见：那是「已取得派发权、结果待核对」，
   *  核对入口不能因为它是中间态而消失。 */
  const isHiddenInFlight = (item: OpenCodeRecoveryRow) =>
    item.kind === "set_agent" && isInFlightRow(item);
  const unresolvedAnchorRows = actionAnchors.filter((item) => {
    if (item.actionId === lockedSendActionId) return false;
    if (isHiddenInFlight(item)) return false;
    return !isAcknowledgedSend(item) || deadActionIds.has(item.actionId);
  });
  /** G4 patch7（GC）：已接管详情错误区「重新选择当前目标」入口的在场判定。
   *  两类**可证明**形态（patch6「尚未接管」面出口的同族扩展，点击共用既有
   *  preview→route/native 版本 CAS 链）：
   *  ① 绑定死亡 = 最近一次发送的受控拒绝回执带绑定死亡族码（确定零写出，锚已清——
   *     F5 实录：钉窗退出后「重新读取」重建不了绑定，重选是唯一自助恢复路）；
   *  ② 核对受阻 = 已受理行被账本宣告死代际（`runtimeDead: true`，GA 收紧后的判定
   *     同源：只有 Ended 才 Some(true)，未判定/活代际不渲染——与 M3 确认出口同判据）。
   *  互斥不重复：读取失败面已有既有重选入口（native-reselect）→ error 在场时本入口
   *  不渲染；未接管面出口（native-reselect-declined）只在 !owned 渲染；确认终态未知
   *  是归档出口、本入口是重建绑定出口，按形态各出现一次。 */
  const bindingDeadReceipt =
    receipt !== null &&
    receipt.kind === "rejected" &&
    typeof receipt.code === "string" &&
    BINDING_DEAD_RESELECT_CODES.has(receipt.code.trim().toUpperCase());
  const deadAcknowledgedSends = acknowledgedSends.filter((item) =>
    deadActionIds.has(item.actionId)
  );
  const ownedReselectEntry =
    owned &&
    error === null &&
    (currentRefusal !== null || bindingDeadReceipt || deadAcknowledgedSends.length > 0);

  const stopAvailable =
    owned &&
    view?.sessionId === sessionId &&
    view.runtimeState === "running" &&
    view.capabilities.stop_session?.allowed === true &&
    writeGateOpen;
  /** 输入区停止键的**在场**判据（2026-11-22 用户口径：就地替换发送键）：
   *  - 投递在途（`sending`）恒在场（终止本次投递不需要任何会话级能力）；
   *  - 否则要求 `stopAvailable`（running + 停止能力 + 写门）。
   *  两者皆否 → 不渲染停止键（于是「无运行无投递时点击无行为」是结构性保证）。 */
  const stopInlineVisible = sending || stopAvailable;

  const stopCurrent = async () => {
    if (
      !stopAvailable ||
      view === null ||
      stoppingRef.current ||
      hasUnresolvedAction(sessionId, "stop", sessionId, view.bindingToken)
    )
      return;
    stoppingRef.current = true;
    setStopping(true);
    setStopReceipt(null);
    const actionBinding = view.bindingToken;
    const actionEpoch = stopActionEpochRef.current;
    const actionId = newNativeActionId();
    stopReceiptActionRef.current = actionId;
    rememberActionBinding(actionId, actionBinding);
    addActionAnchor(sessionId, { actionId, kind: "stop", targetId: sessionId });
    try {
      const result = await submitOpenCodeAction(
        {
          actionId,
          sessionId,
          bindingToken: view.bindingToken,
          operation: { kind: "stop_session" },
        },
        // H5 预算对齐：停止等待 = 服务端统一预算 + 余量（OPENCODE_STOP_POST_TIMEOUT_MS）。
        // 预算内服务端必回正式回执；此处超时语义仍是「结果未知，不自动重发」。
        OPENCODE_STOP_POST_TIMEOUT_MS
      );
      if (isConclusiveState(result.state)) {
        removeConcludedAnchor(sessionId, actionId);
      }
      if (stopActionEpochRef.current !== actionEpoch || readBindingRef.current !== actionBinding)
        return;
      if (result.state === "settled") {
        setStopReceipt({
          kind: result.reasonCode === "already_idle" ? "idle" : "interrupted",
        });
      } else if (result.state === "not_sent" || result.state === "not_accepted") {
        setStopReceipt({ kind: "error", reason: result.reasonCode ?? "" });
        recordBindingRefusal(result.reasonCode ?? null);
      } else {
        setStopReceipt({ kind: "unknown" });
      }
      refresh();
    } catch (err) {
      const code = err instanceof ApiError ? nativeErrorCode(err) : null;
      if (
        err instanceof ApiError &&
        err.status !== null &&
        err.status < 500 &&
        isDefiniteRefusal(code)
      ) {
        removeConcludedAnchor(sessionId, actionId);
      }
      if (stopActionEpochRef.current !== actionEpoch || readBindingRef.current !== actionBinding)
        return;
      if (err instanceof ApiError && err.status !== null && err.status < 500) {
        setStopReceipt({ kind: "error", reason: code ?? "" });
        if (isDefiniteRefusal(code)) recordBindingRefusal(code);
        refresh();
      } else {
        // 写请求已发出但回执丢失：停止是幂等语义，但重复点击可能停到下一个回合，故只提示未知。
        setStopReceipt({ kind: "unknown" });
      }
    } finally {
      if (stopActionEpochRef.current === actionEpoch) {
        stoppingRef.current = false;
        setStopping(false);
      }
    }
  };

  /** 输入区就地停止键（2026-11-22 用户口径，权威）：
   *  - 触发场景 A：本地投递在途（`sendingRef`）→ 中止本次投递；
   *  - 触发场景 B：原生回合 running 且可停止（`stopAvailable`）→ 终止回合；
   *  - 两者都在 → **同步**都停（各自如实呈现结论，互不掩盖）；
   *  - 两者都不在 → **直接 return，零请求**（不发 stop、不改账本、不弹错误）。
   *
   *  判定顺序按用户规格：先看权威视图 running，再看本地在途闸 `sendingRef`。
   *  中止投递只停**本端等待**，不改写服务端可能已发生的事实：作废本次发送代际
   *  （迟到回执不落槽位），锚与发送锁保留 → 走既有「结果未确认，只读核对」路径，
   *  绝不宣称零写出、绝不自动重发。 */
  const handleInlineStop = (): void => {
    const delivering = sendingRef.current;
    if (!stopInlineVisible) return; // 结构性无行为：按钮本就不该在场
    if (!stopAvailable && !delivering) return; // 双保险：无运行无投递 ⇒ 零请求
    if (delivering) {
      sendAbortRef.current?.abort();
      sendAbortRef.current = null;
      sendEpochRef.current += 1; // 迟到回执作废（锚保留，结果按未确认处理）
      sendingRef.current = false;
      setSending(false);
    }
    if (stopAvailable) void stopCurrent();
  };

  const recoverReselect = async (context: NonNullable<typeof lastCandidateRef.current>) => {
    reselectCheckRef.current = context.requestId;
    try {
      for (let attempt = 0; attempt < 8; attempt += 1) {
        if (lastCandidateRef.current !== context || context.sessionId !== sessionIdRef.current)
          return;
        try {
          const result = await readOpenCodeReselectReceipt(context.sessionId, context.requestId);
          if (lastCandidateRef.current !== context || context.sessionId !== sessionIdRef.current)
            return;
          if (result?.state === "not_applied") {
            setReselectUnconfirmed(false);
            setReselectNotCommitted(true);
            lastCandidateRef.current = null;
            return;
          }
          if (result?.state === "committed") {
            if (!result.current) {
              lastCandidateRef.current = null;
              setReselectUnconfirmed(false);
              setReselectSuperseded(true);
              return;
            }
            if (
              result.current &&
              result.bindingToken === context.bindingToken &&
              (await reselect(result.bindingToken, result.routeVersion))
            ) {
              if (
                lastCandidateRef.current === context &&
                context.sessionId === sessionIdRef.current
              ) {
                setReselectUnconfirmed(false);
                setReceipt(null);
                lastCandidateRef.current = null;
              }
            }
            return;
          }
        } catch {
          // 核对失败时仍可能在途，后续只重读本次回执。
        }
        await new Promise<void>((resolve) => setTimeout(resolve, 500));
      }
    } finally {
      if (reselectCheckRef.current === context.requestId) reselectCheckRef.current = null;
    }
  };

  const chooseCurrentTarget = async () => {
    if (
      reselectingRef.current ||
      (reselectUnconfirmed && lastCandidateRef.current?.sessionId === sessionId)
    )
      return;
    const selectedSession = sessionId;
    const selectedBinding = readBindingRef.current;
    const selectedVersion = view?.routeVersion ?? viewErrorRouteVersion(error);
    const epoch = sendBindingEpochRef.current;
    reselectingRef.current = true;
    setReselecting(true);
    setReselectError(false);
    setReselectSuperseded(false);
    setReselectUnconfirmed(false);
    setReselectNotCommitted(false);
    lastCandidateRef.current = null;
    const deadline = AbortSignal.timeout(OPENCODE_RESELECT_POST_TIMEOUT_MS);
    try {
      const preview = await previewOpenCodeCandidate(selectedSession, deadline);
      if (sessionIdRef.current !== selectedSession || sendBindingEpochRef.current !== epoch) return;
      if (
        !window.confirm(
          i18n.t("mobile.openCodeNative.reselectConfirmTarget", {
            directory: preview.candidate.directory,
            service: preview.candidate.serviceId,
            pid: preview.candidate.pid,
          })
        )
      )
        return;
      if (sessionIdRef.current !== selectedSession || sendBindingEpochRef.current !== epoch) return;
      const context = {
        sessionId: selectedSession,
        bindingToken: preview.candidate.bindingToken,
        requestId: newOpenCodeCreateRequestId(),
      };
      lastCandidateRef.current = context;
      await enterOpenCodeNative(
        selectedSession,
        preview.previousRoute === undefined
          ? selectedBinding
          : (preview.previousRoute?.bindingToken ?? null),
        preview.previousRoute === undefined
          ? selectedVersion
          : (preview.previousRoute?.routeVersion ?? null),
        preview.previewToken,
        deadline,
        OPENCODE_RESELECT_POST_TIMEOUT_MS,
        context.requestId
      );
      if (lastCandidateRef.current !== context || sessionIdRef.current !== selectedSession) return;
      setReselectUnconfirmed(true);
      void recoverReselect(context);
    } catch (e) {
      if (sessionIdRef.current !== selectedSession || sendBindingEpochRef.current !== epoch) return;
      const context = lastCandidateRef.current;
      if (
        context &&
        (deadline.aborted ||
          isAbortError(e) ||
          !(e instanceof ApiError) ||
          e.status === null ||
          e.status >= 500 ||
          [OC_RESPONSE_INVALID_MARKER, "OC_STORE_FAILED"].includes(String(e.data?.reasonCode)))
      ) {
        setReselectUnconfirmed(true);
        void recoverReselect(context);
      } else {
        setReselectError(true);
      }
    } finally {
      reselectingRef.current = false;
      if (sessionIdRef.current === selectedSession) setReselecting(false);
    }
  };

  const switchAgent = async () => {
    if (
      !owned ||
      view === null ||
      !agentChoice ||
      agentSwitching ||
      agentSwitchingRef.current ||
      !writeGateOpen ||
      hasUnresolvedAction(sessionId, "set_agent", sessionId, view.bindingToken)
    )
      return;
    if (view.capabilities.set_agent?.allowed !== true) return;
    agentSwitchingRef.current = true;
    setAgentSwitching(true);
    setAgentNotice(null);
    setAgentError(false);
    setAgentReadStale(false);
    const actionBinding = view.bindingToken;
    const actionEpoch = agentActionEpochRef.current;
    const actionId = newNativeActionId();
    rememberActionBinding(actionId, actionBinding);
    addActionAnchor(sessionId, {
      actionId,
      kind: "set_agent",
      targetId: agentChoice,
    });
    try {
      const result = await submitOpenCodeAction({
        actionId,
        sessionId,
        bindingToken: view.bindingToken,
        operation: { kind: "set_agent", agent: agentChoice },
      });
      if (isConclusiveState(result.state)) {
        removeConcludedAnchor(sessionId, actionId);
      }
      if (agentActionEpochRef.current !== actionEpoch || readBindingRef.current !== actionBinding)
        return;
      if (result.state === "settled" && result.reasonCode === "agent_switched") {
        // R3-slim：成功不再挂常驻回执——下拉框随 `agentView.current` 更新即为结果；
        // 只保留失败原因（就地显示在模式行）。
        setAgentNotice(null);
        refresh();
        // 切换**已定案**：此后读当前模式失败只说明「这次没读到」，不是切换失败。
        // 单独兜住，避免落进下面的 catch 报成「模式切换失败或结果未知」误导用户重试。
        try {
          const next = await fetchOpenCodeAgents(sessionId, undefined, view.bindingToken);
          if (
            agentActionEpochRef.current !== actionEpoch ||
            readBindingRef.current !== actionBinding
          )
            return;
          setAgentView(next);
          setAgentChoice(next.current ?? agentChoice);
        } catch {
          if (
            agentActionEpochRef.current !== actionEpoch ||
            readBindingRef.current !== actionBinding
          )
            return;
          setAgentReadStale(true);
        }
      } else {
        setAgentError(true);
      }
    } catch (err) {
      const code = err instanceof ApiError ? nativeErrorCode(err) : null;
      if (
        err instanceof ApiError &&
        err.status !== null &&
        err.status < 500 &&
        isDefiniteRefusal(code)
      ) {
        removeConcludedAnchor(sessionId, actionId);
      }
      if (agentActionEpochRef.current !== actionEpoch || readBindingRef.current !== actionBinding)
        return;
      if (err instanceof ApiError && err.status !== null && err.status < 500) {
        if (code === "OC_ACTION_CONFLICT") {
          // 「等当前回合结束再切换」是可执行的失败原因：放失败槽就地显示。
          setAgentNotice(i18n.t("mobile.openCodeNative.mode.modeBusy"));
        } else {
          setAgentError(true);
        }
      } else {
        setAgentError(true);
      }
    } finally {
      if (agentActionEpochRef.current === actionEpoch) {
        agentSwitchingRef.current = false;
        setAgentSwitching(false);
      }
    }
  };

  let stopCopy: string | null = null;
  if (stopReceipt !== null) {
    if (stopReceipt.kind === "interrupted")
      stopCopy = i18n.t("mobile.openCodeNative.stop.interrupted");
    else if (stopReceipt.kind === "idle") stopCopy = i18n.t("mobile.openCodeNative.stop.idle");
    else if (stopReceipt.kind === "unknown")
      stopCopy = i18n.t("mobile.openCodeNative.stop.unknown");
    else if (stopReceipt.kind === "verifiedNotSent")
      stopCopy = i18n.t("mobile.openCodeNative.stop.verifiedNotSent");
    else {
      const key = stopReceipt.reason ? nativeErrorKey(stopReceipt.reason) : null;
      stopCopy = i18n.t("mobile.openCodeNative.stop.failed", {
        reason: key ? i18n.t(key) : stopReceipt.reason || "unknown",
      });
    }
  }

  return (
    <div
      data-testid="opencode-native-panel"
      data-route={owned ? "opencode_native" : "unclaimed"}
      className="shrink-0 border-t border-[var(--cb)] px-3 py-2"
    >
      {/* R3-slim 会话级诊断收纳：只有确实存在诊断价值（绑定未确认 / 未认领 / 能力告警 /
          交回入口）时才出现这一个紧凑入口。健康会话**不渲染**公告与外框——正常态主聊天页
          只留正文、输入和当下可用的操作。会话级**阻断**仍单独挂在输入区旁（见下方 composer
          前），只解释一次。 */}
      {hasDiagnostics && (
        <div
          data-testid="native-connection-details"
          className="mb-2 rounded-xl border border-[var(--bd)] bg-[var(--cd)] px-3 py-2 text-xs"
        >
          <button
            type="button"
            data-testid="native-connection-details-toggle"
            aria-expanded={connectionDetailsOpen}
            onClick={() => setConnectionDetailsOpen((v) => !v)}
            className={SMALL_BUTTON}
          >
            {connectionDetailsOpen ? (
              <ChevronDown size={13} className="inline align-[-2px]" />
            ) : (
              <ChevronRight size={13} className="inline align-[-2px]" />
            )}
            <span className="ml-1">{i18n.t("mobile.openCodeNative.route.detailsToggle")}</span>
          </button>
          <div hidden={!connectionDetailsOpen}>
            {/* 路由声明：**已接管**属正常态，不再公告（R3-slim）；只有「未接管」才需要
              一句说明——它解释了为什么当前只读、下一步点什么。 */}
            {!owned && (
              <p data-testid="native-route-notice" className="mt-1.5 text-[var(--tx)]">
                {i18n.t("mobile.openCodeNative.route.unclaimed")}
              </p>
            )}
            {!allowLegacy && view?.routeReasonCode === "legacy_pending" && (
              <FrozenLegacyPending key={sessionId} sessionId={sessionId} />
            )}
            {/* D-N5-3：路由行仍 active 但本端被拒（候选重启+换代后旧绑定不可再生 →
              认领被拒）——读取失败区的重选入口不再可达，这里补一个常驻显式出口：
              预览→确认→携带候选凭据重选；死亡代际由服务端豁免旧绑定比对后登记新绑定。
              仅挂被拒面（legacy_pending），未接入（routeState=none）与只读归属未证不挂。 */}
            {!owned &&
              view !== null &&
              (view.routeReasonCode === "legacy_pending" ||
                (!allowLegacy && view.routeReasonCode === "returned_to_legacy")) &&
              view.routeVersion !== null &&
              view.routeVersion !== undefined &&
              view.bindingToken !== null &&
              view.bindingToken !== undefined && (
                <div className="mt-1 flex flex-wrap items-center gap-2">
                  <button
                    type="button"
                    data-testid="native-reselect-declined"
                    className={SMALL_BUTTON}
                    disabled={reselecting}
                    onClick={() => void chooseCurrentTarget()}
                  >
                    {i18n.t("mobile.openCodeNative.reselect")}
                  </button>
                  {reselectUnconfirmed && (
                    <p role="status">
                      {i18n.t("mobile.openCodeNative.reselectUnconfirmed")}{" "}
                      <button
                        type="button"
                        className="underline"
                        onClick={() => {
                          const context = lastCandidateRef.current;
                          if (context) void recoverReselect(context);
                        }}
                      >
                        {i18n.t("mobile.openCodeNative.reselectCheckRequest")}
                      </button>
                    </p>
                  )}
                  <span className="text-[var(--mut)]">
                    {i18n.t("mobile.openCodeNative.route.declinedReselectHint")}
                  </span>
                </div>
              )}
            {owned && routeReason !== null && (
              <p className="mt-0.5 text-[var(--mut)]">{routeReason}</p>
            )}
            {owned && (view?.serviceId !== undefined || view?.serviceUrl !== undefined) && (
              <p data-testid="native-fixed-service" className="mt-0.5 text-[var(--mut)]">
                {i18n.t("mobile.openCodeNative.route.fixedService", {
                  service: view?.serviceId ?? "?",
                  pid: view?.servicePid ?? "?",
                  url: view?.serviceUrl ?? "?",
                })}
              </p>
            )}
            {owned && view?.serviceUrl !== undefined && view?.directory !== undefined && (
              <div className="mt-0.5 text-xs">
                {/* M2：「必须重连归属才成立」只对**未绑定态**（桌面门在场、且原因不是
                  服务身份拒绝——身份失败重连解不掉）成立；普通启动已自动核验时
                  不再矛盾宣称。命令全文保留为诊断信息（规格 §3：放连接详情）。 */}
                {bindingRequired && !bindingBound && !identityRefused && (
                  <p className="text-[var(--mut)]">
                    {i18n.t("mobile.openCodeNative.route.reconnectHint")}
                  </p>
                )}
                {/* R3 复审 F2：命令全文只在这里（详情收纳区），绑定门不再重复展示 */}
                <code className="mt-1 block break-all text-[var(--mut)]">
                  {desktopReconnectCommand(view)}
                </code>
                <div className="mt-1 flex flex-wrap items-center gap-2">
                  <button
                    type="button"
                    data-testid="native-copy-reconnect"
                    className={SMALL_BUTTON}
                    onClick={() => {
                      const command = desktopReconnectCommand(view);
                      void navigator.clipboard?.writeText(command).then(
                        () => setReconnectCopied(true),
                        () => setReconnectCopied(false)
                      );
                    }}
                  >
                    {i18n.t("mobile.openCodeNative.route.copyReconnect")}
                  </button>
                  {/* 重新检测：命令执行、桌面连上后回到这里再确认绑定 */}
                  <button
                    type="button"
                    data-testid="native-bind-refresh"
                    className={SMALL_BUTTON}
                    onClick={() => refresh()}
                  >
                    {i18n.t("mobile.openCodeNative.route.bindingRefresh")}
                  </button>
                  {reconnectCopied && (
                    <span role="status" className="ml-1 text-[var(--mut)]">
                      {i18n.t("mobile.openCodeNative.route.reconnectCopied")}
                    </span>
                  )}
                </div>
              </div>
            )}
            {view?.routeReasonCode && !readOnlyNative && (
              <p>{i18n.t("mobile.openCodeNative.route.unclaimedReason")}</p>
            )}
            {/* R3 能力告警/兼容详情也收进连接详情（默认收起），不再常驻主聊天页 */}
            <p
              data-testid="native-limitations"
              className="mt-1.5 rounded-lg bg-amber-500/10 px-2 py-1.5 text-xs text-amber-700 dark:text-amber-300"
            >
              {i18n.t("mobile.openCodeNative.limitations")}
            </p>
            <div className="mt-1.5 flex flex-wrap items-center gap-2">
              {allowLegacy && (
                <button
                  type="button"
                  data-testid="native-return-legacy"
                  disabled={returning}
                  onClick={() => void handleReturnLegacy()}
                  className={SMALL_BUTTON}
                >
                  {returning
                    ? i18n.t("mobile.openCodeNative.route.returning")
                    : i18n.t("mobile.openCodeNative.route.returnLegacy")}
                </button>
              )}
              {view !== null && (
                <span className="text-[var(--mut)]">
                  {i18n.t("mobile.openCodeNative.runtime", { state: view.runtimeState })}
                </span>
              )}
            </div>
            {returnError !== null && (
              <p data-testid="native-return-error" className={RECEIPT_ERR}>
                {returnError}
              </p>
            )}
            {/* 两步确认（评审 Blocker 2）：只在后端明确报告「有换代前无法核对的动作时出现；
            文案必须写明**重复发送风险**，不得静默强推。 */}
            {allowLegacy && forceOffer !== null && (
              <div
                data-testid="native-force-offer"
                className="mt-1.5 rounded-lg border border-amber-500/40 bg-amber-500/10 px-2 py-1.5 text-xs"
              >
                <p data-testid="native-force-copy" className="text-amber-700 dark:text-amber-300">
                  {i18n.t("mobile.openCodeNative.route.forceOffer", { n: forceOffer })}
                </p>
                <div className="mt-1 flex flex-wrap items-center gap-2">
                  <button
                    type="button"
                    data-testid="native-force-confirm"
                    disabled={returning}
                    onClick={() => void handleReturnLegacy(true)}
                    className={SMALL_BUTTON}
                  >
                    {i18n.t("mobile.openCodeNative.route.forceConfirm")}
                  </button>
                  <button
                    type="button"
                    data-testid="native-force-cancel"
                    disabled={returning}
                    onClick={() => setForceOffer(null)}
                    className={SMALL_BUTTON}
                  >
                    {i18n.t("mobile.openCodeNative.route.forceCancel")}
                  </button>
                </div>
              </div>
            )}
          </div>
        </div>
      )}

      {/* 会话级停止**不再单独成按钮**：用户口径是「只有一个键」——运行中由输入区的
          发送键就地变成停止键（`native-stop-inline`）。这里只保留停止动作的回执，
          避免同屏出现两个「停止」控件。 */}
      {stopReceipt !== null && (
        <div className="mb-2 flex flex-wrap items-center gap-2">
          <span
            role="status"
            data-testid="native-stop-result"
            className={
              stopReceipt.kind === "error" || stopReceipt.kind === "unknown"
                ? RECEIPT_ERR
                : RECEIPT_NEUTRAL
            }
          >
            {stopCopy}
          </span>
        </div>
      )}
      {(readOnlyNative || (owned && view?.capabilities.set_agent?.allowed === true)) &&
        agentView !== null &&
        agentView.options.length > 0 && (
          /* 模式控制收成一行（R3-slim）：当前模式 + 目标下拉 + 切换按钮 + 只读模型名。
             成功切换后不再挂常驻回执——当前模式本身就是回执；失败就地给原因。
             模型读不到可靠字段时整段省略（不显示全局默认值或上一回合模型）。
             窄屏：文字与模型名可截断，按钮 shrink-0 不被挤掉。 */
          <div
            data-testid="native-mode"
            className="mb-2 flex flex-nowrap items-center gap-2 overflow-hidden text-xs"
          >
            <span
              data-testid="native-mode-current"
              className="min-w-0 shrink truncate text-[var(--mut)]"
            >
              {i18n.t("mobile.openCodeNative.mode.modeCurrent", {
                agent:
                  agentView.options.find((option) => option.id === agentView.current)?.name ??
                  agentView.current ??
                  "?",
              })}
            </span>
            <select
              data-testid="native-mode-select"
              aria-label={i18n.t("mobile.openCodeNative.mode.modeTitle")}
              value={agentChoice}
              disabled={
                !owned ||
                view?.capabilities.set_agent?.allowed !== true ||
                !writeGateOpen ||
                agentSwitching ||
                sending ||
                stopping
              }
              onChange={(event) => setAgentChoice(event.target.value)}
              className="min-w-0 shrink rounded border border-[var(--bd)] bg-[var(--cd)] px-1 py-0.5"
            >
              {agentView.options.map((option) => (
                <option key={option.id} value={option.id}>
                  {option.name}
                </option>
              ))}
            </select>
            <button
              type="button"
              data-testid="native-mode-switch"
              className={`${SMALL_BUTTON} shrink-0`}
              disabled={
                !owned ||
                view?.capabilities.set_agent?.allowed !== true ||
                !writeGateOpen ||
                agentSwitching ||
                (view !== null &&
                  hasUnresolvedAction(sessionId, "set_agent", sessionId, view.bindingToken)) ||
                sending ||
                stopping ||
                agentChoice === agentView.current
              }
              onClick={() => void switchAgent()}
            >
              {i18n.t("mobile.openCodeNative.mode.modeSwitch")}
            </button>
            {agentView.model !== undefined && agentView.model !== null && (
              <span
                data-testid="native-model"
                data-provider={agentView.model.providerID}
                data-model={agentView.model.id}
                title={i18n.t("mobile.openCodeNative.mode.modelReadOnly")}
                className="min-w-0 shrink truncate text-[var(--mut)]"
              >
                {agentView.modelName ?? agentView.model.id}
              </span>
            )}
            {agentError && (
              <span
                role="status"
                data-testid="native-mode-error"
                className="shrink-0 text-rose-600 dark:text-rose-400"
              >
                {i18n.t("mobile.openCodeNative.mode.modeError")}
              </span>
            )}
            {agentReadStale && (
              <span
                role="status"
                data-testid="native-mode-read-stale"
                className="shrink-0 text-[var(--mut)]"
              >
                {i18n.t("mobile.openCodeNative.mode.readStale")}
              </span>
            )}
            {/* 模式切换**在途**：就地一句「切换中，通常几秒内完成」，不写「未确认」、
                不催核对（2026-11-22 现场：1–2 秒的正常等待被显示成不能处理）。 */}
            {actionAnchors.some((item) => item.kind === "set_agent" && isInFlightRow(item)) && (
              <span
                role="status"
                data-testid="native-mode-inflight"
                className="shrink-0 text-[var(--mut)]"
              >
                {i18n.t("mobile.openCodeNative.unresolvedActions.inFlight", {
                  kind: i18n.t("mobile.openCodeNative.unresolvedActions.setAgent"),
                })}
              </span>
            )}
            {agentNotice !== null && (
              <span
                role="status"
                data-testid="native-mode-busy"
                className="shrink-0 text-[var(--mut)]"
              >
                {agentNotice}
              </span>
            )}
          </div>
        )}

      {/* R5 完整性标记：只有明确读到权威终止（complete=true）才不提示；
          旧/部分生产端没带这个字段时按不完整处理（fail-open），不得默认全量。
          D-N5-4：原因可归因为「部分消息超响应预算未载入」时如实换用超限标注，
          不与其它部分原因混同；两种取值下都不推断被跳过的数量或内容。 */}
      {view !== null && view.complete !== true && (
        <p
          data-testid="native-partial-history"
          role="note"
          className="mb-2 rounded-lg border border-amber-500/40 bg-amber-500/10 px-2 py-1.5 text-xs text-amber-700 dark:text-amber-300"
        >
          {view.partialReason === "OC_RESPONSE_TOO_LARGE"
            ? i18n.t("mobile.openCodeNative.messages.partialOversize")
            : i18n.t("mobile.openCodeNative.messages.partialHistory")}
        </p>
      )}

      {/* 读取失败：保留已有视图（部分投影不因一次失败变空） */}
      {error !== null && (
        <div data-testid="native-load-error" className={RECEIPT_ERR}>
          <p>{errorCopy(error)}</p>
          <button
            type="button"
            data-testid="native-retry"
            onClick={refresh}
            className={SMALL_BUTTON}
          >
            {i18n.t("mobile.openCodeNative.retry")}
          </button>
          {/* 恢复读失败（含普通 500）也必须保留显式重选入口：用户契约是
              「恢复不成功时仍有路可走」，不限定失败码。 */}
          <button
            type="button"
            data-testid="native-reselect"
            className={SMALL_BUTTON}
            disabled={reselecting}
            onClick={() => void chooseCurrentTarget()}
          >
            {i18n.t("mobile.openCodeNative.reselect")}
          </button>
          {reselectError && <p role="status">{i18n.t("mobile.openCodeNative.reselectFailed")}</p>}
          {reselectSuperseded && (
            <p role="status" data-testid="native-reselect-superseded">
              {i18n.t("mobile.openCodeNative.reselectSuperseded")}
            </p>
          )}
          {reselectUnconfirmed && (
            <p role="status" data-testid="native-reselect-unconfirmed">
              {i18n.t("mobile.openCodeNative.reselectUnconfirmed")}{" "}
              <button
                type="button"
                className="underline"
                onClick={() => {
                  const context = lastCandidateRef.current;
                  if (context) void recoverReselect(context);
                }}
              >
                {i18n.t("mobile.openCodeNative.reselectCheckRequest")}
              </button>
            </p>
          )}
          {reselectNotCommitted && (
            <p role="status" data-testid="native-reselect-not-committed">
              {i18n.t("mobile.openCodeNative.reselectNotCommitted")}
            </p>
          )}
        </div>
      )}
      {view === null && error === null && (
        <p data-testid="native-loading" className="mb-2 text-xs text-[var(--mut)]">
          {i18n.t("mobile.openCodeNative.loading")}
        </p>
      )}

      {/* 2026-11-22 用户口径：暂停不再有独立说明框/独立入口——它只在发送键上体现
          （禁用 + 简短文案），由 OpenCodeNativeQueue 的 onPausedChange 上报权威值。
          队列区块本身只在「需要处理」时出现（待发/未决/读取失败）。 */}
      <OpenCodeNativeQueue
        key={bindingScope}
        sessionId={sessionId}
        readBindingToken={readBinding}
        bindingToken={
          owned && bindingBound && view?.sessionId === sessionId ? view.bindingToken : null
        }
        refreshKey={`${receipt?.kind ?? ""}:${sending}`}
        writeBlockedCode={viewWriteBlockedCode}
        readFailed={readFailed || bindingRefused}
        onBindingRefusal={recordBindingRefusal}
        onPausedChange={(paused) => setOutboxPaused(paused)}
      />

      {/* 未收敛（锚在场）：**唯一**的出口块。发送在途时不显示（避免把当前动作
          误读成「上次」），发送一落地、锚仍在就立刻出现——包括核对失败之后 */}
      {unresolved && !sending && unresolvedActionId !== null && (
        <div data-testid={unresolvedTestId} className={RECEIPT_NEUTRAL}>
          <p>{unresolvedTitle}</p>
          {unresolvedDetail !== null && (
            <p data-testid="native-unresolved-detail" className="mt-0.5 text-[var(--mut)]">
              {unresolvedDetail}
            </p>
          )}
          <UnresolvedActions
            verifying={verifying}
            verifyTestId={unresolvedVerifyTestId}
            onVerify={() => void handleVerify(unresolvedActionId)}
            onAbandon={handleAbandon}
          />
          {/* patch3 D2：确认终态未知并解锁重选（文案如实：结果最终未知；后端对仍可
              核对的活代际行 fail-closed 拒绝，拒绝原因挂在下方短注）。 */}
          <div className="mt-1">
            <button
              type="button"
              data-testid="native-confirm-unknown"
              disabled={confirmUnknownBusy !== null}
              onClick={() => handleConfirmUnknownSend(unresolvedActionId)}
              className={SMALL_BUTTON}
            >
              {confirmUnknownBusy === unresolvedActionId
                ? i18n.t("mobile.openCodeNative.confirmUnknownBusy")
                : i18n.t("mobile.openCodeNative.confirmUnknown")}
            </button>
            {confirmUnknownError?.actionId === unresolvedActionId && (
              <p data-testid="native-confirm-unknown-error" role="status" className="mt-0.5">
                {confirmUnknownError.text}
              </p>
            )}
          </div>
        </div>
      )}

      {/* T7 范围（用户 2026-11-03 裁决）：只读原始 Ruleset，不提供权限档切换。
          这里明确“原始规则”，不冒充合并后的最终有效权限。 */}
      {owned &&
        view !== null &&
        (view.permissionRulesKnown === false || (view.permissionRules?.length ?? 0) > 0) && (
          <section
            data-testid="native-permission-rules"
            className="mb-2 rounded-lg bg-[var(--cb)] px-2 py-1 text-xs"
          >
            <h3 className="font-medium text-[var(--tx)]">
              {i18n.t("mobile.openCodeNative.permissionRules.title")}
            </h3>
            <p className="mt-0.5 text-[var(--mut)]">
              {i18n.t("mobile.openCodeNative.permissionRules.readOnly")}
            </p>
            {view.permissionRulesKnown === false ? (
              <p
                data-testid="native-permission-rules-unknown"
                className="mt-0.5 text-amber-700 dark:text-amber-300"
              >
                {i18n.t("mobile.openCodeNative.permissionRules.unknown")}
              </p>
            ) : (
              <ul className="mt-0.5 space-y-0.5">
                {(view.permissionRules ?? []).map((rule, index) => (
                  <li
                    key={`${rule.action}-${rule.resource}-${index}`}
                    className="text-[var(--mut)]"
                  >
                    {rule.action} {rule.resource} → {rule.effect}
                  </li>
                ))}
              </ul>
            )}
          </section>
        )}

      {actionListErrorSession === sessionId && (
        <p role="status" data-testid="native-action-list-error" className="mb-2 text-xs">
          {i18n.t("mobile.openCodeNative.actionListReadFailed")}{" "}
          <button type="button" className="underline" onClick={syncActionAnchors}>
            {i18n.t("mobile.openCodeNative.actionListRetry")}
          </button>
        </p>
      )}

      {/* 已受理（N4）不再常驻成列表（R3-slim）：已证受理属正常流转，消息进入原生记录
          后只在正文出现一次。`acknowledgedSends` 派生仍保留——它继续参与死代际重选入口
          的在场判定（不在本区渲染），只读核对入口由「未确认动作」的 send 锚承担。 */}
      {/* 未确认动作（G3）：answer/decide 的恢复锚。**不依赖** `view.pending` 与问答卡
          生命周期——卡片卸载、pending 消失、页面刷新后，这里仍是唯一的只读核对入口。
          没有重发按钮：核对只走 `verifyOpenCodeActionWithTimeout`（15s 超时，只读）。 */}
      {unresolvedAnchorRows.length > 0 && (
        <section data-testid="native-unresolved-actions" className="mb-2 space-y-1">
          <h3 className="text-xs font-medium text-[var(--tx)]">
            {i18n.t("mobile.openCodeNative.unresolvedActions.title")}
          </h3>
          {unresolvedAnchorRows.map((item) => {
            const note = anchorChecks[item.actionId];
            const rowVerifying = anchorVerifying === item.actionId;
            const kindLabel =
              item.kind === "answer"
                ? i18n.t("mobile.openCodeNative.unresolvedActions.answer")
                : item.kind === "decide"
                  ? i18n.t("mobile.openCodeNative.unresolvedActions.decide")
                  : item.kind === "stop"
                    ? i18n.t("mobile.openCodeNative.unresolvedActions.stop")
                    : item.kind === "retract"
                      ? i18n.t("mobile.openCodeNative.unresolvedActions.retract")
                      : item.kind === "send"
                        ? i18n.t("mobile.openCodeNative.unresolvedActions.send")
                        : i18n.t("mobile.openCodeNative.unresolvedActions.setAgent");
            return (
              <div
                key={item.actionId}
                role="group"
                aria-label={i18n.t("mobile.openCodeNative.unresolvedActions.rowLabel", {
                  kind: kindLabel,
                  targetId: item.targetId,
                })}
                data-testid="native-action-anchor"
                data-action-id={item.actionId}
                data-kind={item.kind}
                data-target-id={item.targetId}
                className="rounded-lg bg-[var(--cb)] px-2 py-1 text-xs"
              >
                <p className="text-[var(--tx)]">
                  {kindLabel}
                  {" · "}
                  {i18n.t("mobile.openCodeNative.unresolvedActions.target", {
                    targetId: item.targetId,
                  })}
                </p>
                {note === "failed" && (
                  <p
                    data-testid="native-action-verify-failed"
                    role="status"
                    aria-live="polite"
                    className="mt-0.5 text-[var(--mut)]"
                  >
                    {i18n.t("mobile.openCodeNative.unresolvedActions.checkFailed")}
                  </p>
                )}
                {note === "notFound" && (
                  <p
                    data-testid="native-action-verify-not-found"
                    role="status"
                    aria-live="polite"
                    className="mt-0.5 text-[var(--mut)]"
                  >
                    {i18n.t("mobile.openCodeNative.unresolvedActions.notFound")}
                  </p>
                )}
                {/* 2026-11-22 现场：模式切换**在途**（`dispatching`，通常 1–2 秒）也被写成
                    「操作结果尚未确认」，看起来像失败。在途只说明「还没回来」，不是「可能没生效」：
                    这一类改为「切换中」，不挂「未确认」措辞，也不催用户去核对。
                    只有真正没有结论（unconfirmed/无账本）时才保留原措辞。 */}
                {item.kind === "set_agent" &&
                  (item.ledger?.state === "dispatching" || item.ledger?.state === "queued") && (
                    <p data-testid="native-action-inflight" className="mt-0.5 text-[var(--mut)]">
                      {i18n.t("mobile.openCodeNative.unresolvedActions.inFlight", {
                        kind: i18n.t("mobile.openCodeNative.unresolvedActions.setAgent"),
                      })}
                    </p>
                  )}
                {((item.kind === "stop" && stopReceipt?.kind !== "unknown") ||
                  (item.kind === "set_agent" &&
                    item.ledger?.state !== "dispatching" &&
                    item.ledger?.state !== "queued")) && (
                  <p
                    data-testid="native-action-unknown-disposition"
                    className="mt-0.5 text-[var(--mut)]"
                  >
                    {i18n.t(
                      allowLegacy
                        ? "mobile.openCodeNative.unresolvedActions.unknownDisposition"
                        : "mobile.openCodeNative.unresolvedActions.unknownDispositionNative"
                    )}
                  </p>
                )}
                <button
                  type="button"
                  data-testid="native-action-verify"
                  data-action-id={item.actionId}
                  disabled={rowVerifying}
                  onClick={() => void handleAnchorVerify(item)}
                  className={SMALL_BUTTON}
                >
                  {rowVerifying
                    ? i18n.t("mobile.openCodeNative.unresolvedActions.verifying")
                    : i18n.t("mobile.openCodeNative.unresolvedActions.verify")}
                </button>
                {/* patch3 D2：确认终态未知并解锁重选（死代际行的用户出口；后端 fail-closed）。 */}
                {(!item.ledger || deadActionIds.has(item.actionId)) && (
                  <button
                    type="button"
                    data-testid="native-action-confirm-unknown"
                    data-action-id={item.actionId}
                    disabled={confirmUnknownBusy !== null}
                    onClick={() => handleConfirmUnknownRow(item)}
                    className={`${SMALL_BUTTON} ml-2`}
                  >
                    {confirmUnknownBusy === item.actionId
                      ? i18n.t("mobile.openCodeNative.confirmUnknownBusy")
                      : i18n.t("mobile.openCodeNative.confirmUnknown")}
                  </button>
                )}
                {confirmUnknownError?.actionId === item.actionId && (
                  <p
                    data-testid="native-action-confirm-unknown-error"
                    role="status"
                    className="mt-0.5"
                  >
                    {confirmUnknownError.text}
                  </p>
                )}
              </div>
            );
          })}
        </section>
      )}

      {/* 待处理：表单挂可写卡（能力放行时）；审批点击决定即确认（只发 once/reject） */}
      {view !== null && view.pending.length > 0 && (
        <section data-testid="native-pending" className="mb-2 space-y-1">
          <h3 className="text-xs font-medium text-[var(--tx)]">
            {i18n.t("mobile.openCodeNative.pending.title")}
          </h3>
          {view.pending.map((p) =>
            p.kind === "form" ? (
              answerCardMounted ? (
                <div key={`form-${p.id}`} className="space-y-1">
                  {/* 绑定阻断：一句连接指针（原因只在绑定门），卡保持挂载、只锁写 */}
                  {!bindingBound && !readOnlyNative && (
                    <p data-testid="native-pending-form-binding" className="text-[var(--mut)]">
                      {i18n.t("mobile.openCodeNative.pending.formBindingBlocked")}
                    </p>
                  )}
                  <OpenCodeQuestionGroupCard
                    sessionId={sessionId}
                    bindingToken={view.bindingToken}
                    form={p}
                    writeBlocked={
                      !writeGateOpen ||
                      hasUnresolvedAction(sessionId, "answer", p.id, view.bindingToken)
                    }
                    onChanged={refresh}
                    onFormSettled={handleFormSettled}
                  />
                </div>
              ) : (
                <PendingFormRow
                  key={`form-${p.id}`}
                  item={p}
                  blockedReason={view.capabilities.answer_group?.reasonCode ?? null}
                />
              )
            ) : (
              <PendingPermissionRow
                key={`permission-${p.id}`}
                item={p}
                allowedDecisions={
                  owned &&
                  writeGateOpen &&
                  !hasUnresolvedAction(sessionId, "decide", p.id, view.bindingToken)
                    ? DECISION_WORDS.filter((word) => p.decisions.includes(word))
                    : []
                }
                bindingBlocked={!bindingBound && !readOnlyNative}
                readBlocked={
                  readFailed ||
                  readOnlyNative ||
                  hasUnresolvedAction(sessionId, "decide", p.id, view.bindingToken)
                }
                deciding={deciding[p.id] ?? null}
                verifying={decisionVerifying === p.id}
                note={decisionNotes[p.id] ?? null}
                onDecide={(word) => void handleDecide(p, word)}
                onVerify={handleDecisionVerify}
              />
            )
          )}
        </section>
      )}

      {/* 原生 inbox（排队/插队的待消费项）：属正常流转，**默认折叠**成一行入口——
          展开后才有条目与撤回；不让它常驻占住聊天页（R3-slim）。
          queue / steer 文案必须区分（语义不同：一个是排队，一个是打断）。 */}
      {view !== null && view.inbox.length > 0 && (
        <section data-testid="native-inbox" className="mb-2 space-y-1">
          <button
            type="button"
            data-testid="native-inbox-toggle"
            aria-expanded={inboxOpen}
            onClick={() => setInboxOpen((value) => !value)}
            className={SMALL_BUTTON}
          >
            {i18n.t("mobile.openCodeNative.inbox.toggle", { count: view.inbox.length })}
          </button>
          {inboxOpen && (
            <div className="space-y-1">
              {view.inbox.map((item) => (
                <InboxRow
                  key={item.id}
                  item={item}
                  canRetract={
                    retractAllowed &&
                    writeGateOpen &&
                    !actionAnchors.some(
                      (anchor) =>
                        anchor.kind === "retract" &&
                        anchor.targetId === item.id &&
                        (retractAnchorBindingsView.get(anchor.actionId) ?? view.bindingToken) ===
                          view.bindingToken
                    )
                  }
                  canPromote={
                    item.delivery === "queue" &&
                    owned &&
                    writeGateOpen &&
                    view.capabilities.promote_inbox?.allowed === true
                  }
                  retractBlockedReason={view.capabilities.retract?.reasonCode ?? null}
                  retracting={retractingId === item.id}
                  note={retractNotes[item.id] ?? null}
                  onRetract={() => void handleRetract(item.id)}
                  onPromote={() => void handlePromote(item.id)}
                />
              ))}
            </div>
          )}
        </section>
      )}

      {/* 详情页使用同一正文区域；独立输入面板保留可展开窗口。 */}
      {view !== null && (
        <ConversationLocation target={conversationTarget} visible={showConversation}>
          <section data-testid="native-messages" className="mb-2">
            {!conversationTarget && (
              <button
                type="button"
                data-testid="native-messages-toggle"
                onClick={() => setShowMessages((v) => !v)}
                className={SMALL_BUTTON}
              >
                {showMessages
                  ? i18n.t("mobile.openCodeNative.messages.hide")
                  : i18n.t("mobile.openCodeNative.messages.show", { count: view.messages.length })}
              </button>
            )}
            {(showMessages || conversationTarget) && (
              <div className="mt-1 rounded-xl bg-[var(--cd)] px-2 py-2">
                {!view.complete && (
                  <p
                    data-testid="native-messages-partial"
                    className="mb-1 text-xs text-[var(--mut)]"
                  >
                    {i18n.t("mobile.openCodeNative.messages.partial")}
                  </p>
                )}
                {view.messages.length === 0 ? (
                  <p className="text-xs text-[var(--mut)]">
                    {i18n.t("mobile.openCodeNative.messages.empty")}
                  </p>
                ) : (
                  <ul className="space-y-1">
                    {view.messages.map((m) => {
                      // 关联键 = 原生 `messageID + callID`（官方 metadata 同形），不是投影 seq：
                      // 不要把非数字 id 强转成 Number（会得到 NaN，结果永远挂不上）。
                      const anchorId = view.messageIds?.[String(m.seq)];
                      const anchorCall = view.toolCallIds?.[String(m.seq)];
                      return (
                        <NativeMessageRow
                          key={m.seq}
                          message={m}
                          nativeId={anchorId}
                          renderBody={renderBody}
                          processFold={processFold}
                          files={view.files?.[String(m.seq)] ?? []}
                          onPreview={(name, path) => setAttachmentPreview({ name, path })}
                          questionResults={
                            m.kind === "tool-call" &&
                            m.toolName === "question" &&
                            anchorId &&
                            anchorCall
                              ? (questionResultsBySeq.get(`${anchorId}:${anchorCall}`) ?? [])
                              : []
                          }
                        />
                      );
                    })}
                  </ul>
                )}
              </div>
            )}
          </section>
        </ConversationLocation>
      )}

      {/* 回执槽（发送结论；与 inbox/待处理的读回结论分列） */}
      {receiptNode}

      {/* G4 patch7（GC）：已接管详情错误区「重新选择当前目标」入口。绑定死亡/核对受阻
          形态在场才渲染（判据见 ownedReselectEntry）；点击走既有 preview→route/native
          版本 CAS 链（复用 chooseCurrentTarget，routeVersion 缺省态兼容），成功后绑定
          重建、面板回 owned、零重投。 */}
      {ownedReselectEntry && (
        <div
          data-testid="native-reselect-owned"
          className="mb-2 rounded-lg border border-amber-500/40 bg-amber-500/10 px-2 py-1.5 text-xs"
        >
          <p className="text-amber-700 dark:text-amber-300">
            {i18n.t("mobile.openCodeNative.reselectOwnedHint")}
          </p>
          <button
            type="button"
            data-testid="native-reselect-owned-button"
            className={`${SMALL_BUTTON} mt-1`}
            disabled={reselecting}
            onClick={() => void chooseCurrentTarget()}
          >
            {i18n.t("mobile.openCodeNative.reselect")}
          </button>
          {reselectError && <p role="status">{i18n.t("mobile.openCodeNative.reselectFailed")}</p>}
          {reselectSuperseded && (
            <p role="status" data-testid="native-reselect-superseded">
              {i18n.t("mobile.openCodeNative.reselectSuperseded")}
            </p>
          )}
          {reselectUnconfirmed && (
            <p role="status" data-testid="native-reselect-owned-unconfirmed">
              {i18n.t("mobile.openCodeNative.reselectUnconfirmed")}{" "}
              <button
                type="button"
                className="underline"
                onClick={() => {
                  const context = lastCandidateRef.current;
                  if (context) void recoverReselect(context);
                }}
              >
                {i18n.t("mobile.openCodeNative.reselectCheckRequest")}
              </button>
            </p>
          )}
        </div>
      )}

      {/* R3 会话级阻断门：连接问题**只在这一处**解释（具体原因 + 一个可执行入口），
          就近挂在受影响的输入区上方；恢复（desktopBound）后整块消失，
          草稿、未确认锚与正式结果都不受影响。
          R3 复审 F2：重连命令全文/复制不在门里——门上的唯一入口直接打开连接详情，
          命令全文、复制与「重新检测」都集中在这份按需展开的详情里。 */}
      {bindingGateVisible && (
        <section
          data-testid="native-binding-gate"
          className="mb-2 rounded-lg border border-amber-500/40 bg-amber-500/10 px-2 py-1.5 text-xs"
        >
          {/* I1：门上的原因与下一步必须与实际原因相符——身份拒绝给身份文案 +
                重新读取（每次只读投影都会重跑身份核验）；桌面未连接才给重连指引。 */}
          <p className="text-amber-700 dark:text-amber-300">
            {identityRefused
              ? i18n.t("mobile.openCodeNative.route.bindingGateIdentityRefused")
              : i18n.t("mobile.openCodeNative.route.bindingGate")}
          </p>
          {identityRefused ? (
            <button
              type="button"
              data-testid="native-binding-recheck"
              className={SMALL_BUTTON}
              onClick={() => refresh()}
            >
              {i18n.t("mobile.openCodeNative.retry")}
            </button>
          ) : (
            <button
              type="button"
              data-testid="native-binding-details"
              className={SMALL_BUTTON}
              onClick={() => setConnectionDetailsOpen(true)}
            >
              {i18n.t("mobile.openCodeNative.route.bindingDetails")}
            </button>
          )}
        </section>
      )}

      {/* 原生输入区：上传中禁发；正文为空禁发（仅附件消息本批不放行） */}
      {attachments.length > 0 && (
        <div data-testid="native-attachments" className="mb-1 flex flex-wrap gap-1">
          {attachments.map((a) => (
            <span
              key={a.id}
              data-testid="native-attachment"
              data-status={a.status}
              className="rounded-full bg-[var(--cb)] px-2 py-0.5 text-xs text-[var(--tx)]"
            >
              {a.name}
              {a.status === "uploading" ? " …" : ""}
              {a.status === "ready" && a.excludeWarning ? (
                <span
                  data-testid="native-attachment-exclude-warning"
                  className="ml-1 text-amber-700 dark:text-amber-300"
                >
                  {i18n.t("mobile.openCodeNative.composer.excludeWarning")}
                </span>
              ) : null}
              {a.status === "failed" ? (
                <span
                  data-testid="native-attachment-error"
                  data-error-code={a.errorCode ?? "upload_failed"}
                  className="ml-1 text-[var(--mut)]"
                >
                  {a.errorCode === "upload_gate_refused" && a.refusalCode
                    ? i18n.t(
                        nativeErrorKey(a.refusalCode) ??
                          "mobile.openCodeNative.composer.attachmentUploadFailed"
                      )
                    : i18n.t(attachmentErrorKey(a.errorCode ?? "upload_failed"))}
                </span>
              ) : null}
              <button
                type="button"
                data-testid="native-attachment-remove"
                onClick={() => {
                  attachmentControllers.current.get(a.id)?.abort();
                  attachmentControllers.current.delete(a.id);
                  attachmentBudget.current.delete(a.id);
                  setAttachments((prev) => prev.filter((x) => x.id !== a.id));
                }}
                className="ml-1 text-[var(--mut)]"
              >
                ×
              </button>
            </span>
          ))}
        </div>
      )}
      {uploading && (
        <p data-testid="native-upload-gating" className="mb-1 text-xs text-[var(--mut)]">
          {i18n.t("mobile.openCodeNative.composer.uploadGating")}
        </p>
      )}
      {/* 失败附件 = 发送键被禁用：说明「为什么点不了」，出口是移除/重选（不新增二次确认） */}
      {uploadFailed && (
        <p
          data-testid="native-upload-failed-gating"
          role="note"
          className="mb-1 text-xs text-rose-600 dark:text-rose-400"
        >
          {i18n.t("mobile.openCodeNative.composer.uploadFailedGating")}
        </p>
      )}
      {!sendAllowed &&
        view !== null &&
        !sendBlockedDuplicatedByGate &&
        view.routeReasonCode !== "OC_INSTANCE_OWNERSHIP_UNVERIFIED" && (
          <p data-testid="native-send-blocked" className={RECEIPT_ERR}>
            {sendBlockedCopy}
          </p>
        )}
      {/* 未收敛的锚在场：发送键锁住（草稿仍可编辑），出口只有核对/放弃。
          R3-slim：**同一动作只允许一处说明**——未收敛块已在渲染（它自己描述了「结果未确认」
          并给出核对/放弃出口）时，这里不再重复一遍锁定文案；只有未收敛块没在场
          （发送在途等）才用这句解释为什么发送键不可用。 */}
      {unresolved && !sending && unresolvedActionId === null && (
        <p data-testid="native-send-locked" className={RECEIPT_NEUTRAL}>
          {i18n.t("mobile.openCodeNative.composer.lockedByUnresolved")}
        </p>
      )}
      {/* 附件说明不再常驻一行（2026-11-22 用户口径）：改为「+」旁小问号按需展开。
          原生通道确有额外上限（5 MiB / 合计 20 MiB / 最多 10 个），作为同一条提示内
          的补充句，不另起常驻元素。 */}
      <div className="flex items-end gap-2">
        {/* 与旧输入区（Claude Code / Codex 等）**完全一致的附件区**：
            加号在上、小问号在正下方（`flex-col items-center`），图标规格也一致
            （`Plus size={20}` / `CircleHelp size={10}`，同一个 muted 圆形 hover 样式）。
            不并排、不用文字「+」、不另起尺寸。 */}
        <span className="flex shrink-0 flex-col items-center gap-0.5">
          <button
            type="button"
            data-testid="native-attach"
            aria-label={i18n.t("mobile.openCodeNative.composer.attach")}
            aria-expanded={attachHintOpen}
            disabled={!writeGateOpen}
            onClick={() => fileInputRef.current?.click()}
            className="shrink-0 rounded-full p-1 text-[var(--mut)] hover:bg-[var(--cb)] disabled:opacity-40 dark:hover:bg-[var(--btnp)]"
          >
            <Plus size={20} />
          </button>
          <button
            type="button"
            data-testid="native-attach-help"
            aria-label={i18n.t("mobile.composer.attachHelpAria")}
            aria-expanded={attachHintOpen}
            onClick={() => setAttachHintOpen((open) => !open)}
            className="shrink-0 rounded-full p-0.5 text-[10px] leading-none text-[var(--mut)] hover:bg-[var(--cb)] dark:hover:bg-[var(--btnp)]"
          >
            <CircleHelp size={10} />
          </button>
        </span>
        <input
          ref={fileInputRef}
          data-testid="native-file-input"
          disabled={!writeGateOpen}
          type="file"
          multiple
          hidden
          onChange={(e) => {
            const files = Array.from(e.target.files ?? []);
            e.target.value = "";
            if (files.length > 0) addFiles(files);
          }}
        />
        <textarea
          data-testid="native-input"
          value={draft}
          onChange={(e) => {
            draftRevision.current += 1;
            setDraft(e.target.value);
          }}
          rows={2}
          maxLength={10000}
          placeholder={i18n.t("mobile.openCodeNative.composer.placeholder")}
          className="max-h-40 min-h-9 flex-1 resize-none rounded-2xl bg-[var(--cb)] px-3 py-1.5 text-sm text-[var(--tx)] outline-none"
        />
        {/* 2026-11-22 用户口径（只有一个发送键）：
            - 空闲（未运行、未投递）：发送键；
            - 运行中或投递在途：**同一位置**就地变停止键（外圈圆 + 内部方块）；
            - 两者都不在：停止键根本不渲染（结构性无行为，零请求）。
            插队不再有常驻按钮（改由队列行触发）。 */}
        {/* 2026-11-23 用户现场修正：运行中**停止键不占用发送键的位置**。
            之前用 XOR 让停止键替换发送键，导致「运行中想再发一条排队」完全没有入口
            （文字在框里却按不到发送）。现在两键并存：
            - 发送键**始终**在场（运行中也允许提交 → 上游 `delivery:"queue"` 排队）；
            - 停止键只在「运行中或投递在途」时出现在其右侧，无运行无投递时不渲染
              （结构性零行为）。 */}
        <button
          type="button"
          data-testid="native-send"
          data-paused={outboxPaused ? "true" : undefined}
          // 暂停**不**禁用发送键：暂停应只影响「派发」，消息仍能存进本地队列（后端支持）。
          // 禁用会迫使暂停期间连草稿都留不住，与「暂停 = 先留在兔维斯」的语义相反。
          disabled={!canSend}
          title={outboxPaused ? i18n.t("mobile.openCodeNative.composer.pausedShort") : undefined}
          aria-label={
            outboxPaused ? i18n.t("mobile.openCodeNative.composer.pausedShort") : undefined
          }
          className="rounded-full bg-[var(--btnp)]/20 px-3 py-1.5 text-sm text-[var(--tx)] disabled:opacity-40"
          onClick={() => void handleSend("queue")}
        >
          {outboxPaused
            ? i18n.t("mobile.openCodeNative.composer.pausedShort")
            : i18n.t("mobile.openCodeNative.composer.send")}
        </button>
        {stopInlineVisible && (
          <button
            type="button"
            // 沿用既有 `native-stop` testid，让所有既有停止断言/能力门继续指向这个真实控件。
            data-testid="native-stop"
            data-inline="true"
            aria-label={i18n.t("mobile.openCodeNative.composer.stopInline")}
            title={i18n.t("mobile.openCodeNative.composer.stopInline")}
            onClick={() => void handleInlineStop()}
            className="flex h-9 w-9 shrink-0 items-center justify-center rounded-full bg-[var(--btnp)]/20 text-[var(--tx)]"
          >
            <span className="flex h-6 w-6 items-center justify-center rounded-full border-2 border-current">
              <span className="block h-2.5 w-2.5 rounded-[1px] bg-current" />
            </span>
          </button>
        )}
      </div>
      {/* 暂停只体现在发送键本身上（禁用 + 简短文案），不建任何独立说明框/恢复按钮。 */}
      {attachHintOpen && (
        <p
          data-testid="native-attach-hint"
          className="mt-1 rounded-lg bg-[var(--cbg)] px-2 py-1.5 text-[11px] leading-4 text-[var(--mut)]"
        >
          {i18n.t("mobile.openCodeNative.composer.attachHintNative")}
        </p>
      )}
      {attachmentPreview !== null && (
        <NativeAttachmentPreview
          sessionId={sessionId}
          name={attachmentPreview.name}
          path={attachmentPreview.path}
          onClose={() => setAttachmentPreview(null)}
        />
      )}
    </div>
  );
}

const RECEIPT_OK =
  "mb-2 rounded-lg bg-emerald-500/10 px-3 py-2 text-xs text-emerald-700 dark:text-emerald-300";
const RECEIPT_NEUTRAL = "mb-2 rounded-lg bg-[var(--cb)] px-3 py-2 text-xs text-[var(--tx)]";
const RECEIPT_ERR =
  "mb-2 rounded-lg bg-rose-500/10 px-3 py-2 text-xs text-rose-600 dark:text-rose-400";
const SMALL_BUTTON =
  "rounded-full bg-[var(--cb)] px-2 py-0.5 text-xs text-[var(--tx)] disabled:opacity-40";

/** 核对失败的中性原因词：**只描述「为什么没读到」**，不带任何「写没写出」的断言
 *  （那是发送语义的结论，R-1②）。未收录的码原样作为受控码展示。 */
function verifyCauseCopy(code: string | null): string {
  if (code === null) return i18n.t("mobile.openCodeNative.receipt.causeNetwork");
  switch (code.toUpperCase()) {
    case "OC_TRANSPORT_TIMEOUT":
    case "OC_TRANSPORT_FAILED":
    case "OC_UNAUTHORIZED":
      return i18n.t("mobile.openCodeNative.receipt.causeNetwork");
    case "OC_RESPONSE_INVALID":
    case "OC_RESPONSE_TOO_LARGE":
    case "OC_HTTP_STATUS":
      return i18n.t("mobile.openCodeNative.receipt.causeResponse");
    default:
      return i18n.t("mobile.openCodeNative.receipt.causeCode", { code });
  }
}

/** 理由词后缀（受控词表外的词原样展示，便于排障；不编文案）。 */
function reasonSuffix(code: string | null): string {
  return code === null ? "" : `（${code}）`;
}

/** 未收敛动作的两个**显式**出口：只读核对 / 明确放弃。没有第三个（重发）出口。 */
function UnresolvedActions({
  verifying,
  verifyTestId,
  onVerify,
  onAbandon,
}: {
  verifying: boolean;
  verifyTestId: string;
  onVerify: () => void;
  onAbandon: () => void;
}) {
  return (
    <div className="mt-1 flex flex-wrap gap-2">
      <button
        type="button"
        data-testid={verifyTestId}
        disabled={verifying}
        onClick={onVerify}
        className={SMALL_BUTTON}
      >
        {verifying
          ? i18n.t("mobile.openCodeNative.receipt.verifying")
          : i18n.t("mobile.openCodeNative.receipt.verify")}
      </button>
      {/* 放弃**不**被在途核对禁用：核对请求可能永挂（无超时/abort），那时两个出口
          全不可用等于把用户锁死在页面上。放在途核对靠 handleAbandon 的代际闸作废。 */}
      <button
        type="button"
        data-testid="native-abandon"
        onClick={onAbandon}
        className={SMALL_BUTTON}
      >
        {i18n.t("mobile.openCodeNative.receipt.abandon")}
      </button>
    </div>
  );
}

/** 一组已答结果，挂在**原提问位置**（R5b 归位）。
 *
 *  纪律：
 *  - 沿用聊天里既有「过程折叠」形态：默认折叠，标题只写状态与答案条数；展开看全答案。
 *  - 只渲染官方 `answered` 的字段值；`cancelled`/`pending`/`unknown` 各说各话，不展示答案。
 *  - **内部字段不上屏**：不展示 formId、`q0` 之类键名。答案用「选项 label —— 题目文本」
 *    的形态渲染（题目文本来自同一投影里的 question 工具调用参数）；题目文本取不到时
 *    只显示答案值，绝不显示 `q0`。
 *  - 读不到（404/网络/他会话）只说这次没读到；不推断不存在或未答。 */
function QuestionResultBlock({ state }: { state: FormResultState }) {
  const [expanded, setExpanded] = useState(false);
  if (state.kind === "loading") {
    return (
      <p
        data-testid="native-form-result"
        data-state="loading"
        className="text-xs text-[var(--mut)]"
      >
        {i18n.t("mobile.openCodeNative.formResults.loading")}
      </p>
    );
  }
  if (state.kind === "error") {
    return (
      <p
        data-testid="native-form-result"
        data-state="not_read"
        role="status"
        aria-live="polite"
        className="text-xs text-amber-700 dark:text-amber-300"
      >
        {i18n.t("mobile.openCodeNative.formResults.notRead")}
      </p>
    );
  }
  const { result } = state;
  // 只渲染后端派生的可读答案（题目 + 选项 label）；`answers` 的键/值是协议内部形态，
  // 不直接上屏。缺 `readableAnswers`（旧后端）时按「无答案可展示」处理。
  const readable = result.readableAnswers ?? [];
  const statusLabel = formStatusLabel(result.status);
  const summary =
    result.status === "answered"
      ? i18n.t("mobile.openCodeNative.formResults.answeredSummary", {
          status: statusLabel,
          count: readable.length,
        })
      : statusLabel;
  return (
    <div
      data-testid="native-form-result"
      data-status={result.status}
      className="rounded-lg border border-[var(--cb)] bg-[var(--cb)]/50 px-2 py-1 text-xs"
    >
      <button
        type="button"
        data-testid="native-form-result-toggle"
        aria-expanded={expanded}
        onClick={() => setExpanded((value) => !value)}
        className="-mx-1 flex w-[calc(100%+8px)] items-center gap-1 rounded-lg px-1 py-0.5 text-left text-xs text-[var(--mut)] hover:bg-[var(--cb)]/60"
      >
        {expanded ? <ChevronDown size={13} /> : <ChevronRight size={13} />}
        <span data-testid="native-form-result-status" data-status={result.status}>
          {summary}
        </span>
      </button>
      {expanded &&
        (result.status !== "answered" ? (
          <p className="mt-1 text-[var(--mut)]">
            {i18n.t("mobile.openCodeNative.formResults.noAnswerForStatus", { status: statusLabel })}
          </p>
        ) : readable.length === 0 ? (
          <p data-testid="native-form-result-empty" className="mt-1 text-[var(--mut)]">
            {i18n.t("mobile.openCodeNative.formResults.emptyAnswers")}
          </p>
        ) : (
          <ul className="mt-1 space-y-0.5">
            {readable.map((item, index) => (
              <li
                key={`${item.title}-${index}`}
                data-testid="native-form-result-answer"
                className="break-words text-[var(--tx)]"
              >
                <span className="text-[var(--mut)]">{item.title}</span>
                <span className="mx-1 text-[var(--mut)]">→</span>
                {item.answer}
              </li>
            ))}
          </ul>
        ))}
    </div>
  );
}

/** 结果内容稳定键：同一 formId 的 loading→ok 变化需要重挂，其余状态不变。 */
function stateKey(state: FormResultState): string {
  if (state.kind === "loading") return "loading";
  if (state.kind === "error") return "error";
  return `${state.result.formId}:${state.result.status}`;
}

/** 结果状态文案：四个词各有独立文案（unknown 与 cancelled 绝不合并）。 */
function formStatusLabel(status: FormResultStatus): string {
  switch (status) {
    case "answered":
      return i18n.t("mobile.openCodeNative.formResults.status.answered");
    case "cancelled":
      return i18n.t("mobile.openCodeNative.formResults.status.cancelled");
    case "pending":
      return i18n.t("mobile.openCodeNative.formResults.status.pending");
    case "unknown":
      return i18n.t("mobile.openCodeNative.formResults.status.unknown");
  }
}

/** 待答表单只读行：**能力未放行**时才走这里——如实说明限制与受控原因词，
 *  不挂可写卡（不假装能答）。`form.state` 恒 unknown，不当成「待答/已答」的证据。
 *  R3 复审 F1：绑定阻断的表单**不走这里**（题卡保持挂载、面板只加一句连接指针），
 *  本行只剩能力归因一种形态。 */
function PendingFormRow({
  item,
  blockedReason,
}: {
  item: Extract<PendingView, { kind: "form" }>;
  blockedReason: string | null;
}) {
  return (
    <div
      data-testid="native-pending-form"
      data-id={item.id}
      className="rounded-lg bg-[var(--cb)] px-2 py-1 text-xs"
    >
      <p className="text-[var(--tx)]">{item.title}</p>
      <p className="text-[var(--mut)]">{i18n.t("mobile.openCodeNative.pending.formReadOnly")}</p>
      <p data-testid="native-pending-form-blocked" className="text-amber-700 dark:text-amber-300">
        {blockedReason === null
          ? i18n.t("mobile.openCodeNative.questions.capabilityBlockedGeneric")
          : i18n.t("mobile.openCodeNative.questions.capabilityBlocked", {
              reason: blockedReason,
            })}
      </p>
      <p data-testid="native-pending-form-state" className="text-[var(--mut)]">
        {i18n.t("mobile.openCodeNative.pending.stateUnknown", { state: item.state })}
      </p>
    </div>
  );
}

/** 审批行：**点击决定本身就是明确确认**（不弹统一二次确认）。
 *
 *  - 只展示后端 `decisions` 里的 `once`/`reject`（其它词不代猜、不展示成按钮）；后端就是从
 *    同一张能力表派生 `decisions` 的，所以这里不再重复判一次能力键（评审 M-6：那条分支
 *    永远不可达）。能力门仍保留在 `handleDecide` 里作最后一道闸。
 *  - 已提交/已定案/未确认后**不再渲染**写入口（不是禁用）：未确认只留只读核对，绝不重发。 */
function PendingPermissionRow({
  item,
  allowedDecisions,
  bindingBlocked,
  readBlocked,
  deciding,
  verifying,
  note,
  onDecide,
  onVerify,
}: {
  item: Extract<PendingView, { kind: "permission" }>;
  allowedDecisions: DecisionWord[];
  bindingBlocked: boolean;
  /** I2：服务读取失败期间决定按钮已被撤下（会话级原因在面板错误块，行内不冒认归因）。 */
  readBlocked: boolean;
  deciding: DecisionWord | null;
  verifying: boolean;
  note: DecisionReceipt | null;
  onDecide: (decision: DecisionWord) => void;
  onVerify: (requestId: string, actionId: string) => Promise<void>;
}) {
  const locked =
    note !== null &&
    (note.kind === "submitted" || note.kind === "settled" || note.kind === "unknown");

  return (
    <div
      data-testid="native-pending-permission"
      data-id={item.id}
      className="rounded-lg bg-[var(--cb)] px-2 py-1 text-xs"
    >
      <p className="text-[var(--tx)]">{item.title}</p>
      {allowedDecisions.length > 0 && !locked && (
        <div className="mt-0.5 flex flex-wrap gap-2">
          {allowedDecisions.map((word) => (
            <button
              key={word}
              type="button"
              data-testid="native-decision"
              data-decision={word}
              disabled={deciding !== null}
              onClick={() => onDecide(word)}
              className={SMALL_BUTTON}
            >
              {deciding === word
                ? i18n.t("mobile.openCodeNative.pending.deciding")
                : word === "once"
                  ? i18n.t("mobile.openCodeNative.pending.decisionOnce")
                  : word === "reject"
                    ? i18n.t("mobile.openCodeNative.pending.decisionReject")
                    : i18n.t("mobile.openCodeNative.pending.decisionAlways")}
            </button>
          ))}
        </div>
      )}
      {/* 没有可提交的决定且还没有回执：R3 归因区分——绑定未确认是**连接受阻**
          （一句状态指针，原因只在绑定门），后端真没列出决定词才是「本端未放行」；
          I2：读取失败是**会话级**连接问题，面板错误块已解释一次，行内不再冒认
          「本端未放行」的原因（只吞掉这句错误归因，不新增第二处解释）。 */}
      {allowedDecisions.length === 0 && note === null && (
        <>
          {bindingBlocked ? (
            <p data-testid="native-pending-permission-binding" className="mt-0.5 text-[var(--mut)]">
              {i18n.t("mobile.openCodeNative.pending.permissionBindingBlocked")}
            </p>
          ) : (
            !readBlocked && (
              <p
                data-testid="native-pending-permission-readonly"
                className="mt-0.5 text-[var(--mut)]"
              >
                {i18n.t("mobile.openCodeNative.pending.permissionReadOnly")}
              </p>
            )
          )}
        </>
      )}
      {note !== null && (
        <DecisionNote requestId={item.id} note={note} verifying={verifying} onVerify={onVerify} />
      )}
    </div>
  );
}

/** 审批回执：一态一句；未确认只给只读核对（没有重发出口）。 */
function DecisionNote({
  requestId,
  note,
  verifying,
  onVerify,
}: {
  requestId: string;
  note: DecisionReceipt;
  verifying: boolean;
  onVerify: (requestId: string, actionId: string) => Promise<void>;
}) {
  const copy = (() => {
    switch (note.kind) {
      case "submitted":
        return i18n.t("mobile.openCodeNative.pending.decisionSubmitted");
      case "settled":
        return i18n.t("mobile.openCodeNative.pending.decisionSettled");
      case "unknown":
        return i18n.t("mobile.openCodeNative.pending.decisionUnconfirmed");
      case "rejected":
        return note.text;
    }
  })();
  return (
    <div className="mt-0.5">
      <p data-testid="native-decision-receipt" data-kind={note.kind} className="text-[var(--tx)]">
        {copy}
      </p>
      {/* 零写出/受控码：让用户能区分「确定没生效」与「网络抖动」（评审 M-7） */}
      {"code" in note && note.code != null && note.code !== "" && (
        <p data-testid="native-decision-receipt-code" className="text-[var(--mut)]">
          {i18n.t("mobile.openCodeNative.pending.decisionCode", { code: note.code })}
        </p>
      )}
      {note.kind === "unknown" && (
        <button
          type="button"
          data-testid="native-decision-verify"
          disabled={verifying}
          onClick={() => void onVerify(requestId, note.actionId)}
          className={SMALL_BUTTON}
        >
          {verifying
            ? i18n.t("mobile.openCodeNative.pending.decisionVerifying")
            : i18n.t("mobile.openCodeNative.pending.decisionVerify")}
        </button>
      )}
    </div>
  );
}

/** inbox 行：`queue` = 排队待消费；`steer` = 打断注入（不同语义，必须两句话）。
 *  撤回能力未放行时**不渲染撤回钮**，改为如实说明限制（不说「稍后再试」）。 */
function InboxRow({
  item,
  canRetract,
  canPromote,
  retractBlockedReason,
  retracting,
  note,
  onRetract,
  onPromote,
}: {
  item: InboxView;
  canRetract: boolean;
  /** 插队（PATCH delivery=steer）：只对仍在排队的 queue 项开放。 */
  canPromote: boolean;
  retractBlockedReason: string | null;
  retracting: boolean;
  note: "retracted" | "stillThere" | null;
  onRetract: () => void;
  onPromote: () => void;
}) {
  return (
    <div
      data-testid="native-inbox-item"
      data-id={item.id}
      data-delivery={item.delivery}
      className="rounded-lg bg-[var(--cb)] px-2 py-1 text-xs"
    >
      <p className="text-[var(--tx)]">{item.text}</p>
      <div className="mt-0.5 flex flex-wrap items-center gap-2">
        <span className="text-[var(--mut)]">
          {/* 两种投递方式**必须两句话**：queue 是已交付、等回合结束；steer 是已交付、
              正在打断当前回合。都**不是**「没发出去/还在本地待发」——这一点曾是现场误判的根因。 */}
          {item.state === "steering" || item.delivery === "steer"
            ? i18n.t("mobile.openCodeNative.inbox.steer")
            : i18n.t("mobile.openCodeNative.inbox.queue")}
        </span>
        {canPromote && (
          <button
            type="button"
            data-testid={`inbox-promote-${item.id}`}
            onClick={onPromote}
            className="underline"
          >
            {i18n.t("mobile.openCodeNative.outbox.prioritize")}
          </button>
        )}
        {canRetract ? (
          <button
            type="button"
            data-testid="native-inbox-retract"
            disabled={retracting}
            onClick={onRetract}
            className={SMALL_BUTTON}
          >
            {retracting
              ? i18n.t("mobile.openCodeNative.inbox.retracting")
              : i18n.t("mobile.openCodeNative.inbox.retract")}
          </button>
        ) : (
          <span data-testid="native-inbox-retract-blocked" className="text-[var(--mut)]">
            {retractBlockedReason === null
              ? // m-2：能力整键缺席时没有受控原因词——用通用句，不显示「（—）」
                i18n.t("mobile.openCodeNative.inbox.retractBlockedGeneric")
              : i18n.t("mobile.openCodeNative.inbox.retractBlocked", {
                  reason: retractBlockedReason,
                })}
          </span>
        )}
      </div>
      {note !== null && (
        <p
          data-testid="native-inbox-note"
          data-note={note}
          className={
            note === "retracted" ? "text-emerald-700 dark:text-emerald-300" : RECEIPT_NEUTRAL
          }
        >
          {note === "retracted"
            ? i18n.t("mobile.openCodeNative.inbox.retracted")
            : i18n.t("mobile.openCodeNative.inbox.stillThere")}
        </p>
      )}
    </div>
  );
}

/** 原生窗口里的正文行：**复用既有正文渲染器 `renderBody`**（与 SessionDetail 同一实现），
 *  按 `kind` 分派而不是按 `role`——投影把 thinking/tool-call/tool-result 都记作
 *  `role:"assistant"`，只按 role 分支会把工具输出渲染成 assistant 散文（丢 `<pre>`、
 *  丢工具名、丢思考的弱化样式）。
 *
 *  R5c：附件行挂在**对应消息**下（按 `seq` 关联），只展示后端下发的受控元数据；
 *  `previewPath` 缺席时**没有**预览按钮（不伪造入口）。
 */
function FrozenLegacyPending({ sessionId }: { sessionId: string }) {
  const [result, setResult] = useState<number | "failed" | null>(null);
  useEffect(() => {
    const controller = new AbortController();
    void fetchFrozenOpenCodePendingCount(sessionId, controller.signal).then(
      (count) => {
        if (!controller.signal.aborted) setResult(count);
      },
      () => {
        if (!controller.signal.aborted) setResult("failed");
      }
    );
    return () => controller.abort();
  }, [sessionId]);

  if (result === null) return null;
  return (
    <p role="status" data-testid="native-frozen-legacy-pending" className="mt-1.5">
      {result === "failed"
        ? i18n.t("mobile.openCodeNative.route.frozenPendingReadFailed")
        : i18n.t("mobile.openCodeNative.route.frozenPending", { count: result })}
    </p>
  );
}

function NativeMessageRow({
  message,
  nativeId,
  renderBody,
  processFold,
  files,
  onPreview,
  questionResults = [],
}: {
  message: SessionMessage;
  nativeId?: string;
  renderBody: (message: SessionMessage) => ReactNode;
  processFold?: NativeProcessFold;
  files: NativeFileView[];
  onPreview: (name: string, path: string) => void;
  /** R5b 归位：按官方 `metadata.tool` 关联到这条提问的正式结果（可能不止一组）。 */
  questionResults?: FormResultState[];
}) {
  const isUser = message.role === "user";
  // 详情页的一键入口与行内按钮共用状态；独立面板沿用 wire 默认。
  const toggleable = isProcessKind(message.kind);
  const [expandedOverride, setExpandedOverride] = useState<boolean | null>(null);
  const collapsed =
    toggleable && (processFold?.isCollapsed(message) ?? expandedOverride ?? message.collapsed);
  const toggleCollapsed = () => {
    if (processFold) processFold.onToggle(message);
    else setExpandedOverride((v) => !(v ?? message.collapsed));
  };
  return (
    <li
      data-testid={`native-msg-${message.seq}`}
      data-native-id={nativeId}
      data-native-seq={message.seq}
      data-role={message.role}
      data-kind={message.kind}
      className={isUser ? "flex flex-col items-end" : "flex flex-col items-start"}
    >
      <div
        data-testid={`native-msg-body-${message.seq}`}
        className={
          isUser
            ? "max-w-[85%] rounded-2xl bg-[var(--btnp)]/15 px-3 py-1 text-sm text-[var(--tx)]"
            : "max-w-[85%] rounded-2xl bg-[var(--cb)]/60 px-3 py-1 text-sm text-[var(--tx)]"
        }
      >
        {toggleable ? (
          <>
            <button
              type="button"
              data-testid={`native-msg-${message.seq}-toggle`}
              aria-expanded={!collapsed}
              onClick={toggleCollapsed}
              className="-mx-1 flex w-[calc(100%+8px)] items-center gap-1 rounded-lg px-1 py-0.5 text-left text-xs text-[var(--mut)] hover:bg-[var(--cb)]/60 dark:hover:bg-[var(--btnp)]"
            >
              {collapsed ? <ChevronRight size={13} /> : <ChevronDown size={13} />}
              <span className="truncate">{collapsedLabel(message)}</span>
            </button>
            {!collapsed && <div className="mt-1">{renderBody(message)}</div>}
          </>
        ) : (
          renderBody(message)
        )}
      </div>
      {questionResults.length > 0 && (
        <div
          data-testid={`native-question-results-${message.seq}`}
          className="mt-1 w-full max-w-[85%] space-y-1"
        >
          {questionResults.map((state) => (
            <QuestionResultBlock key={`qr-${message.seq}-${stateKey(state)}`} state={state} />
          ))}
        </div>
      )}
      {files.length > 0 && (
        <ul
          data-testid={`native-msg-files-${message.seq}`}
          className="mt-1 flex max-w-[85%] flex-col items-end gap-1"
        >
          {files.map((file, index) => {
            // 预览只展示能读懂的文本或图片；未知类型仍保留附件元数据。
            const previewable =
              file.mime?.startsWith("text/") ||
              file.mime?.startsWith("image/") ||
              file.mime === "application/json";
            const previewPath = previewable ? file.previewPath : undefined;
            return (
              <li
                key={`${file.name}-${index}`}
                data-testid="native-file"
                data-name={file.name}
                data-mime={file.mime ?? ""}
                data-size={file.size ?? ""}
                className="flex max-w-full flex-wrap items-center gap-1 rounded-full bg-[var(--cb)] px-2 py-0.5 text-xs text-[var(--tx)]"
              >
                <span data-testid="native-file-name" className="max-w-[14rem] truncate">
                  {file.name}
                </span>
                {file.mime !== undefined && (
                  <span data-testid="native-file-mime" className="text-[var(--mut)]">
                    {file.mime}
                  </span>
                )}
                {file.size !== undefined && (
                  <span data-testid="native-file-size" className="text-[var(--mut)]">
                    {formatBytes(file.size)}
                  </span>
                )}
                {previewPath !== undefined && (
                  <button
                    type="button"
                    data-testid="native-file-preview"
                    data-path={previewPath}
                    onClick={() => onPreview(file.name, previewPath)}
                    className="text-[var(--mut)] underline"
                  >
                    {i18n.t("mobile.openCodeNative.messages.preview")}
                  </button>
                )}
              </li>
            );
          })}
        </ul>
      )}
    </li>
  );
}

/** 附件预览的加载态（`FilePayload` 成功态原样保留，便于图片 object URL 回收）。 */
type AttachmentPreviewState =
  { phase: "loading" } | { phase: "error" } | { phase: "ok"; payload: FilePayload };

/** 原生附件预览（R5c）：只读 `previewPath`，复用既有受控入口 `fetchFile`——
 *  不自行拼 URL、不读任意本机路径（后端只下发受控目录内的 canonical 路径）。 */
function NativeAttachmentPreview({
  sessionId,
  name,
  path,
  onClose,
}: {
  sessionId: string;
  name: string;
  path: string;
  onClose: () => void;
}) {
  const [state, setState] = useState<AttachmentPreviewState>({ phase: "loading" });
  useEffect(() => {
    let alive = true;
    let objectUrl: string | null = null;
    const ctrl = new AbortController();
    setState({ phase: "loading" });
    fetchFile(sessionId, path, undefined, ctrl.signal)
      .then((payload) => {
        if (!alive) {
          if (payload.kind === "image") URL.revokeObjectURL(payload.url);
          return;
        }
        if (payload.kind === "image") objectUrl = payload.url;
        setState({ phase: "ok", payload });
      })
      .catch(() => {
        if (alive && !ctrl.signal.aborted) setState({ phase: "error" });
      });
    return () => {
      alive = false;
      ctrl.abort();
      if (objectUrl !== null) URL.revokeObjectURL(objectUrl);
    };
  }, [sessionId, path]);

  return (
    <div
      data-testid="native-file-preview-overlay"
      className="fixed inset-0 z-50 flex items-center justify-center bg-black/50 p-4"
    >
      <div
        data-testid="native-file-preview-dialog"
        role="dialog"
        aria-modal="true"
        aria-label={i18n.t("mobile.preview.aria", { name })}
        className="flex max-h-[80vh] w-full max-w-lg flex-col overflow-hidden rounded-xl bg-[var(--cd)]"
      >
        <div className="flex items-center justify-between gap-2 border-b border-[var(--cb)] px-3 py-2">
          <span className="truncate text-sm text-[var(--tx)]">{name}</span>
          <button
            type="button"
            data-testid="native-file-preview-close"
            onClick={onClose}
            className={SMALL_BUTTON}
          >
            {i18n.t("mobile.preview.close")}
          </button>
        </div>
        <div className="overflow-auto p-3 text-sm text-[var(--tx)]">
          {state.phase === "loading" ? (
            <p data-testid="native-file-preview-loading">
              {i18n.t("mobile.openCodeNative.messages.previewLoading")}
            </p>
          ) : state.phase === "error" ? (
            <p data-testid="native-file-preview-error">
              {i18n.t("mobile.openCodeNative.messages.previewFailed")}
            </p>
          ) : state.payload.kind === "image" ? (
            <img
              data-testid="native-file-preview-image"
              onError={() => setState({ phase: "error" })}
              src={state.payload.url}
              alt={name}
              className="max-w-full"
            />
          ) : (
            <pre data-testid="native-file-preview-text" className="break-words whitespace-pre-wrap">
              {state.payload.content}
            </pre>
          )}
        </div>
      </div>
    </div>
  );
}

function ConversationLocation({
  target,
  visible,
  children,
}: {
  target?: HTMLElement | null;
  visible: boolean;
  children: ReactNode;
}) {
  if (!visible) return null;
  return target ? createPortal(children, target) : children;
}
