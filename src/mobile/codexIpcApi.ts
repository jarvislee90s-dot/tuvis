// Codex Desktop IPC 客户端（Task 9）：原生协议投影的 fetch / SSE / 应答 / 队列。
//
// **DTO 纪律**：本文件的 TS 类型与 Rust `services/codex_desktop_ipc/types.rs` 的
// serde 输出**逐字段相同**（camelCase 字段；枚举值 snake_case），任何一侧漂移
// 都是契约破坏。要点：
// - `requestKey` 永远是 string（服务端把 Desktop 的整数/字符串 pending id 编码为
//   `n:<十进制>` / `s:<原串>` 的无损可逆形态）——浏览器侧 2^53 以上整数只有
//   字符串形态无损，**绝不**把它解析回 number；
// - 客户端只回显 requestKey/contextToken；`ownerClientId` / `targetClientId` /
//   raw method 由服务端 Owner Registry 解析（S3），HTTP 写请求一律不上送；
// - `unknown_delivery` 是 200 的**明确回执**（S5：不确定结果绝不自动重试、绝不
//   自动 fallback），不是网络错误——调用方不得按可重试异常处理。
import { ApiError } from "./api";

// ==== DTO（与 types.rs 逐字段对齐，勿漂移）====

/** 投递结果。`not_sent` = 保守判定零投递（写尝试开始前即失败）；`unknown_delivery`
 *  = 已发出但结果未知——禁止自动重试（S5）。 */
export type CodexDelivery = "not_sent" | "accepted" | "rejected" | "unknown_delivery";

/** 动作账本状态（codex_actions.state）。 */
export type CodexActionState =
  "queued" | "reserved" | "accepted" | "rejected" | "unknown" | "cancelled";

/** 控制路由。`headless_compat` 仅普通消息可降级；审批/回答绝不降级为普通消息。 */
export type ControlRoute = "desktop_ipc" | "headless_compat" | "none";

/** 能力档位（spec F8）：用户看到的是实际能力。`read_only` = #172 解耦新增的只读档
 *  （读链已取证、写未开放；旧后端不会下发该值）。 */
export type ControlAvailability =
  "controllable" | "send_only" | "read_only" | "compat_only" | "unavailable";

/** 能力三态（#172/T9b）：把服务端下发的**四档**压成用户看得懂的三态。
 *  - `verified` = 已验证：该 build 有真机证据（可控制 / 可发送）；
 *  - `compat` = 兼容探测（实验）：未知 build——读链已取证但写未开放，或只剩兼容
 *    发送路径（须显式 opt-in 的实验开关）；
 *  - `locked` = 未解锁：服务端明确判不可用。 */
export type CodexCapabilityTier = "verified" | "compat" | "locked";

/**
 * 四档 → 三态的**唯一映射源**（#172/T9b）。
 *
 * 为什么要有这一层：四档是协议内的细分（`read_only` 与 `compat_only` 对用户没有
 * 区别——都是「这个 build 我们没有完整证据」），三态才是用户能据此决策的口径。
 * **徽标渲染与测试都只引用本函数**，禁止在别处各写一份对照表（两份对照表必然漂移，
 * 而漂移出来的档位文案会直接撒谎）。
 */
export function codexCapabilityTier(availability: ControlAvailability): CodexCapabilityTier {
  switch (availability) {
    case "controllable":
    case "send_only":
      return "verified";
    case "read_only":
    case "compat_only":
      return "compat";
    case "unavailable":
      return "locked";
    default:
      // 词表外取值（后端漂移）不猜「已验证」——落到最保守档（不宣称任何能力）
      return "locked";
  }
}

/** 探测只读事实的状态档（#172/T9a 后端下发；小写形态与 serde 对齐）。 */
export type ProbeStatusState = "idle" | "probing" | "probed" | "inconclusive" | "failed";

/**
 * 探测只读事实（#172/T9a）：这台 build 的**只读**探测到哪一步了。
 *
 * 语义（在飞 / 账本结论 / 无记录 / 取不到）：
 *  - `probing`：探测在飞（服务端单飞器有在飞任务）；
 *  - `probed` / `inconclusive` / `failed`：账本 `state` 行的结论；
 *  - `idle` + `desktop_build_unverified`：该 build 没有任何探测记录；
 *  - `idle` + 无码：取不到（**不猜**）。
 *
 * 它是**只读事实**：不携带任何写语义，也不得由前端推断出「能不能操控」。
 */
export interface ProbeStatus {
  state: ProbeStatusState;
  /** 最后一次探测到哪一级（P0..P3）；无记录 → null。 */
  stage: "P0" | "P1" | "P2" | "P3" | null;
  /** 机器码（snake_case）；无 → null。 */
  reasonCode: string | null;
}

/** 探测级次取值域（P0..P3；脏数据不进）。 */
const PROBE_STAGES = new Set(["P0", "P1", "P2", "P3"]);

/** 探测状态取值域（与后端 serde 形态逐字对齐）。 */
const PROBE_STATES = new Set<ProbeStatusState>([
  "idle",
  "probing",
  "probed",
  "inconclusive",
  "failed",
]);

/**
 * 探测 reasonCode → 顶层 `errors` 段的 i18n 键（词表外 → null = **不猜**）。
 *
 * 动态码（`probe_failed:P2`）按**前缀**取基线码的键——`P2` 这样的 stage 不另设文案，
 * 由调用方把 stage 附在基线文案后面（见 `probeReasonStage`）。
 */
const PROBE_REASON_ERROR_KEYS: Record<string, string> = {
  desktop_build_unverified: "errors.desktopBuildUnverified",
  probe_failed: "errors.probeFailed",
  protocol_drift_suspected: "errors.protocolDriftSuspected",
  probe_inconclusive: "errors.probeInconclusive",
};

/** 探测 reasonCode → `errors` 段 i18n 键；词表外（含未知动态码）→ null。 */
export function probeReasonErrorKey(reasonCode: string | null | undefined): string | null {
  if (typeof reasonCode !== "string") return null;
  const base = reasonCode.split(":")[0].trim();
  if (base.length === 0) return null;
  return PROBE_REASON_ERROR_KEYS[base] ?? null;
}

/** 动态码里的 stage（`probe_failed:P2` → `P2`）；没有合法 stage → null。 */
export function probeReasonStage(reasonCode: string | null | undefined): string | null {
  if (typeof reasonCode !== "string") return null;
  const parts = reasonCode.trim().split(":");
  if (parts.length < 2) return null;
  const tail = parts[parts.length - 1].trim();
  return PROBE_STAGES.has(tail) ? tail : null;
}

/**
 * 探测事实的形状守卫（纵深防御，与 `asThreadProjection` 同款纪律）。
 *
 * 老后端**不下发**该字段 → `undefined`（按「无探测信息」处理，界面回到旧路径）；
 * 下发了但形状脏（状态枚举外 / stage 非法）→ 归一成安全值：状态非法整个丢弃
 * （拿不到就不猜），stage 非法降为 `null`。**任何情况下都不会让 undefined 崩**。
 */
export function asProbeStatus(v: unknown): ProbeStatus | undefined {
  if (v === null || typeof v !== "object") return undefined;
  const raw = v as { state?: unknown; stage?: unknown; reasonCode?: unknown };
  if (typeof raw.state !== "string" || !PROBE_STATES.has(raw.state as ProbeStatusState)) {
    return undefined;
  }
  const stage =
    typeof raw.stage === "string" && PROBE_STAGES.has(raw.stage)
      ? (raw.stage as ProbeStatus["stage"])
      : null;
  const reasonCode =
    typeof raw.reasonCode === "string" && raw.reasonCode.length > 0 ? raw.reasonCode : null;
  return { state: raw.state as ProbeStatusState, stage, reasonCode };
}

/** 待处理交互类别。未知 method → unsupported（actionable=false）。 */
export type PendingKind =
  | "command_approval"
  | "file_change_approval"
  | "permissions_approval"
  | "user_input"
  | "unsupported";

/** 完成的动作回执（HTTP 原样返回）。 */
export interface ActionReceipt {
  actionId: string;
  delivery: CodexDelivery;
  route: ControlRoute;
  turnId: string | null;
  envelopeId: string | null;
  reasonCode: string | null;
}

/** 动作视图（不携带正文/答案）。position = 当前设备视角的 FIFO 位置（1 起）。 */
export interface CodexActionView {
  actionId: string;
  state: CodexActionState;
  position: number | null;
  receipt: ActionReceipt | null;
  /** S3 投递阶段（serde default 向后兼容：旧后端缺省 → undefined，前端不猜）：
   *  `queued` 排队/校验等待 / `dispatching` 派发中（含前台已结束、后台收尾中的
   *  「待确认」）/ `settled` 账本终态。 */
  stage?: CodexDeliveryStage;
  /** P4 队列行正文预览（serde skip_serializing_if 向后兼容：旧后端缺省 →
   *  undefined，前端不渲染预览行，不猜）。内容 = 该排队消息 queued_text 原文
   *  （**未剥签名**，服务端已按字符边界截断 ≤120 字符）；仅 state=queued 的行
   *  有值，accepted/unknown/cancelled 行为 undefined。剥签名/单行化/截断等
   *  展示规约在 CodexQueuePanel.queueRowPreview。 */
  textPreview?: string;
}

