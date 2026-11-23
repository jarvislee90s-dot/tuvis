// OpenCode 原生交互的移动端 API（计划任务 9）：只做 DTO 镜像 + 纯 fetch，零 Tauri 依赖。
//
// 契约来源（**逐字镜像，勿凭摘要改形状**）：
// - `src-tauri/src/services/opencode_native/types.rs`（`PublicNativeView` / `PendingView` /
//   `InboxView` / `NativeOperation` / `ActionView`）；
// - `src-tauri/src/remote/opencode_native.rs`（六条路由与统一错误壳 `{error,reasonCode,reason}`）。
//
// 三处**与直觉不同**、按 serde 实际输出镜像的形状（已在报告里登记）：
// 1. `NativeOperation` 是 `#[serde(tag="kind")]` + newtype 结构体载荷 ⇒ 内部标签**扁平化**：
//    `{kind:"send", text, attachmentPaths, delivery, clientMessageId}`，不是 `{kind, value:{…}}`；
// 2. `PendingView` 是 `#[serde(tag="kind", rename_all="snake_case")]`——`rename_all` 只改
//    **变体名**，不改变体字段名 ⇒ 字段仍是 `context_token`（不是 `contextToken`）。
//    独立探针实测：`{"kind":"form","id":…,"fields":[],"context_token":"ctx","state":"unknown"}`；
// 3. `DraftAnswer` 是内部标签 + `content="value"` ⇒ `{kind:"text",value:"…"}` /
//    `{kind:"multi",value:[…]}` / `{kind:"skip"}`（skip 无 value）。
//
// 写路径纪律（宪法「动作已发但结果不明，不切到按键路径重发」）：
// `verifyOpenCodeAction` 是**只读核对**，本模块不提供任何「已发出但结果不明 ⇒ 重发」的入口。
import { ApiError } from "./api";
import type { SessionMessage } from "./api";

/** 原生通道单文件预算（5 MiB）：与后端 `MAX_ATTACHMENT_FILE_BYTES` 同值。
 *  Mac OpenCode 2.0.22 接受 5 MiB，8–20 MiB 会被上游 `Prompt.Base64` 拒绝（500）；
 *  上传端点自身的 20 MiB 上限不变，本常量只用于原生通道的**发送前**预检。 */
export const NATIVE_ATTACHMENT_MAX_BYTES = 5 * 1024 * 1024;

/** 逐操作能力表的键（`NativeOperationId` 的 snake_case wire 词）。 */
export type NativeOperationId =
  | "read_messages"
  | "send"
  | "read_inbox"
  | "retract"
  | "promote_inbox"
  | "read_pending"
  | "permission_once"
  | "permission_reject"
  | "answer_group"
  | "stop_session"
  | "set_agent"
  | "permission_always";

/** 单操作可用性（`OperationAvailability`）：`evidence` 只描述路径/形状，不含正文与凭据。 */
export interface OperationAvailability {
  allowed: boolean;
  evidence: string;
  reasonCode?: string | null;
}

/** 能力表（缺键 = 未判定 = 不可用，与后端同口径）。 */
export type NativeCapabilities = Partial<Record<NativeOperationId, OperationAvailability>>;

/** 原生选项：显示 `label`，提交 `value`（两者不互相替代）。 */
export interface FormOption {
  value: string;
  label: string;
  description?: string | null;
}

/** 字段类型（`FieldKind` 内部标签；`unsupported` 是无载荷单元变体）。 */
export type FormFieldKind =
  | { kind: "text"; options: FormOption[]; custom: boolean }
  | { kind: "multi"; options: FormOption[]; custom: boolean }
  | { kind: "unsupported" };

/** 表单字段：`required` 保留**原始三态**（缺省 ≠ true/false）。
 *
 *  `description` 是官方 `Form.StringField` / `Form.MultiselectField` 的可选题干正文
 *  （`title` 只是短标题）：只读展示用，不参与草稿签名与提交。老载荷没有该键。 */
export interface FormField {
  key: string;
  title: string;
  description?: string | null;
  required?: boolean | null;
  kind: FormFieldKind;
  when?: { key: string; value: string };
}

/** 待处理项（`PendingView`，内部标签按 `kind` 区分两分支）。
 *
 *  - `form.state` 恒为 `"unknown"`：**它不是「待答/已答」的证据**，只表示本端不知道
 *    该表单在原生侧的状态——消费方不得据此判断「还在等」或「已答完」；
 *  - 字段名是 `context_token`（serde 只改变体名，见文件头第 2 条）。 */
export type PendingView =
  | {
      kind: "form";
      id: string;
      title: string;
      fields: FormField[];
      context_token: string;
      state: string;
    }
  | {
      kind: "permission";
      id: string;
      title: string;
      decisions: string[];
      context_token: string;
    };

/** 原生 inbox 项：`queue` = 排队待消费；`steer` = 打断注入（本批写入只产生 queue）。 */
export interface InboxView {
  id: string;
  text: string;
  delivery: "queue" | "steer";
  /** 后端给出的可区分状态词：`waiting` = 等当前回合结束；`steering` = 正在打断并交付。
   *  两者都表示**已交给 OpenCode**，不是「还在本地待发」。 */
  state?: "waiting" | "steering";
}

/** 单条原生消息里的一个附件（R5c；与 Rust `NativeFileView` 逐字段镜像）。
 *
 *  `previewPath` 只在源 URI 是 `file://` 且 canonical 路径位于该会话受控附件目录
 *  （`.tuvis-attachments/<sessionId>/`）内时出现；其余附件只有元数据——**没有**
 *  `previewPath` 时不得伪造预览按钮。
 *  后端**从不**下发 base64 内容；`size` 只是按 base64 长度换算的近似值。 */
export interface NativeFileView {
  name: string;
  mime?: string;
  size?: number;
  previewPath?: string;
}

/** `GET /opencode/view` 的权威只读投影（`NativeRouteView` = 本视图 flatten + 路由字段）。
 *
 *  `route`：`opencode_native` = 该会话已归原生（旧写入口会 409）；`legacy` = 未归原生。
 *  `routeReasonCode`：路由/写入不可用的受控原因词。除「尝试认领被拒」（如
 *  `legacy_pending`）外，**同实例归属未证（B1）也会下发**；已有 active 归属的会话
 *  仍可能是 `route=opencode_native` + 该码，不能把「有原因词」当成「未归原生」。
 *  **打开本视图会认领路由**（验证开关打开且投影成功时）——UI 必须如实告知用户。 */
export interface PublicNativeView {
  /** 当前快照行到原生记录 id；只在该快照内用 seq 查表，持久锚只存原生 id。 */
  messageIds?: Record<string, string>;
  sessionId: string;
  serviceId?: string;
  servicePid?: number;
  serviceUrl?: string;
  directory?: string;
  /** 服务身份门（版本化契约，R2 起）：新后端 `bindingRequired = 服务身份未核验`
   *  （binding_required = !verified，见 projection.rs commit 的调用方注释）——true 时
   *  **desktopBound 不能解锁**（desktop 连接证据覆盖不了身份拒绝）。
   *  旧后端该字段恒 true（桌面门语义，desktopBound=true 即解锁）；区分信号见
   *  {@link PublicNativeView.routeReasonCode}。缺省（缺字段）按未要求处理。 */
  bindingRequired?: boolean;
  /** 观察到的桌面连接证据（进程参数工程检测，非密码学证明）。**只作信息面**：
   *  它不是「允许写」的判据，更不能覆盖 `bindingRequired=true` 的身份拒绝。 */
  desktopBound?: boolean;
  bindingToken: string;
  routeVersion?: number | null;
  revision: number;
  /** 本次消息分页是否读到权威终止（空页/游标耗尽）。`false` = 部分历史：
   *  UI 必须显示「可能不完整」提示，且不得据此判定某条消息未投递。 */
  complete: boolean;
  /** D-N5-4：`"OC_RESPONSE_TOO_LARGE"` = 部分历史原因是「部分消息单页超过响应预算
   *  已停止载入」（如恰 20 MiB 合计附件）——UI 据此如实标注「部分内容过大未载入」，
   *  不与其它部分原因混同；缺省 = 原因不可归因（保持未知），不得推断跳过数量/内容。 */
  partialReason?: string | null;
  capabilities: NativeCapabilities;
  messages: SessionMessage[];
  pending: PendingView[];
  inbox: InboxView[];
  runtimeState: string;
  /** Session.Info.permissions 原始规则；不是合并后的最终有效权限。 */
  permissionRules?: NativePermissionRule[];
  permissionRulesKnown?: boolean;
  /** R5c：附件按消息 `seq` 关联（JSON 的 map 键是字符串）。 */
  files: Record<string, NativeFileView[]>;
  /** 投影行 → 该条 tool-call 的原生调用 id（用于把表单结果精确归位到同一组提问）。 */
  toolCallIds?: Record<string, string>;
  /** 投影行 → 该条 `question` 调用的**正式答案**（题目 + 答案文本）。
   *  来源是消息记录里的官方 answers：表单作答后上游会删除该表单，这是唯一可持久展示的来源。 */
  questionAnswers?: Record<string, { title: string; answer: string }[]>;
  route?: "opencode_native" | "legacy";
  routeReasonCode?: string | null;
}

