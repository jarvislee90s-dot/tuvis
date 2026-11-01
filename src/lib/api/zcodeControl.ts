// ZCode 统一控制手机 API 客户端（T7.2）：纯 fetch，零 Tauri 依赖（PWA/浏览器同构）。
//
// **DTO 唯一定义处**：本文件类型与 Rust `services/zcode_control/types.rs` 的 serde
// 输出**逐字段相同**（`#[serde(rename_all = "camelCase")]` 的字段名；枚举按各自
// `rename_all` 的原值：`ConnectionState`/`ActionState` 为 snake_case，
// `PendingKind`/`ControlEventKind` 为 camelCase）。TS 不能 import Rust 类型，任一侧
// 漂移都是契约破坏——由 tests/mobile/zcodeControlApi.test.ts 的 **TS 手写契约测试**
// 锁定（合成 DTO 夹具，**不是** Rust golden JSON 对拍：Rust 侧改名不会让该测试变红，
// 跨语言漂移仍需人工/端到端核对）。
//
// 纪律：只走官方 `/m/api/v1/zcode/*`（T7.1 冻结入口）；**绝不**回退旧 `session-send`
// 或无头 CLI 通道（那会把官方动作变成终端按键/伪造投递）。错误体只有冻结码
// `{error}`，不含上游原文；非 2xx 一律抛 [`ZcodeControlApiError`]。

/** GET 连接视图 */
export const ZCODE_CONNECTION_PATH = "/m/api/v1/zcode/connection";
/** GET 会话投影（`?sessionId=<原 ID>`） */
export const ZCODE_SESSION_PATH = "/m/api/v1/zcode/session";
/** POST 统一控制动作（`{actionId, action}`） */
export const ZCODE_ACTIONS_PATH = "/m/api/v1/zcode/actions";
/** GET 配对 SSE 事件流（`?sessionId=<原 ID>`；URL 由 [`zcodeEventsUrl`] 生成） */
export const ZCODE_EVENTS_PATH = "/m/api/v1/zcode/events";

// ==== 冻结错误码（与 Rust `zcode_control::codes` 逐字对应，不得增删/本地化）====

export const ZCODE_ERROR_CODES = [
  "needs_config",
  "needs_auth",
  "offline",
  "conflict",
  "unknown",
  "not_ready",
  "unsupported_protocol",
  "unsupported",
  "client_mismatch",
  "needs_evidence",
  "no_session",
  "target_ambiguous",
  "stale_generation",
  "stale_pending",
  "invalid_action",
  "invalid_answer",
  "forbidden_option",
  "action_payload_conflict",
  "storage_failed",
  "payload_too_large",
  "stop_target_changed",
] as const;

export type ZcodeErrorCode = (typeof ZCODE_ERROR_CODES)[number];

const ZCODE_ERROR_CODE_SET: ReadonlySet<string> = new Set(ZCODE_ERROR_CODES);

/** 词表守卫：`reasonCode` 等字符串字段按冻结码收窄（词表外 → false，不猜）。 */
export function isZcodeErrorCode(value: unknown): value is ZcodeErrorCode {
  return typeof value === "string" && ZCODE_ERROR_CODE_SET.has(value);
}

/** 本域 HTTP 失败：`code` = 服务端 `{error}` 的冻结码（词表外归 `unknown`；
 *  非 JSON 错误体/网络层异常为 `null`），`status` = HTTP 状态（网络层为 `null`）。
 *  调用方按 `code` 查本地化文案，**不展示** `message`（只含路径与状态，无上游原文）。 */
export class ZcodeControlApiError extends Error {
  readonly status: number | null;
  readonly code: ZcodeErrorCode | null;

  constructor(status: number | null, code: ZcodeErrorCode | null, message: string) {
    super(message);
    this.name = "ZcodeControlApiError";
    this.status = status;
    this.code = code;
  }
}

async function request<T>(path: string, init?: RequestInit): Promise<T> {
  let response: Response;
  try {
    response = await fetch(path, init);
  } catch (e) {
    throw new ZcodeControlApiError(null, null, `zcode control network failure: ${String(e)}`);
  }
  if (!response.ok) {
    let code: ZcodeErrorCode | null = null;
    try {
      const body = (await response.json()) as { error?: unknown } | null;
      const raw = body?.error;
      if (typeof raw === "string") code = isZcodeErrorCode(raw) ? raw : "unknown";
    } catch {
      /* 非 JSON 错误体（代理注入页等）：code 保持 null，调用方按状态码兜底 */
    }
    throw new ZcodeControlApiError(response.status, code, `zcode control ${response.status}`);
  }
  try {
    return (await response.json()) as T;
  } catch {
    // 2xx 但载荷非 JSON：残缺响应不得被当成 DTO 消费
    throw new ZcodeControlApiError(response.status, "unknown", "zcode control malformed payload");
  }
}