/** S3 投递阶段（`CodexActionView.stage`；serde snake_case）。 */
export type CodexDeliveryStage = "queued" | "dispatching" | "settled";

/** 队列视图。blockedReasonCode 非空（如 compat_outstanding）= 暂停派发，必须展示说明。
 *  blockedActionIds = 在册屏障的动作清单（人工解除端点的靶向依据；旧后端载荷缺省）。
 *  canInterrupt = 本 Desktop build 是否已活体验证「打断当前回合」（interrupt 能力位）；
 *  false（或旧载荷缺省）时队列面板**不得**渲染打断按钮——渲染了也只会 503
 * （2026-10-22 复审 P2-2：无能力 build 按钮在源头隐藏）。 */
export interface CodexQueueView {
  actions: CodexActionView[];
  blockedReasonCode: string | null;
  blockedActionIds?: string[];
  canInterrupt?: boolean;
  /** S2：打断后持久暂停状态（serde default 向后兼容；唯一解除出口 = /codex/queue/resume）。 */
  queuePaused?: boolean;
  queuePauseReason?: string | null;
}

/** 「已进入桌面队列」发送回执 chip（MessageComposer 持有；actionId 是对账锚）。 */
export interface CodexQueuedReceipt {
  kind: "codexQueued";
  position: number;
  actionId: string;
}

/** codexQueued 回执对齐 codex 队列视图真值（2026-10-22 B1 收敛修复）：动作已不在
 *  队列（派发/撤回）→ null（chip 收敛清除）；位次变化 → 跟随；其余 → 原样返回。
 *  纯函数——轮询 effect 每 tick 调用，测试直接钉行为。 */
export function reconcileCodexQueuedReceipt(
  prev: CodexQueuedReceipt,
  view: CodexQueueView
): CodexQueuedReceipt | null {
  const mine = view.actions.find((a) => a.actionId === prev.actionId);
  if (!mine) return null;
  if (mine.position !== null && mine.position !== prev.position) {
    return { ...prev, position: mine.position };
  }
  return prev;
}

/** 平台能力集。false = 该 build 尚未活体验证，不表示协议实现不存在。 */
export interface CapabilitySet {
  send: boolean;
  state: boolean;
  command: boolean;
  permissions: boolean;
  blockingInput: boolean;
  interrupt: boolean;
}

/** 附件类别（Rust `ArtifactKind` 的 snake_case 取值）。 */
export type ArtifactKind = "image" | "document" | "external_image";

/** 附件来源（Rust `ArtifactSource` 的 snake_case 取值）：五类结构化引用 +
 *  用户 `image.url` 的独立外链分支。 */
export type ArtifactSource =
  "image_generation" | "local_image" | "mention" | "file_change" | "image_view" | "image_url";

/** 投影附件（与 Rust `ProjectedArtifact` 逐字段对齐）。可缺省信息用 null；
 *  `unavailableReason` 只表达「引用不可预览/打开」的机器码（展示文案由 UI 决定），
 *  摘要/元数据的有界截断不写入该字段；`fileKey` 由服务端按文件池同一口径生成，
 *  前端只用于关联，不自行推断宿主路径规则。 */
export interface ProjectedArtifact {
  artifactId: string;
  kind: ArtifactKind;
  source: ArtifactSource;
  path: string | null;
  url: string | null;
  fileKey: string | null;
  /** 来源内稳定序号（每个来源独立计数，先于尾窗裁剪统计）。 */
  ordinal: number;
  /** 回合内图片卡索引；只给可展示的本地图片，外链与文件 chip 为 null。 */
  imageIndexInTurn: number | null;
  summary: string | null;
  /** 原始状态标量：不映射为「已完成/成功」。 */
  sourceStatus: string | null;
  /** 原始变更类型标量：不映射为 add/modify/delete。 */
  changeKind: string | null;
  unavailableReason: string | null;
}

/** 投影消息（正文条目）。附件字段为兼容扩展：旧载荷缺省时由
 *  `asThreadProjection` 归一化为空数组/0/null，不崩溃。 */
export interface ProjectedMessage {
  id: string;
  role: string;
  kind: string;
  text: string;
  ts: number | null;
  /** 稳定回合身份；旧载荷缺省 → null。 */
  turnId?: string | null;
  /** 结构化附件；旧载荷缺省 → []。 */
  artifacts?: ProjectedArtifact[];
  /** 完整 canonical 回合的图片卡总数（尾窗裁剪不改变；UI 12 图限额另行展示）。 */
  turnImageCount?: number;
  /** 仅服务端预算省略的附件数；与 UI 的 12 图／6 chip 展示限额无关。 */
  omittedArtifactCount?: number;
  /** T03：正式答复摘要（正文不再重复内部信封标记）。 */
  interactionAnswer?: CodexInteractionAnswerSummary | null;
}

/** T02：逐操作键（serde snake_case）。 */
export type CodexOperation =
  | "send"
  | "command_approval"
  | "file_change_approval"
  | "blocking_input"
  | "async_answer_idle"
  | "interrupt"
  | "request_cancel"
  | "permissions_approval"
  | "mode_read"
  | "mode_set"
  | "permissions_read"
  | "permissions_set";

/** T02：证据来源（三档可解释来源 + 无证据）。 */
export type CodexEvidenceSource = "none" | "shipped" | "contract_matched" | "observed_success";

/** T03：版本能力支持状态（serde snake_case；稳定展示面，与 `allowed` 实时门分离）。 */
export type CodexSupportState = "supported" | "identifying" | "unmatched" | "not_implemented";

const SUPPORT_STATES = new Set<CodexSupportState>([
  "supported",
  "identifying",
  "unmatched",
  "not_implemented",
]);

/**
 * T03：支持态展示值。服务端 `support` 缺失/脏（旧载荷）时按 `evidence` 推导——
 * **仅用于展示**：绝不参与 allowed/资格判断（资格字段原样透传），也不产生任何
 * 新写资格（写门本就不在前端）。
 */
export function supportOf(status: CodexOperationStatus): CodexSupportState {
  if (status.support && SUPPORT_STATES.has(status.support)) return status.support;
  if (!status.implemented) return "not_implemented";
  return status.evidence !== "none" ? "supported" : "identifying";
}

/** T02：单操作统一状态（后端唯一事实；前端不自行拼按钮）。 */
export interface CodexOperationStatus {
  implemented: boolean;
  allowed: boolean;
  requiresAutoAdapt: boolean;
  evidence: CodexEvidenceSource;
  /** T03：版本能力支持状态。旧载荷缺省 → undefined（消费端经 supportOf 推导展示）。 */
  support?: CodexSupportState;
  /** R07（修复轮 1）：`support` 是否**沿用了上次识别结论的展示事实**（真实重连换代窗
   *  内、实时证据尚未装回）。`true` = 应如实显示「连接恢复中」。**纯展示**：不参与
   *  allowed/资格判定。旧载荷缺省 → undefined（按 false 处理）。 */
  supportInherited?: boolean;
  /** C03：`supportInherited` 为 true 时的包身份核对态——`true` = 本代际已对当前包
   *  完成**内容 SHA 核验**且一致（展示身份可当作已确认）；缺省 = 仅 stat 快筛通过
   *  （「待核对」，前端必须与已确认能力明确区分）。**纯展示**。 */
  supportIdentityVerified?: boolean;
  reasonCode: string | null;
}

/** T02：逐操作有效集合。`policyRevision` 是事实指纹（同一组事实恒等）。 */
export interface CodexEffectiveOperations {
  policyRevision: number;
  operations: Partial<Record<CodexOperation, CodexOperationStatus>>;
  /** T03：能力身份摘要（无路径；完整目标指纹+包 SHA+规则版本）。旧载荷缺省 →
   *  undefined。仅用于区分展示目标（短断链保留上次支持展示的前提），不参与授权。 */
  capabilityIdentity?: string;
}

/** T03：交互生命周期（serde snake_case）。 */
export type CodexInteractionState =
  | "pending"
  | "temporarily_blocked"
  | "submitting"
  | "unconfirmed"
  | "answered"
  | "rejected"
  | "cancelled"
  | "ended"
  | "unknown";