/** 原生权限规则（只读展示；不推断继承/合并）。 */
export interface NativePermissionRule {
  action: string;
  resource: string;
  effect: string;
}

/** 投递方式（首批写入只放行 `queue`；`steer` 仅用于读取既有 inbox 展示）。 */
export type DeliveryKind = "queue" | "steer";

/** 草稿答案：合法跳过 = **不写入该键**（`skip` 绝不编码为空串）。 */
export type DraftAnswer =
  { kind: "text"; value: string } | { kind: "multi"; value: string[] } | { kind: "skip" };

/** 原生操作（`NativeOperation`，内部标签扁平化——见文件头第 1 条）。 */
export type NativeOperation =
  | {
      kind: "send";
      text: string;
      attachmentPaths: string[];
      delivery: DeliveryKind;
      clientMessageId: string;
    }
  | {
      kind: "answer";
      formId: string;
      contextToken: string;
      answers: Record<string, DraftAnswer>;
    }
  | { kind: "decide"; requestId: string; contextToken: string; decision: string }
  | { kind: "retract"; itemId: string }
  | { kind: "set_outbox_paused"; paused: boolean; expectedRevision: number }
  | { kind: "retract_outbox" | "rebind_outbox"; targetActionId: string }
  /** 插队（本地待发 → 原生）：把队列里**既有那一行**以 OpenCode 原生 steer 投出去
   *  （自带打断当前回合）。载荷只有目标 actionId——**不携带正文副本**。 */
  | { kind: "prioritize_outbox"; targetActionId: string }
  /** 插队（原生 inbox 内）：把**已在 OpenCode 队列**的 queue 项 PATCH 成 steer，
   *  由上游在当前回合交付并打断。载荷只有 inbox 项 id——同样不带正文。 */
  | { kind: "promote_inbox"; itemId: string }
  | { kind: "stop_session" }
  | { kind: "set_agent"; agent: string };

/** 手机提交的原生命令（唯一原生写入口的请求体；后端 `deny_unknown_fields`）。 */
export interface NativeCommand {
  actionId: string;
  sessionId: string;
  bindingToken: string;
  operation: NativeOperation;
}

/** 动作账本状态（snake_case wire 词）。
 *  `unknown_confirmed`（patch3 D2）= 用户显式确认「结果最终未知、本端不再等待」的
 *  **归档态**：不是「未发出」也不是任何定案，只是不再阻挡显式重选/交回。 */
export type ActionState =
  | "queued"
  | "not_sent"
  | "dispatching"
  | "acknowledged"
  | "confirmed"
  | "unconfirmed"
  | "not_accepted"
  | "settled"
  | "unknown_confirmed";

/** 动作视图（只读核对用）。
 *
 *  `runtimeDead`（M3，P1-c）：**行代际呈现面**，只对 `acknowledged`/`unconfirmed`
 *  行可能由后端携带——账本按登记文件 + 进程事实（与 confirm-unknown 权威闸同一
 *  判定惯例）无法证明「行代际 = 当前活代际」时为 `true`，可证明时为 `false`；
 *  **缺省 = 未判定 ≠ 死亡**，消费方不得把缺席读成任一方向。权威闸仍在
 *  `confirm-unknown` 端点（fail-closed），本字段只驱动「已受理行」何时挂确认出口。 */
export interface ActionView {
  actionId: string;
  state: ActionState;
  reasonCode?: string | null;
  nativeId?: string | null;
  runtimeDead?: boolean;
}

/** 原生表单结果状态（R5b）：`unknown` 与 `cancelled` 是**不同**事实，不得合并。 */
export type FormResultStatus = "answered" | "cancelled" | "pending" | "unknown";

/** 单个字段的官方答案：`text` = 单值，`multi` = 多选，其它 JSON 形态原样归 `unknown`。
 *  与后端 `FormAnswerValue` 逐字镜像（内部标签 + `content`）。 */
export type FormAnswerValue =
  | { kind: "text"; value: string }
  | { kind: "multi"; value: string[] }
  | { kind: "unknown"; value: unknown };

/** 原生表单结果（`NativeFormResult`，R5b）：按原 `sessionId + formId` 只读回读。
 *
 *  `answers` 只在 `status === "answered"` 时有值；`cancelled`/`pending`/`unknown`
 *  一律为空对象——消费方不得把后三者当成「已答」。
 *  **这份 DTO 不进 sessionStorage**：面板只持久化 formId，刷新后重新拉取。 */
export interface NativeFormResult {
  formId: string;
  sessionId: string;
  status: FormResultStatus;
  answers: Record<string, FormAnswerValue>;
  /** 官方 `metadata.tool.messageID`：生成这次提问的 assistant 消息 id（可选）。 */
  toolMessageId?: string;
  /** 官方 `metadata.tool.id`：这次提问对应的工具调用 id（可选）。 */
  toolCallId?: string;
  /** 后端按官方表单规格派生的可读答案（题目 + 选项 label）；缺省 = 无可展示答案。
   *  前端只渲染这里，**不**把 `answers` 的键/值直接上屏。 */
  readableAnswers?: { title: string; answer: string }[];
}

/** 拉取原生表单结果（R5b）：**只读**，绝不写动作账本/路由。
 *
 *  404 = 不存在 / 其他会话 / 读失败（后端不泄露存在性）：调用方必须显示「结果未读到」，
 *  不得据此推断表单不存在或已定案。`signal` 中止按 AbortError 原样抛出。 */
function boundReadInit(
  signal?: AbortSignal,
  expectedBinding?: string | null
): RequestInit | undefined {
  if (!signal && !expectedBinding) return undefined;
  return {
    ...(signal ? { signal } : {}),
    ...(expectedBinding ? { headers: { "X-OpenCode-Expected-Binding": expectedBinding } } : {}),
  };
}

export async function fetchNativeFormResult(
  sessionId: string,
  formId: string,
  signal?: AbortSignal,
  expectedBinding?: string | null
): Promise<NativeFormResult> {
  if (signal?.aborted) throw new DOMException("aborted", "AbortError");
  const q = new URLSearchParams({ session_id: sessionId });
  let r: Response;
  try {
    r = await fetch(
      `/m/api/v1/opencode/forms/${encodeURIComponent(formId)}?${q}`,
      boundReadInit(signal, expectedBinding)
    );
  } catch (e) {
    if (signal?.aborted) throw e; // 主动中止：交给调用方按取消处理
    throw new ApiError(
      null,
      `MOBILE_NETWORK_FAIL:opencode-form-result: opencode-form-result 网络异常: ${String(e)}`
    );
  }
  if (!r.ok) {
    throw new ApiError(r.status, `opencode-form-result ${r.status}`, await readErrorBody(r));
  }
  return (await r.json()) as NativeFormResult;
}

/** 原生修订号事件流端点（EventSource 与测试替身共用同一来源）。 */
export const OPENCODE_NATIVE_EVENTS_PATH = "/m/api/v1/opencode/events";

/** 动作 id 形态：后端只接受 `[A-Za-z0-9_]`（关联 id 由它派生 `msg_<action_id>`）。
 *  `crypto.randomUUID()` 自带连字符**不合法**，故在这里统一归一。 */
export function newNativeActionId(): string {
  const c = globalThis.crypto;
  const raw =
    c && typeof c.randomUUID === "function"
      ? c.randomUUID()
      : `${Date.now().toString(16)}${Math.floor(Math.random() * 1e9).toString(16)}`;
  return `act_${raw.replace(/[^A-Za-z0-9]/g, "")}`;
}

/** 由 action id 派生关联 id（后端要求逐字相等，否则 400 `OC_ACTION_INVALID`）。 */
export function clientMessageIdOf(actionId: string): string {
  return `msg_${actionId}`;
}

/** 统一错误壳解析：后端回 `{error, reasonCode, reason}`（三种键可能同值）。 */
async function readErrorBody(r: Response): Promise<Record<string, unknown> | null> {
  try {
    return (await r.json()) as Record<string, unknown>;
  } catch {
    return null; // 非 JSON 错误体（代理注入页等）：保留 null，由调用方按状态码兜底
  }
}

/** 后端错误体里的受控码（`error` 与 `reasonCode` 同值时优先前者）。 */
export function nativeErrorCode(e: ApiError): string | null {
  const raw = e.data?.error ?? e.data?.reasonCode;
  return typeof raw === "string" && raw.length > 0 ? raw : null;
}

/** H5：**服务端统一端到端预算**（毫秒）的手机侧镜像常量——与后端
 *  `src-tauri/src/services/opencode_native/budget.rs::ACTION_E2E_BUDGET_MS` 同值
 *  （45s；两侧修改必须同步）。统一预算覆盖服务端动作链接收 → 正式回执全链；
 *  派发前超时 ⇒ 后端落「确定未发送」`OC_DISPATCH_BUDGET_EXHAUSTED`（确定零写出）。 */
export const OPENCODE_STOP_SERVER_BUDGET_MS = 45_000;