// ==== DTO（camelCase / 枚举原值；来源：types.rs）====

/** relay 连接状态原值表（snake_case；运行时白名单，与 Rust 枚举逐字对应）。
 *  `connecting` 是配对**前**态，唯一「已配对」= `paired`。 */
export const ZCODE_CONNECTION_STATES = [
  "needs_config",
  "needs_auth",
  "connecting",
  "paired",
  "offline",
  "conflict",
  "unknown",
] as const;

export type ConnectionState = (typeof ZCODE_CONNECTION_STATES)[number];

const ZCODE_CONNECTION_STATE_SET: ReadonlySet<string> = new Set(ZCODE_CONNECTION_STATES);

/** 连接态词表守卫：设置卡等白名单消费方复用同一张表（不再本地复制枚举）。 */
export function isConnectionState(value: unknown): value is ConnectionState {
  return typeof value === "string" && ZCODE_CONNECTION_STATE_SET.has(value);
}

/** 连接视图：`deviceLabel` 仅供显示，不能用于绑定；`reasonCode` ∈ 冻结码。 */
export interface ConnectionView {
  state: ConnectionState;
  generation: number;
  reasonCode: string | null;
  deviceLabel: string | null;
}

/** 动作状态（snake_case）：`accepted` 只表示宿主已受理（**不是**已生效）；
 *  `unknown` 绝不允许自动重发。 */
export type ActionState =
  "accepted" | "confirmed" | "noop" | "rejected" | "send_failed" | "unknown";

/** 动作回执（不携带正文/答案/官方原值）。 */
export interface ActionReceipt {
  actionId: string;
  sessionId: string;
  generation: number;
  state: ActionState;
  reasonCode: string | null;
  sendAdmission?: { delivery: "startNow" | "queue" | "guide"; inputId: string };
}

/** pending 卡种类（camelCase）。 */
export type PendingKind = "permission" | "userInput" | "workspaceHookReview";

/** 问答选项：`value` 是应答回传值，`label` 是展示文案。 */
export interface QuestionOption {
  value: string;
  label: string;
  description: string | null;
  preview: string | null;
}

/** 单个问题；`freeTextAllowed` 派生自 payload.freeText（不是上游问题字段）。 */
export interface QuestionView {
  header: string;
  questionText: string;
  options: QuestionOption[];
  multiSelect: boolean;
  freeTextAllowed: boolean;
}

/** 权限选项：`kind` 是准入白名单依据，`optionId` 是必须回传的宿主原值。 */
export interface PermissionOptionView {
  optionId: string;
  label: string;
  kind: string;
}

/** pending 卡视图（四元组锚：session + 真实行/toolCall + interaction + generation）。 */
export interface PendingView {
  interactionId: string;
  kind: PendingKind;
  anchorRowId: number | null;
  toolCallId: string | null;
  createdAt: number | null;
  summary: string | null;
  toolName: string | null;
  schemaInteraction: string | null;
  sensitive: boolean;
  autoResolution: boolean;
  autoResolutionState: string | null;
  questions: QuestionView[];
  options: PermissionOptionView[];
  /**
   * 官方 `payload.detail` 的只读投影（审批卡正文；Bash 即命令正文）。
   *
   * 服务端已做有界截断（2000 字符 + `…`）：字符串原样透传，对象/数组取紧凑 JSON，
   * 缺失/空/null → `null`。手机端只展示，**不**回传、不落任何存储。
   */
  detail: string | null;
  /**
   * 本卡是否属于**已建模、可远程作答**的 `AskUserQuestion` 子型。
   *
   * 由服务端用与应答闸**同一个**纯判定投影（`kind=userInput` ∧
   * `toolName=AskUserQuestion` ∧ 合法 `schemaInteraction`）；前端只消费本字段，
   * 不再另建一套「差不多」的规则。`false` 时卡片渲染只读面（不给选项/提交），
   * 用户不会「选完才被 `needs_evidence` 拒」。`kind=permission` 恒为 `false`：
   * 审批的可答性由 `approvalReply` 能力与选项白名单决定，不看本字段。
   *
   * **可选字段（缺字段 = 未声明 ⇒ 按旧口径兼容；显式 false = 不可答）**：
   * 服务端（Rust `services::zcode_control::types::PendingView`）**总是**下发本字段，
   * 但旧服务端载荷 / 增量 merge 的帧可能不带它。缺字段时前端按**旧口径**
   * （`kind=userInput` ∧ `toolName=AskUserQuestion` ∧ 合法 `schemaInteraction`）
   * 兼容回退，**不是**第二套权威规则；只有服务端**显式** `false` 才降级为只读卡。
   */
  answerable?: boolean;
  /**
   * `detail` 是否被服务端按同一上限**截断**过（与文本同一次投影算出，前端不按长度猜）。
   *
   * `true` → 卡片必须说明「内容未完全展示」并撤掉「允许一次」，只留「拒绝」与
   * 「打开官方页面」：看不到全貌的审批绝不提供盲批。
   */
  detailTruncated: boolean;
}