/** T03：异步题视图（逐题独立生命周期）。 */
export interface CodexAsyncQuestionView {
  questionKey: string;
  contextToken: string;
  sourceTurnId: string;
  sourceItemId: string;
  questionIndex: number;
  title: string;
  options: string[];
  state: CodexInteractionState;
  actionable: boolean;
  disabledReasonCode: string | null;
}

/** T03：正式答复的安全呈现（"问题／答案"）。`correlated=false` 不提升动作送达状态。 */
export interface CodexInteractionAnswerSummary {
  questionKey: string;
  question: string;
  answer: string;
  correlated: boolean;
}

/** T04：单题作答策略（后端按已核验契约给出；前端不自行推断多值）。 */
export interface CodexAnswerPolicy {
  allowMultiple: boolean;
  maxAnswers: number;
  allowFreeText: boolean;
}

/** 结构化问题（Plan blocking input）。is_secret 的回答不入审计/日志。 */
export interface QuestionOption {
  label: string;
  description: string;
}

export interface QuestionView {
  id: string;
  header: string;
  question: string;
  /** true = 允许自由文本作答；false = 只能选项。 */
  isOther: boolean;
  isSecret: boolean;
  options: QuestionOption[];
  /** T04：作答策略；旧后端缺省 → undefined（前端按保守单值处理）。 */
  answerPolicy?: CodexAnswerPolicy;
}

/** 待处理请求视图。contextToken 是服务端随机 UUID（绑定 boot/epoch/generation/
 *  thread/request）；requestKey/token 任一变化 = 旧上下文作废，调用方必须清旧状态。 */
export interface PendingView {
  requestKey: string;
  contextToken: string;
  kind: PendingKind;
  actionable: boolean;
  disabledReasonCode: string | null;
  /** 服务端策略给出的可用决策词（accept/decline/cancel）；空 = 只能到桌面处理。 */
  allowedDecisions?: string[];
  command: string | null;
  cwd: string | null;
  reason: string | null;
  /** 文件变更审批的安全摘要（变更条数）；命令审批为 null。 */
  fileSummary?: string | null;
  permissionSummary: string | null;
  questions: QuestionView[];
}

/** 内容健康状态（2026-10-18 形态适配）：与写能力（availability）正交——
 *  能发送不代表历史已载入；读取出错（unsupported）不得呈现为真空会话。
 *  ready 且真实 0 条才显示「暂无消息」。 */
export type ContentState = "loading" | "ready" | "partial" | "unsupported";

/**
 * T02：投影的**增量版本束**（与 Rust `types.rs::ProjectionVersion` 逐字段对齐；
 * SSE snapshot/state、GET 读 DTO、轮询同源下发）。`codex-projection-order.ts`
 * 据此把 GET／SSE／手动刷新收进同一落地守卫（迟到的旧响应不得覆盖新状态）。
 * `instanceId` 绑定服务端 hub 世系（服务重启即换新），不是每次请求随机值。
 * **旧后端不下发 → undefined**（按旧载荷退化规则，不获得新写资格）。
 */
export interface ProjectionVersion {
  instanceId: string;
  ownerEpoch: number;
  ownerGeneration: number;
  revision: number | null;
  localSeq: number;
}

/** Thread 状态投影（SSE / REST 同一 Tuvis 投影，IPC 原始 state 不出远程 API）。 */
export interface ThreadProjection {
  threadId: string;
  revision: number | null;
  stale: boolean;
  availability: ControlAvailability;
  capabilities: CapabilitySet;
  runtimeStatus: string;
  contentState: ContentState;
  messages: ProjectedMessage[];
  truncated: boolean;
  /** 历史代际 token（同代分页锚点；旧载荷缺省 → 空串，**不得据此分页**）。 */
  historyToken?: string;
  pending: PendingView[];
  /** #172/T9a：探测只读事实。**老后端不下发** → undefined（界面按「无探测信息」
   *  走旧路径，绝不因缺字段崩）。 */
  probeStatus?: ProbeStatus;
  /** 复审 R-T4：该 Thread 的队列暂停（打断后旧队列冻结；唯一解除出口 =
   *  /codex/queue/resume）。serde default 向后兼容——旧后端缺省 → undefined（按
   *  非暂停处理）。**本字段只作刷新触发面**（帧签名）：暂停变化的 republish 帧
   *  只翻它（revision 不动），签名据此驱动 composer 重拉 send-info——发送门的
   *  权威值始终以 send-info 的 queuePaused 为准（消费见 MessageComposer）。 */
  queuePaused?: boolean;
  /** T02：逐操作有效集合。**旧后端缺字段 → undefined，不得从 availability 推全部
   *  能力**（缺字段 = 未知，不是"全部可用"）。 */
  effectiveOperations?: CodexEffectiveOperations;
  /** T03：异步题视图。旧后端缺字段 → undefined（不据此清卡）。 */
  asyncQuestions?: CodexAsyncQuestionView[];
  /** T02：增量版本束（投影落地守卫的排序事实）。旧后端缺字段/脏形状 → undefined
   *  （退化为既有 revision 规则；缺版本不获得新写资格）。 */
  projectionVersion?: ProjectionVersion;
  /** T03：新鲜镜像同次观察的活动回合 id（「停止本轮」按钮的目标；**不替代后端
   *  writer 校验**）。idle/unknown/stale/旧载荷 → undefined/null（不猜）。 */
  activeTurnId?: string | null;
}

/** 同代只读消息页状态（与 Rust `MessagesPageStatus` 逐字对齐；非正常状态不得
 *  当作完整空页，也不得被解释为「没有更多」）。 */
export type MessagesPageStatus = "ready" | "end" | "loading" | "unavailable" | "cursor_invalid";

/** 同代只读历史页（`GET /m/api/v1/codex/thread/messages`，Task 4 端点）。
 *  `messages` 是**严格在游标之前**的一页（canonical 旧→新）；`historyToken` 是
 *  下一页游标对应的 token，客户端原样回传；`reasonCode` 为有界机器码。 */
export interface MessagesPage {
  messages: ProjectedMessage[];
  historyToken: string;
  revision: number | null;
  nextBefore: string | null;
  status: MessagesPageStatus;
  reasonCode: string | null;
}

// ==== 端点路径（唯一来源：实现与测试 stub 都引用它）====

export const CODEX_THREAD_PATH = "/m/api/v1/codex/thread";
export const CODEX_THREAD_EVENTS_PATH = "/m/api/v1/codex/thread/events";
/** 同代只读历史页（Task 4 端点；长路径，测试/实现判序需在 CODEX_THREAD_PATH 之前）。 */
export const CODEX_THREAD_MESSAGES_PATH = "/m/api/v1/codex/thread/messages";
export const CODEX_PENDING_RESPONSE_PATH = "/m/api/v1/codex/pending-response";
export const CODEX_ACTION_PATH = "/m/api/v1/codex/action";
/** S4 只读核对（长路径，测试/实现判序需在 CODEX_ACTION_PATH 之前）。 */
export const CODEX_ACTION_VERIFY_PATH = "/m/api/v1/codex/action/verify";
/** S4 人工继续（唯一后继 + 撤销写出权 + 解除屏障，同一事务）。 */
export const CODEX_ACTION_MANUAL_RESUME_PATH = "/m/api/v1/codex/action/manual-resume";
export const CODEX_QUEUE_PATH = "/m/api/v1/codex/queue";
export const CODEX_QUEUE_RETRACT_PATH = "/m/api/v1/codex/queue/retract";
/** S2：打断后持久暂停的唯一解除出口。 */
export const CODEX_QUEUE_RESUME_PATH = "/m/api/v1/codex/queue/resume";
export const CODEX_QUEUE_FORCE_CONTINUE_PATH = "/m/api/v1/codex/queue/force-continue";
/** T06：已入队消息的显式优先处理（越队派发既有 queued 行；不接受正文副本）。 */
export const CODEX_QUEUE_PRIORITIZE_PATH = "/m/api/v1/codex/queue/prioritize";
export const CODEX_TURN_INTERRUPT_PATH = "/m/api/v1/codex/turn/interrupt";
/** F03：异步题正式答题（独立动作；不进普通消息 FIFO）。 */
export const CODEX_ASYNC_ANSWER_PATH = "/m/api/v1/codex/async-answer";
/** F08：适配详情（只读）与开关（与桌面设置同源）。 */
export const CODEX_ADAPT_STATUS_PATH = "/m/api/v1/codex/adapt-status";
export const CODEX_ADAPT_SETTINGS_PATH = "/m/api/v1/codex/adapt-settings";

// ==== fetch 基建 ====

/** GET 的网络层异常包装（对齐 api.ts 惯例：status=null = 无响应可读） */
function networkFail(path: string, e: unknown): ApiError {
  return new ApiError(null, `MOBILE_NETWORK_FAIL:${path}: ${path} 网络异常: ${String(e)}`);
}