/** 停止 POST 的**专属**客户端等待 = 服务端预算 + 余量（5s）。对齐裁决：预算内
 *  服务端必回正式回执（settled / 确定未发送 / 受控拒绝），客户端等待比预算宽才
 *  能收到它——比通用 {@link OPENCODE_ACTION_POST_TIMEOUT_MS}（10s）提前掐断会把
 *  慢链（单飞等待 / DB 锁窗）里的合法停止误报成未知态。超时语义不变：**结果未知，
 *  不自动重发**（只读核对，按原 action ID）。 */
export const OPENCODE_STOP_POST_TIMEOUT_MS = OPENCODE_STOP_SERVER_BUDGET_MS + 5_000;

/** 客户端等待上限。超时只查询本次回执，服务端可能仍在执行。 */
export const OPENCODE_RESELECT_POST_TIMEOUT_MS = 50_000;

/** view 认领拒绝体里的路由行版本（patch6 O-M9-2 ①，后端在路由行在场时追加的
 *  只读 DB 事实）：「尚未接管」面（view 409）的显式重选请求据此一次成形，
 *  不再因缺 routeVersion 必 409 成结构性循环（O-M9-2 实测形态）。
 *  缺省/非法（旧后端无此字段）= null：面板保持既有行为（如实失败、可再点），
 *  绝不把缺席读成任何版本。 */
export function viewErrorRouteVersion(e: ApiError | null): number | null {
  const raw = e?.data?.routeVersion;
  return typeof raw === "number" && Number.isSafeInteger(raw) && raw >= 1 ? raw : null;
}

/** 拉取权威只读视图——**同时是「选择原生路由」的入口**（开关开且投影成功即认领）。
 *  403（设备失效，与其余端点同语义）→ 抛 ApiError，由上层按既有全局判废通道处理；
 *  其余受控码（`OC_SERVICE_NOT_REGISTERED` / `OC_PLATFORM_UNVERIFIED` /
 *  `OC_CONTRACT_MISSING` 等）同样抛 ApiError，调用方经 {@link nativeErrorCopy} 呈现。
 *  `signal` 中止按 AbortError 原样抛出（不包装成网络失败）。 */
export async function fetchOpenCodeView(
  sessionId: string,
  signal?: AbortSignal,
  expectedBinding?: string | null
): Promise<PublicNativeView> {
  if (signal?.aborted) throw new DOMException("aborted", "AbortError");
  const q = new URLSearchParams({ session_id: sessionId });
  let r: Response;
  try {
    r = await fetch(`/m/api/v1/opencode/view?${q}`, boundReadInit(signal, expectedBinding));
  } catch (e) {
    if (signal?.aborted) throw e; // 主动中止：交给调用方按取消处理
    throw new ApiError(
      null,
      `MOBILE_NETWORK_FAIL:opencode-view: opencode-view 网络异常: ${String(e)}`
    );
  }
  if (!r.ok) {
    throw new ApiError(r.status, `opencode-view ${r.status}`, await readErrorBody(r));
  }
  return (await r.json()) as PublicNativeView;
}

/** 只读 agent 视图（`GET /opencode/agents`）：current 读不到时为 null，不猜默认值。 */
export interface NativeAgentOption {
  id: string;
  name: string;
  description?: string | null;
}
/** 会话当前模型引用（只读；与 Rust `NativeModelRef` 逐字段镜像）。 */
export interface NativeModelRef {
  id: string;
  /** 保留上游拼写 `providerID`（与 `Model.Ref` 逐字一致）。 */
  providerID: string;
  variant?: string;
}

export interface NativeAgentView {
  current: string | null;
  options: NativeAgentOption[];
  /** 当前会话模型；缺字段 = 无可靠数据，界面整段省略（不猜默认值）。 */
  model?: NativeModelRef | null;
  /** 模型展示名（后端尽力解析；缺失时前端只显示 `id`）。 */
  modelName?: string;
}

export async function fetchOpenCodeAgents(
  sessionId: string,
  signal?: AbortSignal,
  expectedBinding?: string | null
): Promise<NativeAgentView> {
  if (signal?.aborted) throw new DOMException("aborted", "AbortError");
  const q = new URLSearchParams({ session_id: sessionId });
  let r: Response;
  try {
    r = await fetch(`/m/api/v1/opencode/agents?${q}`, boundReadInit(signal, expectedBinding));
  } catch (e) {
    if (signal?.aborted) throw e;
    throw new ApiError(null, `MOBILE_NETWORK_FAIL:opencode-agents: ${String(e)}`);
  }
  if (!r.ok) {
    throw new ApiError(r.status, `opencode-agents ${r.status}`, await readErrorBody(r));
  }
  return (await r.json()) as NativeAgentView;
}

/** 写 POST 的统一**客户端**超时（D1，H13 证据 §10）：10s，与后端 echo 门 W 窗口上限
 *  同量级。背景：浏览器每源 6 连接池在双标签（SSE+轮询）在场时可把新 POST 饿死——
 *  实测挂起 257s 无任何出口。超时语义 = **结果未知**（请求可能已到达服务端），不是
 *  「没发出」：错误必须携带 {@link OC_RESPONSE_INVALID_MARKER}（既有未知态标记），
 *  调用方按「结果未确认 → 只读核对，不重投」呈现，绝不自动重发。 */
export const OPENCODE_ACTION_POST_TIMEOUT_MS = 10_000;

/** 客户端超时的统一落点：可辨未知态（status=null + reasonCode=OC_RESPONSE_INVALID）。
 *  与普通网络错误（无 reasonCode）在错误形状上可辨——测试与调用方都据此分流。 */
function postTimeoutError(scope: string): ApiError {
  return new ApiError(
    null,
    `MOBILE_TIMEOUT:${scope}: ${scope} 客户端超时（结果未知，不自动重发）`,
    { reasonCode: OC_RESPONSE_INVALID_MARKER }
  );
}

/** 提交原生命令（**唯一**原生写入口）。200 含 `Unconfirmed`（结果未知）——未知不是
 *  错误码，是一个必须如实呈现的终态；非 2xx 的受控拒绝才抛 ApiError。
 *  发送和恢复派发按服务端动作预算加响应余量等待。其他操作沿用各自超时，超时按未知态抛出。
 *
 *  `signal`（可选，2026-11-22 输入区就地停止键）：调用方主动中止本次投递。中止按
 *  AbortError **原样**抛出（不折成客户端超时/零写出）——请求可能已到达服务端，调用方
 *  必须按「结果未确认」处理（保留恢复锚、不自动重发）。 */
export async function submitOpenCodeAction(
  command: NativeCommand,
  timeoutMs: number = command.operation.kind === "send" ||
  (command.operation.kind === "set_outbox_paused" && !command.operation.paused)
    ? OPENCODE_STOP_SERVER_BUDGET_MS + 5_000
    : OPENCODE_ACTION_POST_TIMEOUT_MS,
  signal?: AbortSignal
): Promise<ActionView> {
  if (signal?.aborted) throw new DOMException("aborted", "AbortError");
  const controller = new AbortController();
  const onExternalAbort = () => controller.abort();
  signal?.addEventListener("abort", onExternalAbort);
  const timer = setTimeout(() => controller.abort(), timeoutMs);
  try {
    const r = await fetch("/m/api/v1/opencode/actions", {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify(command),
      signal: controller.signal,
    });
    if (!r.ok) {
      const data = await readErrorBody(r);
      if (controller.signal.aborted) throw postTimeoutError("opencode-actions");
      throw new ApiError(r.status, `opencode-actions ${r.status}`, data);
    }
    return (await r.json()) as ActionView;
  } catch (e) {
    // 调用方主动中止：原样抛出（结局未知，绝不宣称零写出）
    if (signal?.aborted) throw e;
    if (controller.signal.aborted || isAbortError(e)) throw postTimeoutError("opencode-actions");
    if (e instanceof ApiError) throw e;
    throw new ApiError(
      null,
      `MOBILE_NETWORK_FAIL:opencode-actions: opencode-actions 网络异常: ${String(e)}`
    );
  } finally {
    clearTimeout(timer);
    signal?.removeEventListener("abort", onExternalAbort);
  }
}

/** 显式**重新进入**原生交互（评审 R-2）。
 *
 *  为什么必须显式：`draining` 是用户点过「交回旧路径」的结果。若允许 `GET /opencode/view`
 *  把它重新认领，一次刷新/换会话/第二台配对设备就会把退回操作静默撤销，旧写入口再次 409。
 *
 *  被拒（旧队列仍有未决行 / 开关已关）→ 抛 `ApiError`（409 `OC_ROUTE_CHANGED` /
 *  `OC_ROUTE_NOT_OWNED`），零变更；已 `active` 时幂等成功。 */
/** 候选预览（只读）：确认前固定「用户看到的目标」。
 *
 *  返回的 candidate.bindingToken 只在服务端 TTL 内、同设备、同监听代际可作为
 *  {@link enterOpenCodeNative} 的 candidateToken；候选变化时确认会被服务端拒绝。 */
export type OpenCodeCandidatePreview = {
  sessionId: string;
  candidate: {
    sessionId: string;
    directory: string;
    serviceId: string;
    pid: number;
    startedAtMs: number;
    bindingToken: string;
  };
  expiresAtMs: number;
  /** 一次性随机确认凭据：candidateToken 用它，不复用稳定 bindingToken。 */
  previewToken: string;
  previousRoute?: { routeVersion: number; bindingToken: string | null } | null;
};

