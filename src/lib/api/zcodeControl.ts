// ZCode 统一控制手机 API 客户端（T7.2）：纯 fetch，零 Tauri 依赖（PWA/浏览器同构）。
//
// **DTO 唯一定义处**：本文件类型与 Rust `services/zcode_control/types.rs` 的 serde
// 输出**逐字段相同**（`#[serde(rename_all = "camelCase")]` 的字段名；枚举按各自
// `rename_all` 的原值：`ConnectionState`/`ActionState` 为 snake_case，
// `PendingKind`/`ControlEventKind` 为 camelCase）。TS 不能 import Rust 类型，任一侧
// 漂移都是契约破坏——由 tests/mobile/zcodeControlApi.test.ts 的金标 JSON 对拍。
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

/** relay 连接状态（snake_case）。`connecting` 是配对**前**态，唯一「已配对」= `paired`。 */
export type ConnectionState =
  "needs_config" | "needs_auth" | "connecting" | "paired" | "offline" | "conflict" | "unknown";

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

/** 本会话能力矩阵（八项固定字段，不复制 contract 的宽泛 true）。 */
export interface CapabilityView {
  send: CapabilityFlag;
  singleSelect: CapabilityFlag;
  multiSelect: CapabilityFlag;
  freeText: CapabilityFlag;
  interrupt: CapabilityFlag;
  approvalReply: CapabilityFlag;
  model: CapabilityFlag;
  thoughtLevel: CapabilityFlag;
}

/** 会话投影视图（手机唯一可见的会话状态面）；`ready=false` 时控制面必须零写。 */
export interface SessionView {
  sessionId: string;
  generation: number;
  revision: number | null;
  ready: boolean;
  pending: PendingView[];
  control: ControlView | null;
  selection: SelectionView | null;
  capabilities: CapabilityView;
}

/** 问答答案条目。 */
export interface AnswerEntry {
  questionText: string;
  selectedValues: string[];
  freeText: string | null;
}

/** 统一控制动作（tagged `kind`；全部携带 `sessionId,generation`）。 */
export type ControlAction =
  | { kind: "send"; sessionId: string; generation: number; text: string }
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
  | { kind: "selection"; sessionId: string; generation: number; model: string; thought: string };

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