/** 非 2xx：错误体 JSON 解析进 data（403 设备失效/409 过期上下文的分診依据） */
async function toApiError(path: string, r: Response): Promise<ApiError> {
  let data: Record<string, unknown> | null = null;
  try {
    data = (await r.json()) as Record<string, unknown>;
  } catch {
    /* 非 JSON 错误体（代理注入页等）：data 保持 null，按状态码兜底 */
  }
  return new ApiError(r.status, `${path} ${r.status}`, data);
}

// ==== Thread 投影 ====

/** 投影载荷的最小形状守卫（纵深防御）：缺 availability/messages/pending 的残缺
 *  载荷（旧后端/代理注入页）按错误处理——调用方保留 null 态走旧路径，绝不拿残缺
 *  DTO 渲染出「四档全空」的假界面。contentState 同校验（2026-10-19 复审补入：
 *  服务端恒序列化该字段，缺省/枚举外取值 = 残缺载荷——「暂无消息」的判定依赖它，
 *  拿不到真值宁可报错也不把 loading/unsupported 冒充成 ready 空会话）。 */
const CONTENT_STATES = new Set(["loading", "ready", "partial", "unsupported"]);

/** 消息兼容归一化（2026-10-20 Task 3）：`turnId`/`artifacts`/`turnImageCount`/
 *  `omittedArtifactCount` 是兼容扩展，旧载荷缺省时补 null/[]/0；其他字段原样保留。
 *  缺省 ≠ 服务端省略：`omittedArtifactCount` 只由后端预算给出，旧载荷不会被伪造成
 *  「有附件被省略」。 */
function normalizeProjectedMessage(message: ProjectedMessage): ProjectedMessage {
  return {
    ...message,
    turnId: message.turnId ?? null,
    artifacts: Array.isArray(message.artifacts) ? message.artifacts : [],
    turnImageCount:
      typeof message.turnImageCount === "number" && message.turnImageCount >= 0
        ? message.turnImageCount
        : 0,
    omittedArtifactCount:
      typeof message.omittedArtifactCount === "number" && message.omittedArtifactCount >= 0
        ? message.omittedArtifactCount
        : 0,
    // T03：正式答复摘要（缺省保持 undefined——不改变旧载荷的字段形状；
    // 有值时归一化，脏值丢弃不半渲染）。
    ...(message.interactionAnswer === undefined
      ? {}
      : { interactionAnswer: asInteractionAnswer(message.interactionAnswer) }),
  };
}

const OPERATION_KEYS = new Set<string>([
  "send",
  "command_approval",
  "file_change_approval",
  "blocking_input",
  "async_answer_idle",
  "interrupt",
  "request_cancel",
  "permissions_approval",
  "mode_read",
  "mode_set",
  "permissions_read",
  "permissions_set",
]);

const EVIDENCE_SOURCES = new Set(["none", "shipped", "contract_matched", "observed_success"]);

const INTERACTION_STATES = new Set([
  "pending",
  "temporarily_blocked",
  "submitting",
  "unconfirmed",
  "answered",
  "rejected",
  "cancelled",
  "ended",
  "unknown",
]);

/** T02：逐操作集合归一化。**缺字段/形状脏 → undefined**（未知），绝不从
 *  availability 或 capabilities 推导"全部可用"。未知操作键被忽略（前向兼容）。 */
function asEffectiveOperations(raw: unknown): CodexEffectiveOperations | undefined {
  if (raw === null || typeof raw !== "object") return undefined;
  const value = raw as {
    policyRevision?: unknown;
    operations?: unknown;
    capabilityIdentity?: unknown;
  };
  if (typeof value.policyRevision !== "number") return undefined;
  if (value.operations === null || typeof value.operations !== "object") return undefined;
  const operations: Partial<Record<CodexOperation, CodexOperationStatus>> = {};
  for (const [key, status] of Object.entries(value.operations as Record<string, unknown>)) {
    if (!OPERATION_KEYS.has(key)) continue; // 未知操作不猜
    if (status === null || typeof status !== "object") continue;
    const item = status as Record<string, unknown>;
    if (typeof item.implemented !== "boolean" || typeof item.allowed !== "boolean") continue;
    const evidence =
      typeof item.evidence === "string" && EVIDENCE_SOURCES.has(item.evidence)
        ? (item.evidence as CodexEvidenceSource)
        : "none";
    // T03：support 归一——枚举内词原样透出；缺省/脏词按 evidence 推导**展示值**
    // （仅展示，不参与 allowed/资格，也不产生新写资格）。allowed/reasonCode 原样透传。
    const rawSupport =
      typeof item.support === "string" && SUPPORT_STATES.has(item.support as CodexSupportState)
        ? (item.support as CodexSupportState)
        : undefined;
    const support =
      rawSupport ??
      (!item.implemented ? "not_implemented" : evidence !== "none" ? "supported" : "identifying");
    operations[key as CodexOperation] = {
      implemented: item.implemented,
      allowed: item.allowed,
      requiresAutoAdapt: item.requiresAutoAdapt === true,
      evidence,
      support,
      // R07：沿用标记归一——严格 true 才算（旧载荷/脏值 → undefined）。
      ...(item.supportInherited === true ? { supportInherited: true } : {}),
      // C03：已核对标记归一——严格 true 才算（旧载荷/脏值 → undefined = 待核对）。
      ...(item.supportIdentityVerified === true ? { supportIdentityVerified: true } : {}),
      reasonCode: typeof item.reasonCode === "string" ? item.reasonCode : null,
    };
  }
  // T03：能力身份摘要（无路径）——非字符串（旧载荷缺省/脏值）→ undefined。
  const capabilityIdentity =
    typeof value.capabilityIdentity === "string" ? value.capabilityIdentity : undefined;
  return { policyRevision: value.policyRevision, operations, capabilityIdentity };
}

/** T03：异步题归一化。缺字段 → undefined（不据此清卡）；脏条目丢弃不猜。 */
function asAsyncQuestions(raw: unknown): CodexAsyncQuestionView[] | undefined {
  if (!Array.isArray(raw)) return undefined;
  const questions: CodexAsyncQuestionView[] = [];
  for (const entry of raw) {
    if (entry === null || typeof entry !== "object") continue;
    const q = entry as Record<string, unknown>;
    if (typeof q.questionKey !== "string" || typeof q.contextToken !== "string") continue;
    if (typeof q.state !== "string" || !INTERACTION_STATES.has(q.state)) continue;
    questions.push({
      questionKey: q.questionKey,
      contextToken: q.contextToken,
      sourceTurnId: typeof q.sourceTurnId === "string" ? q.sourceTurnId : "",
      sourceItemId: typeof q.sourceItemId === "string" ? q.sourceItemId : "",
      questionIndex: typeof q.questionIndex === "number" ? q.questionIndex : 0,
      title: typeof q.title === "string" ? q.title : "",
      options: Array.isArray(q.options)
        ? q.options.filter((option): option is string => typeof option === "string")
        : [],
      state: q.state as CodexInteractionState,
      actionable: q.actionable === true,
      disabledReasonCode: typeof q.disabledReasonCode === "string" ? q.disabledReasonCode : null,
    });
  }
  return questions;
}

/** T03：正式答复摘要归一化（缺省 → null；字段不全会丢弃，不半渲染）。 */
function asInteractionAnswer(raw: unknown): CodexInteractionAnswerSummary | null {
  if (raw === null || typeof raw !== "object") return null;
  const value = raw as Record<string, unknown>;
  if (
    typeof value.questionKey !== "string" ||
    typeof value.question !== "string" ||
    typeof value.answer !== "string"
  ) {
    return null;
  }
  return {
    questionKey: value.questionKey,
    question: value.question,
    answer: value.answer,
    correlated: value.correlated === true,
  };
}

/**
 * T02：增量版本束归一化（附加只读字段，与 probeStatus 同纪律）。缺字段或任一
 * 子字段缺失/错型 → **undefined**（不透传半截版本束——排序事实必须完整才有意义，
 * 脏束若被当真会让落地守卫做出错误的先后判定）。
 */
function asProjectionVersion(raw: unknown): ProjectionVersion | undefined {
  if (raw === null || typeof raw !== "object") return undefined;
  const v = raw as Record<string, unknown>;
  if (
    typeof v.instanceId !== "string" ||
    typeof v.ownerEpoch !== "number" ||
    typeof v.ownerGeneration !== "number" ||
    typeof v.localSeq !== "number" ||
    (v.revision !== null && typeof v.revision !== "number")
  ) {
    return undefined;
  }
  return {
    instanceId: v.instanceId,
    ownerEpoch: v.ownerEpoch,
    ownerGeneration: v.ownerGeneration,
    revision: v.revision,
    localSeq: v.localSeq,
  };
}