export async function previewOpenCodeCandidate(
  sessionId: string,
  signal?: AbortSignal
): Promise<OpenCodeCandidatePreview> {
  let r: Response;
  try {
    r = await fetch(`/m/api/v1/opencode/route/preview?sessionId=${encodeURIComponent(sessionId)}`, {
      signal,
    });
  } catch (e) {
    if (signal?.aborted || isAbortError(e)) throw e;
    throw new ApiError(
      null,
      `MOBILE_NETWORK_FAIL:opencode-route-preview: opencode-route-preview 网络异常: ${String(e)}`
    );
  }
  if (!r.ok) {
    throw new ApiError(r.status, `opencode-route-preview ${r.status}`, await readErrorBody(r));
  }
  let value: unknown;
  try {
    value = await r.json();
  } catch {
    throw new ApiError(null, "opencode-route-preview response invalid");
  }
  if (value === null || typeof value !== "object")
    throw new ApiError(null, "opencode-route-preview response invalid");
  const result = value as Partial<OpenCodeCandidatePreview>;
  const candidate = (result as { candidate?: unknown }).candidate as
    Partial<OpenCodeCandidatePreview["candidate"]> | undefined;
  if (
    result.sessionId !== sessionId ||
    !candidate ||
    candidate.sessionId !== sessionId ||
    typeof candidate.directory !== "string" ||
    !candidate.directory.trim() ||
    typeof candidate.serviceId !== "string" ||
    !candidate.serviceId.trim() ||
    typeof candidate.pid !== "number" ||
    !Number.isSafeInteger(candidate.pid) ||
    candidate.pid <= 0 ||
    typeof candidate.startedAtMs !== "number" ||
    !Number.isSafeInteger(candidate.startedAtMs) ||
    typeof candidate.bindingToken !== "string" ||
    !candidate.bindingToken.trim() ||
    typeof result.expiresAtMs !== "number" ||
    !Number.isSafeInteger(result.expiresAtMs) ||
    typeof result.previewToken !== "string" ||
    !result.previewToken.trim()
  ) {
    throw new ApiError(null, "opencode-route-preview response invalid");
  }
  const previousRoute = (result as { previousRoute?: unknown }).previousRoute;
  if (previousRoute !== undefined && previousRoute !== null) {
    const route = previousRoute as { routeVersion?: unknown; bindingToken?: unknown };
    if (
      typeof route.routeVersion !== "number" ||
      !Number.isSafeInteger(route.routeVersion) ||
      route.routeVersion < 1 ||
      (route.bindingToken !== null && typeof route.bindingToken !== "string")
    ) {
      throw new ApiError(null, "opencode-route-preview route version invalid");
    }
  }
  return result as OpenCodeCandidatePreview;
}

/** 只读路由核对（丢回执恢复用）：零认领零安装零写入，仅读当前路由行摘要。
 *  与 view 的认领入口不同——本核对绝不改变路由，可安全用于「确认 POST 结果未知」。
 *  bindingToken 与本次候选一致 + routeState=active ⇒ 本次确认已生效。 */
export async function checkOpenCodeRoute(
  sessionId: string,
  bindingToken: string,
  signal?: AbortSignal
): Promise<{
  sessionId: string;
  routeState: string;
  bindingToken?: string;
  routeVersion?: number;
}> {
  let r: Response;
  try {
    r = await fetch(`/m/api/v1/opencode/route/check?sessionId=${encodeURIComponent(sessionId)}`, {
      headers: { "x-opencode-expected-binding": bindingToken },
      signal,
    });
  } catch (e) {
    if (signal?.aborted || isAbortError(e)) throw e;
    throw new ApiError(
      null,
      `MOBILE_NETWORK_FAIL:opencode-route-check: opencode-route-check 网络异常: ${String(e)}`
    );
  }
  if (!r.ok) {
    throw new ApiError(r.status, `opencode-route-check ${r.status}`, await readErrorBody(r));
  }
  let value: unknown;
  try {
    value = await r.json();
  } catch {
    throw new ApiError(null, "opencode-route-check response invalid");
  }
  if (value === null || typeof value !== "object")
    throw new ApiError(null, "opencode-route-check response invalid");
  const result = value as {
    sessionId?: unknown;
    routeState?: unknown;
    bindingToken?: unknown;
    routeVersion?: unknown;
  };
  if (
    result.sessionId !== sessionId ||
    typeof result.routeState !== "string" ||
    !result.routeState
  ) {
    throw new ApiError(null, "opencode-route-check response invalid");
  }
  if (
    result.routeState !== "none" &&
    (typeof result.bindingToken !== "string" ||
      !result.bindingToken ||
      typeof result.routeVersion !== "number" ||
      !Number.isSafeInteger(result.routeVersion) ||
      result.routeVersion < 1)
  ) {
    throw new ApiError(null, "opencode-route-check response invalid");
  }
  return result as {
    sessionId: string;
    routeState: string;
    bindingToken?: string;
    routeVersion?: number;
  };
}

export type OpenCodeReselectReceipt =
  | { state: "in_progress" | "not_applied" }
  | {
      state: "committed";
      current: boolean;
      routeVersion: number;
      bindingToken: string;
      window: { pid: number; startedAtMs: number; boot: string | null };
    };

function parseReselectReceipt(
  value: Record<string, unknown> | null,
  sessionId: string,
  requestId: string
): OpenCodeReselectReceipt | null {
  if (value?.sessionId !== sessionId || value.requestId !== requestId) return null;
  if (value.state === "in_progress" || value.state === "not_applied") return { state: value.state };
  const window = value.window as { pid?: unknown; startedAtMs?: unknown; boot?: unknown } | null;
  if (
    value.state !== "committed" ||
    typeof value.current !== "boolean" ||
    typeof value.routeVersion !== "number" ||
    !Number.isSafeInteger(value.routeVersion) ||
    value.routeVersion < 1 ||
    typeof value.bindingToken !== "string" ||
    !value.bindingToken ||
    !window ||
    typeof window.pid !== "number" ||
    !Number.isSafeInteger(window.pid) ||
    window.pid < 1 ||
    typeof window.startedAtMs !== "number" ||
    !Number.isSafeInteger(window.startedAtMs) ||
    window.startedAtMs < 1 ||
    (window.boot !== null && typeof window.boot !== "string")
  )
    return null;
  return {
    state: "committed",
    current: value.current,
    routeVersion: value.routeVersion,
    bindingToken: value.bindingToken,
    window: { pid: window.pid, startedAtMs: window.startedAtMs, boot: window.boot },
  };
}

export async function readOpenCodeReselectReceipt(
  sessionId: string,
  requestId: string
): Promise<OpenCodeReselectReceipt | null> {
  const query = new URLSearchParams({ sessionId, requestId });
  const response = await fetch(`/m/api/v1/opencode/route/native/status?${query}`, {
    cache: "no-store",
    signal: AbortSignal.timeout(5000),
  });
  if (!response.ok) return null;
  return parseReselectReceipt(await readErrorBody(response), sessionId, requestId);
}

/** 路由类 POST 的回执校验失败：「请求可能已生效但结果未确认」的稳定标记。
 *  调用方（面板/输入区）据此渲染未确认文案并保留旧视图，不得盲目重试。 */
export const OC_RESPONSE_INVALID_MARKER = "OC_RESPONSE_INVALID";

/** 显式重新进入原生交互（`POST /opencode/route/native`）：
 *  被拒（旧队列仍有未决行 / 开关已关）→ 抛 `ApiError`（409 `OC_ROUTE_CHANGED` /
 *  `OC_ROUTE_NOT_OWNED`），零变更；已 `active` 时幂等成功。
 *
 *  `candidateToken`：候选预览（{@link previewOpenCodeCandidate}）签发的绑定令牌——确认时
 *  服务端把登记对象固定为预览目标，候选变化（登记改指别的服务/目录/代际）即拒绝零变更。
 *
 *  回执校验失败按「结果未确认」抛出（status=null、data.reasonCode=OC_RESPONSE_INVALID）：
 *  POST 可能已生效，调用方必须保留旧视图并提示核对，不得盲目重试。 */
