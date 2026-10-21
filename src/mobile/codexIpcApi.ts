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

/** 能力四档（spec F8）：用户看到的是实际能力。 */
export type ControlAvailability = "controllable" | "send_only" | "compat_only" | "unavailable";

/** 待处理交互类别。未知 method → unsupported（actionable=false）。 */
export type PendingKind =
  "command_approval" | "permissions_approval" | "user_input" | "unsupported";

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
}

/** 队列视图。blockedReasonCode 非空（如 compat_outstanding）= 暂停派发，必须展示说明。
 *  blockedActionIds = 在册屏障的动作清单（人工解除端点的靶向依据；旧后端载荷缺省）。 */
export interface CodexQueueView {
  actions: CodexActionView[];
  blockedReasonCode: string | null;
  blockedActionIds?: string[];
}

/** 平台能力集。false = 该 build 尚未活体验证，不表示协议实现不存在。 */
export interface CapabilitySet {
  send: boolean;
  state: boolean;
  command: boolean;
  permissions: boolean;
  blockingInput: boolean;
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
}

/** 待处理请求视图。contextToken 是服务端随机 UUID（绑定 boot/epoch/generation/
 *  thread/request）；requestKey/token 任一变化 = 旧上下文作废，调用方必须清旧状态。 */
export interface PendingView {
  requestKey: string;
  contextToken: string;
  kind: PendingKind;
  actionable: boolean;
  disabledReasonCode: string | null;
  command: string | null;
  cwd: string | null;
  reason: string | null;
  permissionSummary: string | null;
  questions: QuestionView[];
}

/** 内容健康状态（2026-10-18 形态适配）：与写能力（availability）正交——
 *  能发送不代表历史已载入；读取出错（unsupported）不得呈现为真空会话。
 *  ready 且真实 0 条才显示「暂无消息」。 */
export type ContentState = "loading" | "ready" | "partial" | "unsupported";

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
export const CODEX_QUEUE_PATH = "/m/api/v1/codex/queue";
export const CODEX_QUEUE_RETRACT_PATH = "/m/api/v1/codex/queue/retract";
export const CODEX_QUEUE_FORCE_CONTINUE_PATH = "/m/api/v1/codex/queue/force-continue";

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
  onState: (s: ThreadProjection) => void,
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
   *  能收字节但解析不出完整投影不足以证明链路健康（不清零失败计数）。 */
  function dispatch(e: Event) {
    try {
      const parsed = asThreadProjection(JSON.parse((e as MessageEvent).data));
      failures = 0;
      onState(parsed);
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
    next.addEventListener("snapshot", dispatch);
    next.addEventListener("state", dispatch);
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
export type CodexPendingActionWord = "approve" | "reject" | "answer";

/** POST /codex/pending-response 请求体（**白名单契约**：只有这些键——后端
 *  deny_unknown_fields，owner/权限对象/raw method 一律 400）。 */
export interface PendingResponseBody {
  sessionId: string;
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