function asThreadProjection(v: unknown): ThreadProjection {
  const p = v as ThreadProjection | null;
  if (
    p === null ||
    typeof p !== "object" ||
    typeof p.availability !== "string" ||
    typeof p.contentState !== "string" ||
    !CONTENT_STATES.has(p.contentState) ||
    !Array.isArray(p.messages) ||
    !Array.isArray(p.pending)
  ) {
    throw new ApiError(200, "codex/thread: malformed projection payload");
  }
  return {
    ...p,
    // 历史代际 token 是兼容扩展：旧载荷缺省补空串（服务端新载荷恒序列化）。
    historyToken: typeof p.historyToken === "string" ? p.historyToken : "",
    messages: p.messages.map(normalizeProjectedMessage),
    // 探测事实（#172/T9a）：老后端不下发 → undefined；下发但形状脏 → 归一。
    // 归一放在守卫之后：投影的**主干**字段缺失仍然按残缺载荷报错（不拿半截 DTO
    // 渲染界面），而探测事实是**附加**只读字段，脏值只降级不致命。
    probeStatus: asProbeStatus((p as { probeStatus?: unknown }).probeStatus),
    // 复审 R-T4：队列暂停事实（附加只读字段，与 probeStatus 同纪律）——旧载荷
    // 缺省 / 脏值 → undefined（按非暂停处理，不猜）。
    queuePaused:
      typeof (p as { queuePaused?: unknown }).queuePaused === "boolean"
        ? (p as { queuePaused: boolean }).queuePaused
        : undefined,
    // T02：逐操作有效集合（附加只读字段）。缺字段/脏载荷 → **不添加该键**
    // （undefined 语义 = 未知）；**不得**从 availability 推导"全部可用"。
    ...(() => {
      const operations = asEffectiveOperations(
        (p as { effectiveOperations?: unknown }).effectiveOperations
      );
      return operations === undefined ? {} : { effectiveOperations: operations };
    })(),
    // T03：异步题（附加只读字段）。缺字段 → 不添加该键（不据此清卡）。
    ...(() => {
      const questions = asAsyncQuestions((p as { asyncQuestions?: unknown }).asyncQuestions);
      return questions === undefined ? {} : { asyncQuestions: questions };
    })(),
    // T02：增量版本束（附加只读字段）。缺字段/脏形状 → undefined（旧载荷，
    // 落地守卫退化为既有 revision 规则，不获得新写资格）。恒覆盖 `...p` 里
    // 的原键：脏束不得原样透传。
    projectionVersion: asProjectionVersion(
      (p as { projectionVersion?: unknown }).projectionVersion
    ),
    // T03：活动回合 id（附加只读字段）——字符串才透出；缺省/脏值 → undefined
    //（无目标，不猜；「停止本轮」按钮消费，写边界校验在后端）。
    activeTurnId: (() => {
      const v = (p as { activeTurnId?: unknown }).activeTurnId;
      return typeof v === "string" && v.length > 0 ? v : undefined;
    })(),
  };
}

/** 拉取 Thread 投影（useCodexThread 的轮询/refresh 通道）。
 *  403（设备失效）→ null，与 fetchSendInfo 同语义——由上层全局判废通道处理；
 *  其余非 2xx / 网络异常 / 残缺载荷 → 抛 ApiError。 */
export async function fetchCodexThread(
  sessionId: string,
  signal?: AbortSignal
): Promise<ThreadProjection | null> {
  const q = new URLSearchParams({ session_id: sessionId });
  let r: Response;
  try {
    r = await fetch(`${CODEX_THREAD_PATH}?${q}`, { signal });
  } catch (e) {
    throw networkFail("codex/thread", e);
  }
  if (r.status === 403) return null; // 设备失效 → 回配对页（全局判废）
  if (!r.ok) throw await toApiError("codex/thread", r);
  return asThreadProjection(await r.json());
}

// ==== 同代只读历史页（Task 5 客户端；端点见 Task 4）====

const MESSAGES_PAGE_STATUSES = new Set([
  "ready",
  "end",
  "loading",
  "unavailable",
  "cursor_invalid",
]);

/**
 * 消息页的共享归一化（与投影同一消息归一化 `normalizeProjectedMessage`）。
 *
 * 守卫：`status` 必填且必须属于五个状态值、`messages` 必须是数组；其余字段兼容
 * 缺省（`historyToken` → 空串、`nextBefore`/`reasonCode` → null、`revision` → null）。
 * `status !== "ready"` 的页按契约恒为空消息——即使载荷带了半页也丢弃，绝不把
 * 非正常状态伪装成完整空页。
 */
function asMessagesPage(v: unknown): MessagesPage {
  const p = v as MessagesPage | null;
  if (
    p === null ||
    typeof p !== "object" ||
    typeof p.status !== "string" ||
    !MESSAGES_PAGE_STATUSES.has(p.status) ||
    !Array.isArray(p.messages)
  ) {
    throw new ApiError(200, "codex/thread/messages: malformed messages page payload");
  }
  const ready = p.status === "ready";
  return {
    messages: ready ? p.messages.map(normalizeProjectedMessage) : [],
    historyToken: typeof p.historyToken === "string" ? p.historyToken : "",
    revision: typeof p.revision === "number" ? p.revision : null,
    nextBefore: typeof p.nextBefore === "string" && p.nextBefore !== "" ? p.nextBefore : null,
    status: p.status,
    reasonCode: typeof p.reasonCode === "string" && p.reasonCode !== "" ? p.reasonCode : null,
  };
}

/** `fetchCodexMessagesPage` 参数（Task 4 端点契约）。 */
export interface CodexMessagesPageQuery {
  sessionId: string;
  /** 历史代际 token：首次取当前投影 `historyToken`，后续取上一页 `historyToken`。 */
  historyToken: string;
  /** 游标（严格更早）；缺省/空串 = 历史末端锚点。 */
  beforeId?: string | null;
  /** 页大小（1..=50）；缺省由服务端按 50 处理。 */
  limit?: number;
  signal?: AbortSignal;
}

/**
 * 读取一页更早的 canonical 消息（只读；不触发发送/审批/写入）。
 *
 * 400（缺 token / 非法 limit）、404（会话不存在）等非 2xx 抛 `ApiError`；
 * 网络异常包装为 `ApiError(status=null)`；残缺载荷抛 `ApiError(200, malformed...)`。
 * 注意：403 不在此函数内吞成 null——由 hook 统一按不可用处理，避免分页层静默
 * 改变设备失效语义（与 `fetchCodexThread` 的 null 约定分列）。
 */
export async function fetchCodexMessagesPage(query: CodexMessagesPageQuery): Promise<MessagesPage> {
  const params = new URLSearchParams({
    session_id: query.sessionId,
    history_token: query.historyToken,
  });
  if (query.beforeId) params.set("before_id", query.beforeId);
  if (query.limit !== undefined) params.set("limit", String(query.limit));
  let r: Response;
  try {
    r = await fetch(`${CODEX_THREAD_MESSAGES_PATH}?${params}`, { signal: query.signal });
  } catch (e) {
    throw networkFail("codex/thread/messages", e);
  }
  if (!r.ok) throw await toApiError("codex/thread/messages", r);
  return asMessagesPage(await r.json());
}

// ==== SSE 订阅 ====

/** 断流后重连的退避基数（毫秒）：第 N 次失败等待 N×基准（api.ts connectEvents 同节奏） */
export const CODEX_SSE_RECONNECT_BASE_MS = 1000;

/** 降级阈值：连续失败达到该次数即放弃 SSE（计划 Task 9：连续 3 次失败降为 3s GET）。
 *  刻意与看板通道的 DEGRADE_AFTER_FAILURES=2 分列：原生投影断流 ≠ 看板断流，
 *  阈值各自演进互不牵连。 */
export const CODEX_SSE_DEGRADE_AFTER = 3;

/**
 * 订阅 Thread 投影事件流（GET /codex/thread/events），返回停止函数。
 *
 * 事件名：首帧 `snapshot`（连接建立/断线重连后的基线校正），后续 `state`（增量
 * 投影）——两帧的 data 均为 ThreadProjection，同 DTO 转交 onState。
 *
 * 断线策略与 api.ts connectEvents 同构（可数失败语义）：onerror 显式 close() +
 * 手动线性退避重连；连续 CODEX_SSE_DEGRADE_AFTER 次失败 → onUnavailable() 一次，
 * 由调用方接管轮询。坏帧（截断/代理注入/合法 JSON 但缺字段的残缺载荷）跳过该帧
 * 不断流——与 GET 通道同一形状守卫（asThreadProjection），残缺 DTO 绝不直入
 * onState 渲染出「四档全空」的假界面。
 */