export async function enterOpenCodeNative(
  sessionId: string,
  bindingToken: string | null = null,
  routeVersion: number | null = null,
  candidateToken: string | null = null,
  signal?: AbortSignal,
  timeoutMs: number = OPENCODE_ACTION_POST_TIMEOUT_MS,
  requestId?: string
): Promise<{
  sessionId: string;
  route: "opencode_native";
  bindingToken: string;
  routeVersion: number;
}> {
  if (signal?.aborted) throw new DOMException("aborted", "AbortError");
  // 调用方中止与客户端超时合成一个 signal：外部中止按 AbortError 原样抛，
  // 超时按「结果未确认」标记抛（与网络失败同一未知态，绝不宣称零变更）。
  const controller = new AbortController();
  const onExternalAbort = () => controller.abort();
  signal?.addEventListener("abort", onExternalAbort);
  const timer = setTimeout(() => controller.abort(), timeoutMs);
  let r: Response;
  try {
    r = await fetch("/m/api/v1/opencode/route/native", {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({
        sessionId,
        ...(bindingToken ? { bindingToken } : {}),
        ...(routeVersion !== null ? { routeVersion } : {}),
        ...(candidateToken ? { candidateToken } : {}),
        ...(requestId ? { requestId } : {}),
      }),
      signal: controller.signal,
    });
  } catch (e) {
    if (signal?.aborted) throw e; // 主动中止交调用方按「未读到」处理
    if (isAbortError(e)) throw postTimeoutError("opencode-route-native");
    // 请求可能已到达服务端：网络失败按「结果未确认」处理，不得宣称零变更。
    throw new ApiError(
      null,
      `MOBILE_NETWORK_FAIL:opencode-route-native: opencode-route-native 网络异常: ${String(e)}`,
      { reasonCode: OC_RESPONSE_INVALID_MARKER }
    );
  } finally {
    clearTimeout(timer);
    signal?.removeEventListener("abort", onExternalAbort);
  }
  if (!r.ok) {
    throw new ApiError(r.status, `opencode-route-native ${r.status}`, await readErrorBody(r));
  }
  let value: unknown;
  try {
    value = await r.json();
  } catch {
    throw new ApiError(null, "opencode-route-native response invalid", {
      reasonCode: OC_RESPONSE_INVALID_MARKER,
    });
  }
  if (value === null || typeof value !== "object")
    throw new ApiError(null, "opencode-route-native response invalid", {
      reasonCode: OC_RESPONSE_INVALID_MARKER,
    });
  if (requestId && !parseReselectReceipt(value as Record<string, unknown>, sessionId, requestId)) {
    throw new ApiError(null, "opencode-route-native receipt invalid", {
      reasonCode: OC_RESPONSE_INVALID_MARKER,
    });
  }
  const result = value as {
    sessionId?: unknown;
    route?: unknown;
    bindingToken?: unknown;
    routeVersion?: unknown;
  };
  if (
    result.sessionId !== sessionId ||
    result.route !== "opencode_native" ||
    typeof result.bindingToken !== "string" ||
    !result.bindingToken.trim() ||
    typeof result.routeVersion !== "number" ||
    !Number.isSafeInteger(result.routeVersion) ||
    result.routeVersion < 1
  ) {
    throw new ApiError(null, "opencode-route-native response invalid", {
      reasonCode: OC_RESPONSE_INVALID_MARKER,
    });
  }
  return {
    sessionId,
    route: "opencode_native",
    bindingToken: result.bindingToken,
    routeVersion: result.routeVersion,
  };
}

/** 只读动作视图（别人的动作与不存在的动作同码 404，不做存在性预言机）。
 *
 *  生产调用方：面板的「未确认动作」恢复区在挂载/刷新时按锚逐条读取当前状态，
 *  只有结论性状态才剪除锚。404 只表示当前没查到（首次派发可能仍在 reserve 之前），
 *  锚与行一律保留。 */
export async function readOpenCodeAction(actionId: string): Promise<ActionView> {
  let r: Response;
  try {
    r = await fetch(`/m/api/v1/opencode/actions/${encodeURIComponent(actionId)}`);
  } catch (e) {
    throw new ApiError(
      null,
      `MOBILE_NETWORK_FAIL:opencode-action-read: opencode-action-read 网络异常: ${String(e)}`
    );
  }
  if (!r.ok) {
    throw new ApiError(r.status, `opencode-action-read ${r.status}`, await readErrorBody(r));
  }
  return (await r.json()) as ActionView;
}

/** J6（原生新建）：opencode 会话原生链创建——目录核对 + serve 代理创建 +
 *  resume 起窗（服务端一次性完成）；返回原生 sessionId 与规范目录。 */
export interface OpenCodeCreateReceipt {
  requestId: string;
  sessionId: string | null;
  directory: string;
  creation: "unconfirmed" | "created";
  window: "pending" | "created" | "opening" | "opened" | "failed" | "blocked";
  reasonCode: string | null;
  reason: string | null;
  firstSentenceStatus: string | null;
}

export function newOpenCodeCreateRequestId(): string {
  const bytes = globalThis.crypto.getRandomValues(new Uint8Array(16));
  return Array.from(bytes, (byte) => byte.toString(16).padStart(2, "0")).join("");
}

const OPENCODE_CREATE_PATH = "/m/api/v1/opencode/create-native";

async function readCreateReceipt(
  response: Response,
  requestId: string
): Promise<OpenCodeCreateReceipt> {
  const body = await readErrorBody(response);
  if (
    body?.requestId === requestId &&
    typeof body.directory === "string" &&
    (body.creation === "unconfirmed" || body.creation === "created") &&
    (typeof body.sessionId === "string" || body.sessionId === null)
  ) {
    return body as unknown as OpenCodeCreateReceipt;
  }
  throw new ApiError(response.status, `opencode-create-native ${response.status}`, body);
}

export async function createOpenCodeNativeSession(
  requestId: string,
  directory: string,
  firstSentence: string
): Promise<OpenCodeCreateReceipt> {
  const response = await fetch(OPENCODE_CREATE_PATH, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ requestId, directory, firstSentence }),
  });
  return readCreateReceipt(response, requestId);
}

export async function readOpenCodeCreateReceipt(
  requestId: string
): Promise<OpenCodeCreateReceipt | null> {
  const response = await fetch(`${OPENCODE_CREATE_PATH}?${new URLSearchParams({ requestId })}`, {
    cache: "no-store",
  });
  if (response.status === 404) return null;
  return readCreateReceipt(response, requestId);
}

export interface OpenCodeCreatePreview {
  previewToken: string;
  sessionId: string;
  directory: string;
  serviceId: string;
  servicePid: number;
}

export async function previewOpenCodeCreateRecovery(
  requestId: string
): Promise<OpenCodeCreatePreview> {
  const response = await fetch(
    `${OPENCODE_CREATE_PATH}/preview?${new URLSearchParams({ requestId })}`,
    { cache: "no-store" }
  );
  const body = await readErrorBody(response);
  if (
    !response.ok ||
    typeof body?.previewToken !== "string" ||
    typeof body.sessionId !== "string" ||
    typeof body.directory !== "string" ||
    typeof body.serviceId !== "string" ||
    typeof body.servicePid !== "number"
  ) {
    throw new ApiError(response.status, `opencode-create-preview ${response.status}`, body);
  }
  return body as unknown as OpenCodeCreatePreview;
}

export async function recoverOpenCodeCreatedSession(
  requestId: string,
  candidateToken?: string,
  verifyOnly = false
): Promise<OpenCodeCreateReceipt> {
  const response = await fetch(`${OPENCODE_CREATE_PATH}/recover`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ requestId, candidateToken, verifyOnly }),
  });
  return readCreateReceipt(response, requestId);
}

/** J2 恢复清单条目（服务端 `OpenCodeActionListEntry` camelCase 同形）：独立于单读
 *  ActionView 契约——`operation` 为移动端 kind 映射必需字段（用户复审 JB-1）；
 *  服务端只回待核对三态（queued/acknowledged/unconfirmed，JB-2）。 */
export interface OpenCodeActionListEntry {
  actionId: string;
  state: string;
  operation: string;
  reasonCode?: string;
  nativeId?: string;
  runtimeDead?: boolean;
}

/** 按设备读取待核对清单。成功空数组是正式结果，传输和形状错误保留给调用方。 */
export async function fetchOpenCodeSessionActions(
  sessionId: string
): Promise<OpenCodeActionListEntry[]> {
  const r = await fetch(`/m/api/v1/opencode/actions?sessionId=${encodeURIComponent(sessionId)}`);
  if (!r.ok) throw new ApiError(r.status, `opencode-action-list ${r.status}`);
  const raw: unknown = await r.json();
  if (
    !Array.isArray(raw) ||
    !raw.every(
      (item): item is OpenCodeActionListEntry =>
        typeof item === "object" &&
        item !== null &&
        typeof item.actionId === "string" &&
        typeof item.state === "string" &&
        typeof item.operation === "string" &&
        (item.runtimeDead === undefined || typeof item.runtimeDead === "boolean")
    )
  ) {
    throw new ApiError(null, "opencode-action-list response invalid");
  }
  return raw as OpenCodeActionListEntry[];
}

/** 是否为「主动中止」（超时/离页）：调用方必须把它当作「没读到」，绝不当动作结论。 */
export function isAbortError(e: unknown): boolean {
  return (
    (e instanceof DOMException && e.name === "AbortError") ||
    (e instanceof Error && e.name === "AbortError")
  );
}

/** **只读核对**（不是执行）：按权威回读收敛结果不明的动作，绝不重发。
 *  开关关闭时同样可用（既有动作仍可核对）。
 *
 *  `signal`：可选中止（15s 超时见 {@link verifyOpenCodeActionWithTimeout}）。中止按
 *  AbortError 原样抛出——它只说明这次**没读到**，既不是 `not_sent` 也不是任何动作结论。 */