/** control 里的一条活动（`foregroundExecutionId` 是 stop 守卫 id 的来源）。 */
export interface ActiveWork {
  kind: string;
  foregroundExecutionId: string | null;
}

/** 宿主明确宣告的 control（`activeWorks` 是数组才算「已确认」，含显式空数组）。 */
export interface ControlView {
  activeWorks: ActiveWork[];
}

/** 模型目录项（只含当前 provider）。 */
export interface ModelChoice {
  modelId: string;
  thoughtLevels: string[];
}

/** 当前模型/思考档位。 */
export interface SelectionView {
  provider: string;
  model: string;
  thought: string | null;
  models: ModelChoice[];
}

/** 单个能力开关（默认关闭并带冻结原因码）。 */
export interface CapabilityFlag {
  enabled: boolean;
  reasonCode: string | null;
}

/** 本会话能力矩阵（十一项固定字段，不复制 contract 的宽泛 true）。
 *
 *  三项 `queue*` 是**队列专属**槽位：默认关闭（`reasonCode=needs_evidence`），
 *  绝不因 `send` 开闸而放行；即使开闸，服务端每帧仍用新鲜官方队列二次校验。 */
export interface CapabilityView {
  send: CapabilityFlag;
  singleSelect: CapabilityFlag;
  multiSelect: CapabilityFlag;
  freeText: CapabilityFlag;
  interrupt: CapabilityFlag;
  approvalReply: CapabilityFlag;
  model: CapabilityFlag;
  thoughtLevel: CapabilityFlag;
  /** 队列撤回/重排（`deleteQueueItem` / `reorderQueueItem`）。 */
  queueEdit: CapabilityFlag;
  /** 队列「立即执行」（`sendQueuedNow`；**不是**「引导」）。 */
  queueNow: CapabilityFlag;
  /** 恢复队列自动处理（`setAutoDrain(true)`）。 */
  queueResume: CapabilityFlag;
}

/** 官方输入队列里的一项（`state.queue.items[]` 的只读投影）。
 *  `inputId` = 官方 `queueItemId`（宿主派生 `queue_<commandId>`，Tuvis 原样透传、
 *  **不重算**）；`position` = `order.queuePosition`（0 基，缺席 = null，绝不按数组
 *  下标猜位置）；`text` 是官方队列项正文原文（与正文同等级，可下发、不落日志）。
 *  `kind`/`dispatchState` 是服务端按证据白名单投影的官方值，白名单外 = null；
 *  `hasAttachments` 只告知**有没有**附件，绝不下发附件对象/路径。 */
export interface QueueItemView {
  inputId: string;
  text: string;
  position: number | null;
  /** 官方 `kind`（白名单 `sendText|sendGoalCommand|compact`；null = 未知）。 */
  kind: string | null;
  /** 官方 `dispatch.state`（白名单 `admitted|queued|reserved|promoting|drained`）。 */
  dispatchState: string | null;
  /** 是否带附件（只告知有无）。 */
  hasAttachments: boolean;
}

/** 官方 `state.availability` 里与队列写有关的只读资格（`known=false` = 未知/全关）。
 *
 *  证据只覆盖 `queueEdit`/`sendQueuedNow` 两项；官方没有 `setAutoDrain` 的可用位，
 *  因此「恢复」资格不由本对象表达（只看 `paused===true` + 能力闸）。 */
export interface QueueActionAvailability {
  known: boolean;
  queueEditAllowed: boolean;
  sendQueuedNowAllowed: boolean;
}

/** 官方输入队列视图（`state.queue`）。三态必须可区分：
 *  - `known=false`：宿主**未宣告**队列（旧载荷/快照缺键）→ 未知，不是空队列；
 *  - `known=true` + `items=[]`：宿主明确宣告空队列；
 *  - `paused=null`：宿主未给 `autoDrain` → **不得**当成未暂停。
 *  `pauseReason` 是官方枚举 `stopped|manual|error` 的原样透传（服务端已白名单，
 *  非枚举/缺席 = 缺省）；它只在 `paused=true` 时有展示意义，前端不猜原因。 */
export interface QueueView {
  known: boolean;
  paused: boolean | null;
  pauseReason?: string | null;
  items: QueueItemView[];
  /** 队列写资格（默认全关；只是必要条件之一，不构成「可点」的充分条件）。 */
  availability: QueueActionAvailability;
}

/** 官方 `state.inputRouting` 的最小投影（至少 `mode`；`reasonCode` 可缺省）。
 *  只用于发送合同判定：`mode === "choice"` 且队列非空时，手机必须显式选择旧队列
 *  的处置（保留/清空），并由服务端对新鲜队列做 CAS。 */