export function subscribeCodexThread(
  sessionId: string,
  onState: (s: ThreadProjection, event: "snapshot" | "state") => void,
  onUnavailable: () => void
): () => void {
  const path = `${CODEX_THREAD_EVENTS_PATH}?${new URLSearchParams({ session_id: sessionId })}`;
  let es: EventSource | null = null;
  let failures = 0;
  let reconnectTimer: ReturnType<typeof setTimeout> | null = null;
  let degraded = false;
  let stopped = false; // 停止后禁止任何重连再被装上

  function degrade() {
    if (degraded) return;
    degraded = true;
    es?.close();
    es = null;
    onUnavailable();
  }

  /** 解析帧并转交回调；坏帧跳过（不断流），成功帧清零失败计数。
   *  形状守卫与 GET 同一（asThreadProjection）：合法 JSON 但残缺的帧（缺
   *  contentState / messages / pending 或枚举外取值）同样按坏帧跳过——
   *  能收字节但解析不出完整投影不足以证明链路健康（不清零失败计数）。
   *  T02：第二参数带出事件名（snapshot = 当前代际最新握手，消费方据此授予
   *  instance 切换资格）。事件名由各自的 addEventListener 闭包显式传入，
   *  不读 Event.type（测试替身/代理层不保证保留）。 */
  function dispatch(e: Event, event: "snapshot" | "state") {
    try {
      const parsed = asThreadProjection(JSON.parse((e as MessageEvent).data));
      failures = 0;
      onState(parsed, event);
    } catch {
      /* 坏帧：忽略（能收字节但解析不了不足以证明链路健康，不清零） */
    }
  }

  function connect() {
    if (stopped || degraded) return;
    let next: EventSource;
    try {
      next = new EventSource(path);
    } catch {
      // 环境不支持 EventSource：不做注定失败的退避重试，直接降级轮询
      degrade();
      return;
    }
    es = next;
    next.addEventListener("snapshot", (e) => dispatch(e, "snapshot"));
    next.addEventListener("state", (e) => dispatch(e, "state"));
    next.onerror = () => {
      if (stopped || degraded) return;
      next.close(); // 关掉内建重连（自管节奏才有可数失败语义）
      if (es === next) es = null;
      failures += 1;
      if (failures >= CODEX_SSE_DEGRADE_AFTER) {
        degrade();
      } else {
        reconnectTimer = setTimeout(connect, CODEX_SSE_RECONNECT_BASE_MS * failures);
      }
    };
  }

  connect();
  return () => {
    stopped = true;
    es?.close();
    es = null;
    if (reconnectTimer) clearTimeout(reconnectTimer);
  };
}

// ==== pending 应答 ====

/** 待处理请求的动作词（与后端 `PendingAction` 的 HTTP 形态对应；answer 携带 answers） */
export type CodexPendingActionWord = "approve" | "reject" | "cancel" | "answer";

/** POST /codex/pending-response 请求体（**白名单契约**：只有这些键——后端
 *  deny_unknown_fields，owner/权限对象/raw method 一律 400）。 */
export interface PendingResponseBody {
  sessionId: string;
  /** H11：前台剩余预算（毫秒）；客户端从点击起算并如实透传。 */
  frontBudgetMs?: number;
  /** Tuvis 动作 UUID（一次用户动作生成一次，双击沿同 ID） */
  actionId: string;
  /** 无损 requestKey（`n:<十进制>` / `s:<原串>`），永远字符串原样回显 */
  requestKey: string;
  /** 服务端下发的上下文令牌（原样回显） */
  contextToken: string;
  action: CodexPendingActionWord;
  /** action="answer" 时按原问题 ID 映射的标签数组（is_secret 答案走此通道，
   *  服务端只入 hash 不入审计原文） */
  answers?: Record<string, string[]>;
}

/**
 * 应答原生 pending（批准/拒绝/结构化回答）。
 *
 * 回执契约：HTTP 200 = 明确回执（**含 unknown_delivery**——那是结果未知，不是
 * 网络错误，调用方不得自动重试）；409 = 过期上下文（stale_context，token/owner
 * 已换代，等新投影）；403/400/404 照 ApiError 惯例抛出。
 */
/** H10/H11：**可靠的投递前拒绝**机器码（登记前零投递；可保稿、结束本次动作）。
 *
 *  只认我方错误体里的机器码——代理 5xx / 网络异常 / 未知码一律按"结果未知"
 *  处理（沿原 ID 只读核对，绝不自动重发）。 */
export const CODEX_PRE_DELIVERY_ERROR_CODES: ReadonlySet<string> = new Set([
  "bad_request",
  "answers_mismatch",
  "answer_not_option",
  "invalid_id",
  "forbidden",
  "no_session",
  "no_action",
  "capability_unavailable",
  "empty_answers",
  "too_many_questions",
  "too_many_answers",
  "answer_too_long",
  "answers_too_large",
  "action_mismatch",
  "stale_context",
  "action_conflict",
  "question_occupied",
  "question_already_answered",
]);

export function isCodexPreDeliveryRefusal(e: unknown): boolean {
  if (!(e instanceof ApiError) || e.status === null) return false;
  const code = typeof e.data?.error === "string" ? e.data.error : null;
  return code !== null && CODEX_PRE_DELIVERY_ERROR_CODES.has(code);
}

export async function respondCodexPending(body: PendingResponseBody): Promise<ActionReceipt> {
  let r: Response;
  try {
    r = await fetch(CODEX_PENDING_RESPONSE_PATH, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify(body),
    });
  } catch (e) {
    throw networkFail("codex/pending-response", e);
  }
  if (!r.ok) throw await toApiError("codex/pending-response", r);
  return (await r.json()) as ActionReceipt;
}

// ==== 动作查询 / 队列 ====

/** 查询动作状态（只读核对；404 = 其他设备的动作/不存在 → ApiError）。 */
export async function fetchCodexAction(actionId: string): Promise<CodexActionView> {
  let r: Response;
  try {
    r = await fetch(`${CODEX_ACTION_PATH}?${new URLSearchParams({ action_id: actionId })}`);
  } catch (e) {
    throw networkFail("codex/action", e);
  }
  if (!r.ok) throw await toApiError("codex/action", r);
  return (await r.json()) as CodexActionView;
}

/** 拉取本设备在该 Thread 的队列视图（FIFO 位置/暂停原因）。
 *  403（设备失效）→ null（fetchCodexThread 同口径）；其余非 2xx → ApiError。 */
export async function fetchCodexQueue(sessionId: string): Promise<CodexQueueView | null> {
  const q = new URLSearchParams({ session_id: sessionId });
  let r: Response;
  try {
    r = await fetch(`${CODEX_QUEUE_PATH}?${q}`);
  } catch (e) {
    throw networkFail("codex/queue", e);
  }
  if (r.status === 403) return null;
  if (!r.ok) throw await toApiError("codex/queue", r);
  return (await r.json()) as CodexQueueView;
}

/** 撤回本设备的 queued 动作。200 → cancelled 的 CodexActionView；
 *  409 = 已派发不可撤（ApiError，调用方保留条目并刷新）；404 = 不存在/他人动作。 */
export async function retractCodexAction(actionId: string): Promise<CodexActionView> {
  let r: Response;
  try {
    r = await fetch(CODEX_QUEUE_RETRACT_PATH, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ actionId }),
    });
  } catch (e) {
    throw networkFail("codex/queue/retract", e);
  }
  if (!r.ok) throw await toApiError("codex/queue/retract", r);
  return (await r.json()) as CodexActionView;
}

/** 人工解除「无出口保守屏障」（2026-10-19 复审 Important：native 投递结果未知的
 *  队列屏障没有自动恢复路径）——用户已在桌面端核对该消息实际去向后的**显式覆盖**，
 *  调用方必须先经二次确认（重复内容风险）。
 *  200 → 解除后的 CodexActionView（状态不改写，只清屏障）；409 = queued（应走撤回）
 *  或屏障不在册；404 = 不存在/他人动作。重复调用幂等（返回当前状态，不重复解除）。 */
export async function forceContinueCodexAction(actionId: string): Promise<CodexActionView> {
  let r: Response;
  try {
    r = await fetch(CODEX_QUEUE_FORCE_CONTINUE_PATH, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ actionId }),
    });
  } catch (e) {
    throw networkFail("codex/queue/force-continue", e);
  }
  if (!r.ok) throw await toApiError("codex/queue/force-continue", r);
  return (await r.json()) as CodexActionView;
}

// ==== 打断桌面回合 ====