export async function verifyOpenCodeAction(
  actionId: string,
  signal?: AbortSignal
): Promise<ActionView> {
  if (signal?.aborted) throw new DOMException("aborted", "AbortError");
  let r: Response;
  try {
    r = await fetch(`/m/api/v1/opencode/actions/${encodeURIComponent(actionId)}/verify`, {
      method: "POST",
      signal,
    });
  } catch (e) {
    if (signal?.aborted || isAbortError(e)) throw e; // 主动中止：交调用方按「未读到」处理
    throw new ApiError(
      null,
      `MOBILE_NETWORK_FAIL:opencode-action-verify: opencode-action-verify 网络异常: ${String(e)}`
    );
  }
  if (!r.ok) {
    throw new ApiError(r.status, `opencode-action-verify ${r.status}`, await readErrorBody(r));
  }
  return (await r.json()) as ActionView;
}

/** 核对请求的统一超时（G8）：15s 内没有结论就中止并让按钮恢复可点。 */
export const OPENCODE_VERIFY_TIMEOUT_MS = 15_000;

/** {@link verifyOpenCodeAction} 的 15s 超时包装：超时 = 中止本次**读**，
 *  绝不等价于「没发出」——调用方收到 AbortError 后必须保持「未确认」并允许重试。 */
export async function verifyOpenCodeActionWithTimeout(
  actionId: string,
  timeoutMs: number = OPENCODE_VERIFY_TIMEOUT_MS
): Promise<ActionView> {
  const controller = new AbortController();
  const timer = setTimeout(() => controller.abort(), timeoutMs);
  try {
    return await verifyOpenCodeAction(actionId, controller.signal);
  } finally {
    clearTimeout(timer);
  }
}

/** **显式确认终态未知**（patch3 D2，H13 证据 §10-D2）：死代际 `acknowledged` /
 *  `unconfirmed` 行的用户可达出口。成功 → 账本转入 `unknown_confirmed` 归档态
 *  （判定保持未知，不冒充定案；审计 = 属主设备 + 确认时间 + 审计词），未决屏障解除、
 *  待答占用释放、绝不再派发——之后可显式重新选择目标。
 *
 *  拒绝面（保持 fail-closed）：非属主/不存在 → 404；行代际仍可核对（当前活代际）→
 *  409 `OC_ACTION_STATE`（零改写，先用只读核对收敛）。带写 POST 同款客户端超时（D1）。 */
export async function confirmOpenCodeActionUnknown(
  actionId: string,
  timeoutMs: number = OPENCODE_ACTION_POST_TIMEOUT_MS
): Promise<ActionView> {
  const controller = new AbortController();
  const timer = setTimeout(() => controller.abort(), timeoutMs);
  let r: Response;
  try {
    r = await fetch(`/m/api/v1/opencode/actions/${encodeURIComponent(actionId)}/confirm-unknown`, {
      method: "POST",
      signal: controller.signal,
    });
  } catch (e) {
    if (isAbortError(e)) throw postTimeoutError("opencode-action-confirm-unknown");
    throw new ApiError(
      null,
      `MOBILE_NETWORK_FAIL:opencode-action-confirm-unknown: opencode-action-confirm-unknown 网络异常: ${String(e)}`
    );
  } finally {
    clearTimeout(timer);
  }
  if (!r.ok) {
    throw new ApiError(
      r.status,
      `opencode-action-confirm-unknown ${r.status}`,
      await readErrorBody(r)
    );
  }
  return (await r.json()) as ActionView;
}

/** 交回旧路径的回执。`abandoned` = 本次**作废**的「代际已不可核对」动作条数
 *  （非 force 恒 0；> 0 必须如实告知用户「可能导致重复发送」）。 */
export interface RouteLegacyResult {
  sessionId: string;
  route: "legacy";
  bindingToken: string;
  routeVersion: number;
  abandoned: number;
}

/** 交回旧路径被拒时，拒绝体里「可被 force 作废」的动作条数（缺省 = 0 / 未提供）。
 *
 *  只有这个数 > 0 才代表「显式强制交回」有意义：这些行属于**换代前**的服务代际，
 *  本端再也无法用权威回读证明它们的结果。 */
export function unverifiableActionsOf(e: ApiError): number {
  const raw = e.data?.unverifiableActions;
  return typeof raw === "number" && Number.isFinite(raw) && raw > 0 ? Math.trunc(raw) : 0;
}

/** 显式交回旧路径（`POST /opencode/route/legacy`）：幂等；仍有结果未明的原生动作时
 *  409（`OC_ROUTE_CHANGED`，零变更；体里带 `unverifiableActions`）。
 *
 *  `force`（评审 Blocker 2）= **用户显式确认**的作废：后端先把「代际已不可核对」的
 *  动作写成 `not_sent` 墓碑（碑文 `unverifiable_generation`，占用同事务释放、永不重派），
 *  再交回旧路径。**只能由二次确认触发**，任何自动重试都不得带它。 */
export async function returnOpenCodeRouteToLegacy(
  sessionId: string,
  bindingToken: string,
  routeVersion: number,
  force = false,
  confirmedActionIds?: string[],
  timeoutMs: number = OPENCODE_ACTION_POST_TIMEOUT_MS
): Promise<RouteLegacyResult> {
  const controller = new AbortController();
  const timer = setTimeout(() => controller.abort(), timeoutMs);
  let r: Response;
  try {
    r = await fetch("/m/api/v1/opencode/route/legacy", {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify(
        force
          ? { sessionId, bindingToken, routeVersion, force: true, confirmedActionIds }
          : { sessionId, bindingToken, routeVersion }
      ),
      signal: controller.signal,
    });
  } catch (e) {
    if (isAbortError(e)) throw postTimeoutError("opencode-route-legacy");
    // 请求可能已到达服务端：网络失败按「结果未确认」处理，不得宣称零变更。
    throw new ApiError(
      null,
      `MOBILE_NETWORK_FAIL:opencode-route-legacy: opencode-route-legacy 网络异常: ${String(e)}`,
      { reasonCode: OC_RESPONSE_INVALID_MARKER }
    );
  } finally {
    clearTimeout(timer);
  }
  if (!r.ok) {
    throw new ApiError(r.status, `opencode-route-legacy ${r.status}`, await readErrorBody(r));
  }
  let body: unknown;
  try {
    body = await r.json();
  } catch {
    throw new ApiError(null, "opencode-route-legacy response invalid", {
      reasonCode: OC_RESPONSE_INVALID_MARKER,
    });
  }
  if (body === null || typeof body !== "object")
    throw new ApiError(null, "opencode-route-legacy response invalid", {
      reasonCode: OC_RESPONSE_INVALID_MARKER,
    });
  const value = body as Partial<RouteLegacyResult>;
  if (
    value.sessionId !== sessionId ||
    value.route !== "legacy" ||
    value.bindingToken !== bindingToken ||
    value.routeVersion !== routeVersion + 1 ||
    !Number.isSafeInteger(value.abandoned) ||
    (value.abandoned ?? -1) < 0 ||
    (!force && value.abandoned !== 0) ||
    (force && (!confirmedActionIds || value.abandoned !== new Set(confirmedActionIds).size))
  ) {
    throw new ApiError(null, "opencode-route-legacy response invalid", {
      reasonCode: OC_RESPONSE_INVALID_MARKER,
    });
  }
  return value as RouteLegacyResult;
}

// ==== 受控码 → 用户可读文案（一码一文案，单点映射；调用方勿另写一套）====

/** 原生动作的**结论性**状态判定：true = 结果已定，不再需要核对。
 *
 *  `not_sent` / `not_accepted` / `confirmed` / `settled` 都证明「没有待核对的东西」；
 *  `unknown_confirmed`（patch3 D2）= 用户已显式确认「终态未知、不再等待」——不再需要
 *  核对入口，但**它不是「没发出」或任何定案**；`dispatching` / `acknowledged` /
 *  `unconfirmed` 一律按**未确认**处理（`dispatching` 是过程态，客户端看到它只说明
 *  后端正在派发，不能当成「自己还在发」）。 */
export function isConclusiveState(state: ActionState): boolean {
  return (
    state === "confirmed" ||
    state === "settled" ||
    state === "not_sent" ||
    state === "not_accepted" ||
    state === "unknown_confirmed"
  );
}