export interface InputRoutingView {
  mode: string;
  reasonCode: string | null;
}

/** 会话投影视图（手机唯一可见的会话状态面）；`ready=false` 时控制面必须零写。
 *  `queue` 可选：缺字段 = 未知（旧服务端载荷），前端绝不把缺省当成空队列。
 *  `inputRouting` 可选：缺字段 = 宿主未宣告（未知），绝不猜 `choice`。 */
export interface SessionView {
  sessionId: string;
  generation: number;
  revision: number | null;
  ready: boolean;
  pending: PendingView[];
  control: ControlView | null;
  selection: SelectionView | null;
  capabilities: CapabilityView;
  queue?: QueueView;
  inputRouting?: InputRoutingView;
}

/** 问答答案条目。 */
export interface AnswerEntry {
  questionText: string;
  selectedValues: string[];
  freeText: string | null;
}

/** `inputRouting.mode="choice"` 时对旧队列的处置（严格枚举，不接受任意字符串）。 */
export type HeldQueueDisposition = "keepQueueAndSend" | "clearQueueAndSend";

/** 发送时对旧队列的处置；`expectedQueueItemIds` 是官方当前队列的完整 ID 列表与顺序，
 *  服务端写前对新队列做 CAS，失配则拒绝并要求刷新后重选。 */
export interface HeldQueueChoice {
  disposition: HeldQueueDisposition;
  expectedQueueItemIds: string[];
}

/** 统一控制动作（tagged `kind`；全部携带 `sessionId,generation`）。 */
export type ControlAction =
  | {
      kind: "send";
      sessionId: string;
      generation: number;
      text: string;
      heldQueue?: HeldQueueChoice;
    }
  | { kind: "snooze"; sessionId: string; generation: number; interactionId: string }
  | {
      kind: "answer";
      sessionId: string;
      generation: number;
      interactionId: string;
      answers: AnswerEntry[];
    }
  | {
      kind: "permission";
      sessionId: string;
      generation: number;
      interactionId: string;
      optionId: string;
    }
  | {
      kind: "stop";
      sessionId: string;
      generation: number;
      expectedForegroundExecutionId: string;
    }
  | { kind: "selection"; sessionId: string; generation: number; model: string; thought: string }
  // ---- 队列写（W3）：目标一律是官方 `queueItemId`，绝不是位置/下标/正文 ----
  /** 撤回一条官方队列项。 */
  | { kind: "queueDelete"; sessionId: string; generation: number; queueItemId: string }
  /** 重排：把 `queueItemId` 移到 `beforeQueueItemId` 之前。
   *  `null`（官方「移末尾」语义未经实机确认）当前会被服务端拒绝。 */
  | {
      kind: "queueReorder";
      sessionId: string;
      generation: number;
      queueItemId: string;
      beforeQueueItemId: string | null;
    }
  /** **立即执行**一条队列项（`sendQueuedNow`）；不是「引导」。 */
  | { kind: "queueNow"; sessionId: string; generation: number; queueItemId: string }
  /** 恢复队列自动处理（`setAutoDrain(true)`）；不重发任何队列项。 */
  | { kind: "queueResume"; sessionId: string; generation: number };

// ==== 端点 ====

/** 当前连接视图。GET 只读：不触发 OAuth、不隐式新建 relay。 */
export async function getZcodeConnection(): Promise<ConnectionView> {
  return await request<ConnectionView>(ZCODE_CONNECTION_PATH);
}

/** 当前 Tuvis ZCode 会话投影（绑定由服务端按原 ID 完成，手机不自报 workspace）。 */
export async function getZcodeSession(sessionId: string): Promise<SessionView> {
  return await request<SessionView>(
    `${ZCODE_SESSION_PATH}?sessionId=${encodeURIComponent(sessionId)}`
  );
}

/**
 * 提交一次统一控制动作（POST `/m/api/v1/zcode/actions`）。
 *
 * 请求体**只有** `{actionId, action}`：设备身份/workspace/能力/供应商字段由服务端
 * 配对门与本机 DB 填充，多一个键都会被 400 `invalid_action` 拒绝。回执 `state`
 * 区分 accepted/confirmed/noop/unknown（200 恒定），`accepted` ≠ 已生效。
 */
export async function postZcodeAction(
  actionId: string,
  action: ControlAction
): Promise<ActionReceipt> {
  return await request<ActionReceipt>(ZCODE_ACTIONS_PATH, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ actionId, action }),
  });
}

/** 配对 SSE 事件流地址（会话作用域：唯一查询参数 = 原样编码的 sessionId）。 */
export function zcodeEventsUrl(sessionId: string): string {
  return `${ZCODE_EVENTS_PATH}?sessionId=${encodeURIComponent(sessionId)}`;
}