/** 打断桌面当前回合的结果（2026-10-22 复审 P2-3：投递结果三分支如实回报）。
 *  - `interruptedTurnId` 非空 = 确实打断了那个回合；
 *  - `interruptedTurnId` 为 null 且 `uncertain` false = 桌面侧没有可打断的活动回合
 *   （**不谎报成功**，也不当作错误——回合可能刚好自己结束了）；
 *  - `uncertain` true = 停止指令已发出但结果未知（RPC 超时等）——打断**可能已生效**，
 *    调用方必须如实呈现「不确定」，不得当作失败诱导立刻重按（重按 = 双发破坏性
 *    指令）。此时 `reasonCode` 携带机器码（如 timeout）。 */
export interface InterruptTurnResult {
  ok: boolean;
  uncertain: boolean;
  interruptedTurnId: string | null;
  expectedTurnId: string | null;
  /** uncertain=true 时的机器码（timeout / caller_dropped 等）；其余为 null。 */
  reasonCode?: string | null;
}

/** 打断桌面 Codex 当前回合（2026-10-21 新增）。
 *
 * **破坏性动作**：被打断回合未完成的工作会丢失——调用方必须先经两步确认并向用户
 * 披露该风险，不得静默触发。
 *
 * `turnId` 缺省时由服务端取**在册原生屏障绑定的 turnId** 作 `expectedTurnId` 安全阀
 * （Desktop 侧「与当前活动回合不符 → 静默不动作」）；两路都取不到阀值 → 409
 * `no_expected_turn` 拒绝（零 IPC 写入，复审 P1-1——服务端绝不无阀照发）。
 * 200 = 打断请求已投递（`uncertain` 见上）；409 = 上下文过期（stale_context）或
 * 桌面明确拒绝（desktop_rejected）；503 = 该 Desktop 版本未验证打断能力 / 连接不可用。
 * 错误体的 `reason` 为中文可读文案，展示侧优先取用（不裸示机器码）。 */
export async function interruptCodexTurn(
  sessionId: string,
  turnId?: string
): Promise<InterruptTurnResult> {
  let r: Response;
  try {
    r = await fetch(CODEX_TURN_INTERRUPT_PATH, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify(turnId ? { sessionId, turnId } : { sessionId }),
    });
  } catch (e) {
    throw networkFail("codex/turn/interrupt", e);
  }
  if (!r.ok) throw await toApiError("codex/turn/interrupt", r);
  const raw = (await r.json()) as Partial<InterruptTurnResult>;
  return {
    ok: raw.ok === true,
    uncertain: raw.uncertain === true,
    interruptedTurnId: raw.interruptedTurnId ?? null,
    expectedTurnId: raw.expectedTurnId ?? null,
    reasonCode: raw.reasonCode ?? null,
  };
}

// ==== S4：只读核对 / 人工继续 / S2 队列恢复 ====

/** S4 只读核对结论（`CodexVerifyOutcome` 四值；「没找到」≠「没发送」）。 */
export type CodexVerifyOutcome = "not_delivered" | "delivered" | "accepted_unbound" | "unresolved";

/** S4 只读核对结果（`CodexActionVerifyView`；serde camelCase）。 */
export interface CodexActionVerifyView {
  actionId: string;
  stage: CodexDeliveryStage;
  outcome: CodexVerifyOutcome;
  turnId: string | null;
  runLimitReleased: boolean;
  reasonCode: string | null;
  verifiedAt: number;
  /** G09：组答题已由正式答复确认的成员（非组动作 = 空数组）。 */
  answeredQuestionKeys?: string[];
}

/**
 * 只读核对一次发送的实际去向（S4：≤10 秒有界；零发送 mutation——核对绝不重发）。
 * 404 = 不存在/他人动作 → ApiError；unresolved 是 200 的正常结论（继续待确认）。
 */
export async function verifyCodexAction(
  actionId: string,
  signal?: AbortSignal
): Promise<CodexActionVerifyView> {
  let r: Response;
  try {
    r = await fetch(`${CODEX_ACTION_VERIFY_PATH}?${new URLSearchParams({ action_id: actionId })}`, {
      signal,
    });
  } catch (e) {
    throw networkFail("codex/action/verify", e);
  }
  if (!r.ok) throw await toApiError("codex/action/verify", r);
  return (await r.json()) as CodexActionVerifyView;
}

/** resumed 响应的 `send`（后继随即走正常发送路径的真实结果）：receipt = 真实回执
 *  （Accepted / NotSent / UnknownDelivery…）；queued = 忙态留队带位次。
 *  W2：前端必须按此呈现后继的真实五分类，不得凭「后继已创建」宣称已发。 */
export interface CodexSuccessorSend {
  receipt?: ActionReceipt;
  queued?: boolean;
  position?: number | null;
}

/** S4 人工继续的 200 结论（幂等：重复/并发最多一个后继）。 */
export type CodexManualResumeResult =
  | {
      status: "resumed";
      predecessor: CodexActionView;
      successorActionId: string;
      /** 后继随即走正常发送路径： receipt = 真实回执；queued = 忙态留队带位次。 */
      send: CodexSuccessorSend;
    }
  | { status: "converged"; action: CodexActionView }
  | { status: "already_resumed"; predecessor: CodexActionView; successorActionId: string }
  /** 复审 round 4（V2）：写右已被在途派发取得、尝试未收尾——保持待确认（不是
   * converged；收尾后可再核对，人工继续仍可用）。 */
  | { status: "write_right_in_flight"; action: CodexActionView };

/** POST /codex/action/manual-resume 请求体（camelCase；deny_unknown_fields）。 */
export interface ManualResumeBody {
  actionId: string;
  newActionId: string;
  /** 必须与原动作正文一致（服务器权威——后继重发的就是原消息）。 */
  text: string;
  reason?: string;
}

/**
 * 明确人工继续（S4 设计 §6.2）：只读核对仍无结论时的知情出口。调用方必须先展示
 * 三件风险（原消息仍可能迟到；再次发送产生新消息；本操作不是证明原消息未送达）
 * 并拿到用户确认——本函数不做任何确认，只执行。
 * 失败：404 no_action；409 queued_use_retract / manual_resume_text_mismatch /
 * action_conflict；403/503 静态门（事务前拒绝零写入）。
 */
export async function manualResumeCodexAction(
  body: ManualResumeBody
): Promise<CodexManualResumeResult> {
  let r: Response;
  try {
    r = await fetch(CODEX_ACTION_MANUAL_RESUME_PATH, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify(body),
    });
  } catch (e) {
    throw networkFail("codex/action/manual-resume", e);
  }
  if (!r.ok) throw await toApiError("codex/action/manual-resume", r);
  return (await r.json()) as CodexManualResumeResult;
}

/**
 * 恢复被打断暂停的发送队列（S2：暂停的**唯一**解除出口；幂等——未暂停同样 200
 * `{ok, paused:false}`）。
 */
export async function resumeCodexQueue(
  sessionId: string
): Promise<{ ok: boolean; paused: boolean }> {
  let r: Response;
  try {
    r = await fetch(CODEX_QUEUE_RESUME_PATH, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ sessionId }),
    });
  } catch (e) {
    throw networkFail("codex/queue/resume", e);
  }
  if (!r.ok) throw await toApiError("codex/queue/resume", r);
  return (await r.json()) as { ok: boolean; paused: boolean };
}

// ==== T06：已入队消息的显式优先处理 ====

/** 优先处理响应判别联合（后端 `PrioritizeResp`：`result` 判别，字段 camelCase）。
 *  全部携带**原 actionId**；receipt/action 为既有动作回执或账本视图。 */
export type CodexPrioritizeResult =
  /** 越队派发已受理（一次性授权恰好消费一次；旧队列保持暂停）。 */
  | { result: "prioritized"; actionId: string; receipt: ActionReceipt }
  /** 原动作已离开队列（并发派发赢者/已终态）——`action` 是账本实际状态。 */
  | { result: "already_dispatched"; actionId: string; action: CodexActionView }
  /** 明确零投递。`queued`=动作是否仍在队列（true=入口保留可重试）；`receipt`=
   *  已落账的最终回执（未落账为 null）。 */
  | {
      result: "not_sent";
      actionId: string;
      reasonCode: string;
      receipt: ActionReceipt | null;
      queued: boolean;
    }
  /** 结果未知（停止不确定/前台预算到期）——未派发、不自动重试，稍后核对。 */
  | { result: "unknown"; actionId: string; reasonCode: string };

/**
 * 已入队消息的显式优先处理（T06「先处理这条」）：对具体 queued 消息发起越队——
 * **不接受正文副本**（后端 deny_unknown_fields 拒 text 等多余键），派发的永远是
 * 原 queued 行（原 actionId/原 wire 文本/原 client_message_id）；旧队列保持暂停。
 *
 * `confirmInterrupt`：桌面回合运行中必须 true（服务端才会定向停止当前回合）；
 * false + 运行中 → 明确零停止零越队（not_sent/interrupt_confirmation_required）。
 * `expectedTurnId` 由调用方在**确认点击时刻**冻结（与 T05 停止纪律同源）；缺省由
 * 服务端取在册屏障/镜像活动回合作安全阀。`frontBudgetMs` 可选（缺省 30s）。
 */