// ==== 拒绝码分类：只有「派发前拒绝」才能判定「确定没写出」====
//
// 判据来自后端两个文件（不是猜的）：
// - `src-tauri/src/services/opencode_native/types.rs::codes`（33 个受控码的全集）；
// - `src-tauri/src/remote/opencode_native.rs::native_error_response`（码 → HTTP 状态的唯一映射）。
//
// 关键事实：**409 不等于「没写出」**。该函数把 `OC_TRANSPORT_*` / `OC_SERVICE_*` /
// `OC_RESPONSE_*` / `OC_UNAUTHORIZED` / `OC_HTTP_STATUS` 等传输与服务类码一起落进
// `_ => CONFLICT` 臂（注释原文：「要么零写出，要么结果未知」），服务端账本把它们记作
// `Unconfirmed`。因此客户端只能对**派发前拒绝**清锚；其余一律按未知结局锁写（fail closed）。
//
// 集合一的依据（后端在收到原生写之前就拒绝，或读投影阶段即失败 → 零派发）：
// - 400 臂（客户端形状/内容不符）：OC_ACTION_INVALID / OC_COMMAND_UNSUPPORTED /
//   OC_SEND_EMPTY / OC_SEND_TOO_LONG / OC_ATTACHMENT_REFUSED / OC_ANSWER_INCOMPLETE /
//   OC_ANSWER_INVALID / OC_ANSWER_UNSUPPORTED / OC_FORM_INVALID；
// - 派发前复核（abandon 理由，零网络写）：OC_BINDING_STALE / OC_ROUTE_NOT_OWNED /
//   OC_ACTION_CONFLICT（同 actionId 异载荷或待答项已被占用）；
// - 路由/平台门（旧入口或本端根本无权写）：OC_ROUTE_CHANGED / OC_PLATFORM_UNVERIFIED；
// - 同实例归属门（B1）：`NativeHub::execute` 在 **reserve 之前**就调用
//   `require_instance_ownership`，因此该码从 POST `/opencode/actions` 返回时既没有原生
//   写、也没有建立动作行（升级前已有 active 归属的会话同样如此）。队列/发送/答题拿到它
//   只需清掉本次锚，不能把这条动作当成「结果未知」永久锁住。
//   注意：该结论依赖后端「归属拒绝先于 reserve」的次序；若将来把归属证明改成运行时可变
//   状态，必须重新审查这个分类，而不是直接沿用。
//
// 集合二（**未知结局**，必须保持锁）：OC_TRANSPORT_FAILED / OC_TRANSPORT_TIMEOUT /
// OC_UNAUTHORIZED / OC_RESPONSE_INVALID / OC_RESPONSE_TOO_LARGE / OC_HTTP_STATUS /
// OC_SERVICE_NOT_REGISTERED / OC_SERVICE_REGISTRATION_INVALID / OC_CONTRACT_MISSING /
// OC_CONTRACT_ID_UNSUPPORTED / OC_SESSION_MISMATCH / OC_PATH_INVALID / OC_SCOPE_MISSING /
// OC_ENDPOINT_* / OC_VERSION_MISMATCH / OC_GENERATION_MISMATCH /
// OC_DIRECTORY_* / OC_FORM_UNSUPPORTED / OC_REQUEST_SETTLED / OC_ACTION_STATE /
// OC_STORE_FAILED / OC_ACTION_NOT_FOUND，以及**任何未收录的码**。
// （OC_RUNTIME_UNVERIFIED 原在本集合；R2 门证实它在 reserve 前拒绝、已移入集合一，
// 见 DEFINITE_REFUSAL_CODES 内注释。）
//
// 为什么连 `OC_REQUEST_SETTLED`（后端注释明说「派发前已被定案」）也放在未知集合：
// 它的语义是「在派发前被定案」，但客户端拿到的只是一个码——多锁一次可由核对（→ not_sent）
// 直接收敛，误判一次就是双写。方向只取保守。

/** 派发前受控拒绝（**确定零写出**）：只有这些码允许清锚、恢复编辑。 */
export const DEFINITE_REFUSAL_CODES: ReadonlySet<string> = new Set([
  // 400 臂：客户端形状/内容不符（后端先校验后写）
  "OC_ACTION_INVALID",
  "OC_COMMAND_UNSUPPORTED",
  "OC_SEND_EMPTY",
  "OC_SEND_TOO_LONG",
  "OC_ATTACHMENT_REFUSED",
  "OC_ANSWER_INCOMPLETE",
  "OC_ANSWER_INVALID",
  "OC_ANSWER_UNSUPPORTED",
  "OC_FORM_INVALID",
  // 派发前复核（abandon 理由）
  "OC_ACTION_CONFLICT",
  "OC_BINDING_STALE",
  "OC_ROUTE_NOT_OWNED",
  // G1 换窗并发守卫：回声验证之后、取得派发权之前伴表窗口身份/路由版本漂移
  // （同服务显式重选换窗）⇒ 最终派发事务 CAS 零写拒绝；待发项原样保留（queued
  // 记原因 / not_sent 墓碑带理由），两种次序都证明「零写出、零新队列项」。
  "OC_ROUTE_WINDOW_MISMATCH",
  "OC_SESSION_MISMATCH",
  "OC_DIRECTORY_UNRESOLVED",
  "OC_DIRECTORY_MISMATCH",
  // 路由/平台门
  "OC_ROUTE_CHANGED",
  "OC_PLATFORM_UNVERIFIED",
  // 同实例归属门（B1）：后端在 reserve 之前拒绝，确定零写出、零动作行
  "OC_INSTANCE_OWNERSHIP_UNVERIFIED",
  "OC_INSTANCE_AMBIGUOUS",
  "OC_DESKTOP_BINDING_REQUIRED",
  // R2 服务身份门（I2）：execute 在 dispatch_snapshot / verify_target_identity 阶段
  // （**均先于 reserve**）按证据链拒绝 ⇒ 零动作行、零原生写。即使将来某路径在
  // reserve 之后复核出该码，后端 abandon() 也会先写 not_sent 墓碑（identity_unverified，
  // 结论性终态）再返回 —— 两种次序下都不是「结果未知」，不得留锚锁写。
  "OC_RUNTIME_UNVERIFIED",
  // echo-challenge 归属写门（H03）：预留前（reserve 之前，零动作行）与派发权前
  // （claim/入队之前，not_sent 墓碑）两处都会按 `OC_GATE_*` 拒绝
  // ——两种次序都证明「零写出、零新队列项」，按确定拒绝清锚；重试 = 新 intent。
  "OC_GATE_NO_ACK",
  "OC_GATE_LATE_ACK",
  "OC_GATE_PID_MISMATCH",
  "OC_GATE_NONCE_MISMATCH",
  "OC_GATE_TITLE_INVALID",
  "OC_GATE_PROBE_UNWRITABLE",
  "OC_GATE_UNREACHABLE",
  "OC_GATE_BUSY",
  "OC_GATE_PID_UNVERIFIED",
  "OC_GATE_PROBE_TARGET",
  // N2 语义证据准入（H02 结论速览 9 的版本绑定）：构建/平台/启动形态组合未登记时
  // 在探针开始前拒绝（三零：零探针写、零动作行、零队列项）——确定零写出，清锚。
  // 恢复 = 工程侧按升级复验流程（E1/E1c → 登记）登记该组合，不是用户可自行重试的态。
  "OC_GATE_SEMANTICS_UNPROVEN",
  // H5 统一端到端预算耗尽：**只在取得派发权之前产生**（claim 是授权写出的唯一入口，
  // 写只发生在 claim 之后）⇒ 预算拒绝 = 确定零写出证明（not_sent 墓碑碑文
  // budget_exhausted 可核对），清锚后换新 actionId 重试安全（停止幂等、发送零重投）。
  "OC_DISPATCH_BUDGET_EXHAUSTED",
]);

/** 该拒绝码是否**确定零写出**。未收录（含 null）= 结局未知 → 调用方必须保持锁。
 *  大小写不敏感：原生路由回 `OC_*`，旧入口 409 回 `oc_route_changed`。 */
export function isDefiniteRefusal(code: string | null): boolean {
  return code !== null && DEFINITE_REFUSAL_CODES.has(code.trim().toUpperCase());
}

/** 「归属未证」呈现族（H05）：既有 `OC_INSTANCE_*` 归属门 + `OC_DESKTOP_BINDING_REQUIRED`
 *  + echo 写门的全部 `OC_GATE_*`。这族码在**视图层**出现时（routeReasonCode / 能力表
 *  reasonCode）只允许一处阻断说明（会话级绑定门或发送阻断行，二者互斥），且必须经
 *  {@link nativeErrorKey} 走本地化文案——机器串绝不上屏。
 *  注意：写动作回执（POST 409）里的门码按「确定零写出」处理（清锚 + 专属短语），
 *  不进入本判定——那是回执槽的单处呈现，与视图层阻断无关。 */
export function isOwnershipRefusalCode(code: string | null | undefined): boolean {
  if (code === null || code === undefined || code.trim() === "") return false;
  const normalized = code.trim().toUpperCase();
  return (
    normalized === "OC_INSTANCE_OWNERSHIP_UNVERIFIED" ||
    normalized === "OC_INSTANCE_AMBIGUOUS" ||
    normalized === "OC_DESKTOP_BINDING_REQUIRED" ||
    normalized.startsWith("OC_GATE_")
  );
}

/** 绑定死亡族（G4 patch7，GC）：已接管详情错误区「重新选择当前目标」入口的判据。
 *  这些码作为**发送受控拒绝回执**（确定零写出）出现时，说明绑定指向的目标已不可写
 *  ——钉窗退出/观察过期（F5 实录 §6.2②）、TUI 未应答、窗口身份证据失效、绑定失效。
 *  「重新读取」重建不了绑定（绑定行仍指向死窗），恢复路径 = 显式重选（preview→
 *  route/native 版本 CAS 链，死亡代际由服务端豁免旧绑定比对）。
 *  与其余确定拒绝码的分工（各按其文案的下一步）：内容/形状类（SEND_EMPTY 等）重发
 *  即可；瞬态类（GATE_BUSY / GATE_UNREACHABLE）等待重试；OC_ROUTE_WINDOW_MISMATCH
 *  的新窗口已是当前绑定（重新发送）；OC_GATE_PROBE_TARGET 是探测会话守卫；
 *  OC_GATE_SEMANTICS_UNPROVEN 归工程侧登记——都不在本族。 */