export async function prioritizeCodexQueueMessage(body: {
  sessionId: string;
  actionId: string;
  expectedTurnId?: string;
  confirmInterrupt: boolean;
}): Promise<CodexPrioritizeResult> {
  let r: Response;
  try {
    r = await fetch(CODEX_QUEUE_PRIORITIZE_PATH, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify(body),
    });
  } catch (e) {
    throw networkFail("codex/queue/prioritize", e);
  }
  if (!r.ok) throw await toApiError("codex/queue/prioritize", r);
  return (await r.json()) as CodexPrioritizeResult;
}

// ==== actionId 生成 ====

/**
 * 生成 Tuvis 动作 UUID（v4）。一次用户动作生成一次：发送/批准/拒绝/回答各自独立；
 * 同一动作的失败重试**沿同 ID**（服务端按 actionId 去重，双击/重试不双投）。
 * 三类 UUID（action / envelope / clientUserMessageId）独立生成，此处只出第一种。
 */
export function newActionId(): string {
  const c = globalThis.crypto;
  if (c && typeof c.randomUUID === "function") return c.randomUUID();
  // 旧内核兜底：RFC 4122 v4 手工拼装（version 4 + variant 10）
  const b = new Uint8Array(16);
  if (c && typeof c.getRandomValues === "function") {
    c.getRandomValues(b);
  } else {
    for (let i = 0; i < 16; i += 1) b[i] = Math.floor(Math.random() * 256);
  }
  b[6] = (b[6] & 0x0f) | 0x40;
  b[8] = (b[8] & 0x3f) | 0x80;
  const hex = (slice: Uint8Array): string =>
    Array.from(slice)
      .map((x) => x.toString(16).padStart(2, "0"))
      .join("");
  return `${hex(b.slice(0, 4))}-${hex(b.slice(4, 6))}-${hex(b.slice(6, 8))}-${hex(
    b.slice(8, 10)
  )}-${hex(b.slice(10, 16))}`;
}

// ==== T08：移动端适配详情与开关（F08）====

/** 适配详情（GET /codex/adapt-status；**不携包路径/源码**）。 */
export interface CodexAdaptDetails {
  sessionId: string;
  systemVersion: string;
  tuvisVersion: string;
  desktopBuild: string | null;
  autoAdaptEnabled: boolean;
  hintSeen: boolean;
  effectiveOperations?: CodexEffectiveOperations;
  lastFailure: CodexFailureObservation | null;
}

/** 最近失败观测（与成功证据分离）。 */
export interface CodexFailureObservation {
  capability: string;
  state: string;
  stage: string | null;
  errorCode: string | null;
  observedAt: number | null;
  nextRetryAt: number | null;
}

/** 开关变更（严格白名单：只允许 set_enabled / dismiss_hint 两种组合）。 */
export type CodexAdaptChange =
  | { sessionId: string; action: "set_enabled"; enabled: boolean }
  | { sessionId: string; action: "dismiss_hint" };

/** 异步正式答题请求体（**不含** raw/owner/method/attachments）。 */
export interface AsyncAnswerBody {
  sessionId: string;
  actionId: string;
  questionKey: string;
  contextToken: string;
  answer: string;
  bypassPausedQueue?: boolean;
  /** 前台剩余预算（毫秒；省略 = 服务端缺省 30_000，仍夹上限）。 */
  frontBudgetMs?: number;
}

function asAdaptDetails(v: unknown): CodexAdaptDetails {
  const d = v as CodexAdaptDetails | null;
  if (
    d === null ||
    typeof d !== "object" ||
    typeof d.sessionId !== "string" ||
    typeof d.autoAdaptEnabled !== "boolean" ||
    typeof d.hintSeen !== "boolean"
  ) {
    throw new ApiError(200, "codex/adapt-status: malformed details payload");
  }
  return {
    sessionId: d.sessionId,
    systemVersion: typeof d.systemVersion === "string" ? d.systemVersion : "",
    tuvisVersion: typeof d.tuvisVersion === "string" ? d.tuvisVersion : "",
    desktopBuild: typeof d.desktopBuild === "string" ? d.desktopBuild : null,
    autoAdaptEnabled: d.autoAdaptEnabled,
    hintSeen: d.hintSeen,
    effectiveOperations: asEffectiveOperations(d.effectiveOperations),
    lastFailure:
      d.lastFailure && typeof d.lastFailure === "object"
        ? {
            capability: String(d.lastFailure.capability ?? ""),
            state: String(d.lastFailure.state ?? ""),
            stage: d.lastFailure.stage ?? null,
            errorCode: d.lastFailure.errorCode ?? null,
            observedAt: d.lastFailure.observedAt ?? null,
            nextRetryAt: d.lastFailure.nextRetryAt ?? null,
          }
        : null,
  };
}

/** 读取适配详情（只读；不触发任何写动作）。 */
export async function fetchCodexAdaptStatus(
  sessionId: string,
  signal?: AbortSignal
): Promise<CodexAdaptDetails> {
  const q = new URLSearchParams({ sessionId });
  let r: Response;
  try {
    r = await fetch(`${CODEX_ADAPT_STATUS_PATH}?${q}`, { signal });
  } catch (e) {
    throw networkFail("codex/adapt-status", e);
  }
  if (!r.ok) throw await toApiError("codex/adapt-status", r);
  return asAdaptDetails(await r.json());
}

/** 更新自动适配开关或首次提示（服务端严格白名单校验）。 */
export async function updateCodexAdaptSettings(
  body: CodexAdaptChange,
  signal?: AbortSignal
): Promise<CodexAdaptDetails> {
  let r: Response;
  try {
    r = await fetch(CODEX_ADAPT_SETTINGS_PATH, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(body),
      signal,
    });
  } catch (e) {
    throw networkFail("codex/adapt-settings", e);
  }
  if (!r.ok) throw await toApiError("codex/adapt-settings", r);
  return asAdaptDetails(await r.json());
}

/** 异步题正式答题（独立动作类别；回执直接是 ActionReceipt，不返回 Queued）。 */
export async function answerCodexAsync(
  body: AsyncAnswerBody,
  signal?: AbortSignal
): Promise<ActionReceipt> {
  let r: Response;
  try {
    r = await fetch(CODEX_ASYNC_ANSWER_PATH, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(body),
      signal,
    });
  } catch (e) {
    throw networkFail("codex/async-answer", e);
  }
  if (!r.ok) throw await toApiError("codex/async-answer", r);
  const receipt = (await r.json()) as ActionReceipt | null;
  if (receipt === null || typeof receipt !== "object" || typeof receipt.actionId !== "string") {
    throw new ApiError(200, "codex/async-answer: malformed receipt payload");
  }
  return receipt;
}

/** B05：同组多题一次正式提交（服务端从当前原题验证整组；客户端不传 raw）。 */
export const CODEX_ASYNC_ANSWER_GROUP_PATH = "/m/api/v1/codex/async-answer-group";

export interface AsyncAnswerGroupEntry {
  questionKey: string;
  contextToken: string;
  answer: string;
}

export interface AsyncAnswerGroupBody {
  sessionId: string;
  actionId: string;
  answers: AsyncAnswerGroupEntry[];
  /** G08：显式复核确认“已由别端回答”的题（不得再次派发）。 */
  acknowledgedAnswered?: string[];
  frontBudgetMs?: number;
}

export async function answerCodexAsyncGroup(
  body: AsyncAnswerGroupBody,
  signal?: AbortSignal
): Promise<ActionReceipt> {
  let r: Response;
  try {
    r = await fetch(CODEX_ASYNC_ANSWER_GROUP_PATH, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(body),
      signal,
    });
  } catch (e) {
    throw networkFail("codex/async-answer-group", e);
  }
  if (!r.ok) throw await toApiError("codex/async-answer-group", r);
  const receipt = (await r.json()) as ActionReceipt | null;
  if (receipt === null || typeof receipt !== "object" || typeof receipt.actionId !== "string") {
    throw new ApiError(200, "codex/async-answer-group: malformed receipt payload");
  }
  return receipt;
}

/** queued→cancelled CAS 成功才返回可编辑正文；409 不得回填。 */
export async function editCodexQueueMessage(
  sessionId: string,
  actionId: string
): Promise<{ actionId: string; text: string }> {
  let r: Response;
  try {
    r = await fetch("/m/api/v1/codex/queue/edit", {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ sessionId, actionId }),
    });
  } catch (e) {
    throw networkFail("codex/queue/edit", e);
  }
  if (!r.ok) throw await toApiError("codex/queue/edit", r);
  return (await r.json()) as { actionId: string; text: string };
}