export const BINDING_DEAD_RESELECT_CODES: ReadonlySet<string> = new Set([
  "OC_GATE_PID_UNVERIFIED",
  "OC_GATE_NO_ACK",
  "OC_GATE_LATE_ACK",
  "OC_GATE_PID_MISMATCH",
  "OC_GATE_NONCE_MISMATCH",
  "OC_GATE_TITLE_INVALID",
  "OC_GATE_PROBE_UNWRITABLE",
  "OC_BINDING_STALE",
]);

/** 受控码 → i18n 键（未收录的码返回 null：调用方必须回落后端的 `data.reason`，再退到
 *  通用句——**任何情况下都不许把合成出来的 `opencode-actions 409` 这类机器串上屏**）。
 *  这里只收「用户能据此做点什么」的常码；其余 33 个 `OC_*` 码走同一通用句。 */
export function nativeErrorKey(code: string | null): string | null {
  switch (code) {
    case "OC_TRANSPORT_TIMEOUT":
    case "OC_TRANSPORT_FAILED":
      return "mobile.openCodeNative.errors.transportFailed";
    case "OC_UNAUTHORIZED":
      return "mobile.openCodeNative.errors.unauthorized";
    case "OC_RESPONSE_INVALID":
      return "mobile.openCodeNative.errors.responseInvalid";
    case "OC_ACTION_INVALID":
      return "mobile.openCodeNative.errors.actionInvalid";
    case "OC_SERVICE_NOT_REGISTERED":
    case "OC_SERVICE_REGISTRATION_INVALID":
      return "mobile.openCodeNative.errors.serviceNotRegistered";
    case "OC_INSTANCE_OWNERSHIP_UNVERIFIED":
      return "mobile.openCodeNative.errors.instanceOwnershipUnverified";
    // echo-challenge 归属写门（H03 / H05 收口）：门码各有专属原因短语——同一阻断事实
    // 在 UI 上只允许一处解释，但不同门码的「下一步」不同（等终端 / 稍候 / 重选目标 /
    // 重新读取），因此逐一映射，绝不把机器串当文案上屏。
    case "OC_GATE_NO_ACK":
    case "OC_GATE_LATE_ACK":
      return "mobile.openCodeNative.errors.gateNoAck";
    case "OC_GATE_BUSY":
      return "mobile.openCodeNative.errors.gateBusy";
    case "OC_GATE_PROBE_TARGET":
      return "mobile.openCodeNative.errors.gateProbeTarget";
    case "OC_GATE_UNREACHABLE":
      return "mobile.openCodeNative.errors.gateUnreachable";
    case "OC_GATE_PID_UNVERIFIED":
      return "mobile.openCodeNative.errors.gatePidUnverified";
    case "OC_GATE_PID_MISMATCH":
    case "OC_GATE_NONCE_MISMATCH":
    case "OC_GATE_TITLE_INVALID":
    case "OC_GATE_PROBE_UNWRITABLE":
      return "mobile.openCodeNative.errors.gateEvidenceConflict";
    // N2 语义证据准入：该构建的归属行为证据未登记——恢复责任在工程侧（升级复验
    // 后登记），用户侧只读不受影响；专属短语与门码族分开（下一步不同）。
    case "OC_GATE_SEMANTICS_UNPROVEN":
      return "mobile.openCodeNative.errors.semanticsUnproven";
    case "OC_INSTANCE_AMBIGUOUS":
      return "mobile.openCodeNative.errors.instanceAmbiguous";
    case "OC_DESKTOP_BINDING_REQUIRED":
      return "mobile.openCodeNative.errors.desktopBindingRequired";
    case "OC_PLATFORM_UNVERIFIED":
      return "mobile.openCodeNative.errors.platformUnverified";
    case "OC_ROUTE_NOT_OWNED":
    case "oc_route_not_owned":
      return "mobile.openCodeNative.errors.routeNotOwned";
    case "OC_ROUTE_CHANGED":
    case "oc_route_changed":
      return "mobile.openCodeNative.errors.routeChanged";
    // G1 换窗并发守卫：目标窗口在归属验证后发生了变化（如刚做过显式重选）——
    // 本次零写出，待发项保留；下一步 = 重新发送（新窗口已是当前绑定）。
    case "OC_ROUTE_WINDOW_MISMATCH":
      return "mobile.openCodeNative.errors.routeWindowMismatch";
    case "OC_BINDING_STALE":
      return "mobile.openCodeNative.errors.bindingStale";
    case "OC_COMMAND_UNSUPPORTED":
    case "oc_command_unsupported":
      return "mobile.openCodeNative.errors.commandUnsupported";
    case "OC_SEND_EMPTY":
      return "mobile.openCodeNative.errors.sendEmpty";
    case "OC_SEND_TOO_LONG":
      return "mobile.openCodeNative.errors.sendTooLong";
    case "OC_ATTACHMENT_REFUSED":
      return "mobile.openCodeNative.errors.attachmentRefused";
    case "OC_ACTION_CONFLICT":
    case "oc_action_conflict":
      return "mobile.openCodeNative.errors.actionConflict";
    case "OC_ACTION_NOT_FOUND":
      return "mobile.openCodeNative.errors.actionNotFound";
    // H5 统一端到端预算耗尽：确定未发出（零写出），可安全重试——专属短语，
    // 不落通用句也不暗示「停止失败」终态。
    case "OC_DISPATCH_BUDGET_EXHAUSTED":
      return "mobile.openCodeNative.errors.dispatchBudgetExhausted";
    // 读面 drift / 写面门码共用一码一文案（patch3 O2，H13 证据 §10-O2）：读面 409 的
    // 上游英文 reason 原文不得上屏——接入 H05 本地化映射后单点生效（读面横幅与写面
    // 回执槽都经 nativeErrorKey）。
    case "OC_RUNTIME_UNVERIFIED":
      return "mobile.openCodeNative.errors.runtimeUnverified";
    case "OC_GENERATION_MISMATCH":
      return "mobile.openCodeNative.errors.generationMismatch";
    // H6（服务换代自助恢复）：只读恢复核对发现目标漂移（服务重启换登记 id / 目录
    // 变化）时的客户端合成码（useOpenCodeNative 恢复闸）。可操作文案指向显式重选
    // ——三段链第一段的码面：错误提示必须指向下一步动作，不落通用句。
    case "OC_TARGET_CHANGED":
      return "mobile.openCodeNative.errors.targetChanged";
    case "OC_REQUEST_SETTLED":
      return "mobile.openCodeNative.errors.requestSettled";
    case "OC_CONTRACT_MISSING":
    case "OC_CONTRACT_ID_UNSUPPORTED":
    case "OC_FORM_UNSUPPORTED":
    case "OC_ANSWER_UNSUPPORTED":
      return "mobile.openCodeNative.errors.capabilityMissing";
    case "not_opencode":
      return "mobile.openCodeNative.errors.notOpenCode";
    case "no_session":
      return "mobile.openCodeNative.errors.noSession";
    default:
      return null;
  }
}

/** 配对门内只读策略，移动端不提供写接口。 */
export async function fetchOpenCodePolicy(signal?: AbortSignal): Promise<{
  policy: "native_only" | "legacy_compatible";
  platform?: string;
  createRoute?: "opencode_native" | "legacy" | "unavailable";
}> {
  const response = await fetch("/m/api/v1/opencode/policy", { credentials: "same-origin", signal });
  if (!response.ok) throw new ApiError(response.status, "policy unavailable");
  return response.json();
}

export async function fetchFrozenOpenCodePendingCount(
  sessionId: string,
  signal: AbortSignal
): Promise<number> {
  const query = new URLSearchParams({ session_id: sessionId });
  const response = await fetch(`/m/api/v1/opencode/legacy-outbox?${query}`, {
    credentials: "same-origin",
    signal,
  });
  if (!response.ok) throw new ApiError(response.status, "legacy outbox unavailable");
  const body = await response.json();
  if (!Number.isSafeInteger(body?.count) || body.count < 0) {
    throw new Error("Invalid legacy outbox count");
  }
  return body.count;
}

export interface OutboxItem {
  actionId: string;
  owner: "tuvis" | "native";
  state: ActionState;
  textSummary: string;
  attachmentCount: number;
  reasonCode?: string | null;
}
export interface OutboxSnapshot {
  sessionId: string;
  revision: number;
  paused: boolean;
  items: OutboxItem[];
}

/** 本地快照；服务断链仍可读，GET 不修改队列修订。 */
export async function fetchNativeOutbox(
  sessionId: string,
  signal?: AbortSignal,
  expectedBinding?: string | null
): Promise<OutboxSnapshot> {
  const response = await fetch(
    `/m/api/v1/opencode/outbox?session_id=${encodeURIComponent(sessionId)}`,
    boundReadInit(signal, expectedBinding ?? undefined)
  );
  if (!response.ok)
    throw new ApiError(response.status, "opencode outbox", await readErrorBody(response));
  return (await response.json()) as OutboxSnapshot;
}
