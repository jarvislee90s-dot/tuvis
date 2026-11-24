// DSH Desktop 远控领域类型（T01；计划 §2.2「固定领域类型」的 TS 镜像）。
//
// 与 Rust `src-tauri/src/services/dsh_control/model.rs` 逐字段对应：
// 结构体字段 camelCase、枚举 wire 词 snake_case（UploadKind 为 lowercase）。
// 类型名统一 `Dsh` 前缀，避免与 Codex 既有 DTO（ActionState 等）撞名。
//
// 本机专有类型**不进本镜像**（§2.2 纪律）：LocalHost（路径/PID 仅本机）、
// ClientIdentity、AuthorizedPeer（秘密句柄）、UploadInput（字节流）、
// DshEnvironment（注入环境）、NativeResponse/ContractEvidence（后端契约层）。
//
// 运行时守卫：`parseDshActionPayload` 是动作闭集（15 变体 + deny_unknown_fields
// 纪律）的前端单点；未知 type 一律拒绝——不接收任意 ns/method。

import type { SessionMessage } from "@/mobile/api";

// ============================================================
// 目标与请求身份
// ============================================================

/** 远控目标（Rust `Target`）。 */
export interface DshTarget {
  hostId: string;
  /** 连接代际：旧代际目标立即失效。 */
  epoch: number;
  sessionId: string;
}

/** 原生请求身份（Rust `RequestKey`；ID 以原生字符串保存，不由题干生成）。 */
export interface DshRequestKey extends DshTarget {
  clientId: string;
  eventId: string;
  agentId: string;
  revision: number;
}

// ============================================================
// 操作与能力
// ============================================================

/** 远控操作键（Rust `Operation` 28 词；serde snake_case 逐字对应）。 */
export type DshOperation =
  | "read"
  | "send"
  | "file_upload"
  | "image_upload"
  | "model_select"
  | "reasoning_select"
  | "approve_once"
  | "reject"
  | "single_answer"
  | "multi_answer"
  | "free_answer"
  | "group_answer"
  | "plan_review"
  | "cancel_current"
  | "queue_edit"
  | "queue_remove"
  | "steer"
  | "session_create"
  | "session_rename"
  | "session_fork"
  | "session_search"
  | "archive"
  | "unarchive"
  | "pin"
  | "unpin"
  | "skills_list"
  | "commands_list"
  | "command_execute";

/** 全部操作词（顺序 = Rust `Operation::ALL`；测试锁长度 28）。 */
export const DSH_OPERATIONS: readonly DshOperation[] = [
  "read",
  "send",
  "file_upload",
  "image_upload",
  "model_select",
  "reasoning_select",
  "approve_once",
  "reject",
  "single_answer",
  "multi_answer",
  "free_answer",
  "group_answer",
  "plan_review",
  "cancel_current",
  "queue_edit",
  "queue_remove",
  "steer",
  "session_create",
  "session_rename",
  "session_fork",
  "session_search",
  "archive",
  "unarchive",
  "pin",
  "unpin",
  "skills_list",
  "commands_list",
  "command_execute",
] as const;

/** 证据来源（Rust `EvidenceSource` 四档）。 */
export type DshEvidenceSource = "none" | "shipped" | "contract_matched" | "observed_success";

/** 单操作状态（Rust `OperationStatus`；后端唯一事实，前端不自行拼按钮）。 */
export interface DshOperationStatus {
  implemented: boolean;
  allowed: boolean;
  /**
   * 证据是否锚定当前目标（当前整包内容指纹一致 / 当前会话活体观测）。
   *
   * **不等于**「该 build 已真机实测」——后者由 `evidence === "shipped"` 表达；
   * 契约匹配档 = `verified` + `requiresAutoAdapt`（兼容可尝试，非实测）。
   */
  verified: boolean;
  /** 是否依赖「自动适配」活门（关闭即撤回）。 */
  requiresAutoAdapt: boolean;
  evidence: DshEvidenceSource;
  reasonCode: string | null;
}

/** 逐操作有效集合（Rust `EffectiveOperations`）。 */
export interface DshEffectiveOperations {
  revision: number;
  /** 未知操作不在表内 = 未知/不可用（Partial：后端可能比本端新）。 */
  operations: Partial<Record<DshOperation, DshOperationStatus>>;
}

// ============================================================
// 动作与回执
// ============================================================

/** 动作状态（Rust `ActionState` 9 档；Succeeded 必须有效果证据，受理不算成功）。 */
export type DshActionState =
  | "queued"
  | "reserved"
  | "accepted"
  | "running"
  | "succeeded"
  | "rejected"
  | "failed"
  | "unknown_delivery"
  | "cancelled";

/** 动作回执（Rust `ActionReceipt`）。 */
export interface DshActionReceipt {
  actionId: string;
  state: DshActionState;
  nativeId: string | null;
  reasonCode: string | null;
}

/** 单题作答（Rust `QuestionAnswer`；原生形状 {id, selected[]}）。 */
export interface DshQuestionAnswer {
  id: string;
  selected: string[];
}

/** `send`：普通文本/带附件发送。 */
export interface DshSendPayload {
  type: "send";
  text: string;
  /** 附件回执 id（`DshUploadReceipt.receiptId`，不是任意文件 ID）。 */
  receiptIds: string[];
  queueOnly: boolean;
  bypassPausedQueue: boolean;
}

/** `approval`：审批决策（decision = 原生 outcome 词，如 allowed-once/rejected）。 */
export interface DshApprovalPayload {
  type: "approval";
  key: DshRequestKey;
  decision: string;
}

/** `answer`：问答题作答（逐题以原问题 ID 为键）。 */
export interface DshAnswerPayload {
  type: "answer";
  key: DshRequestKey;
  answers: DshQuestionAnswer[];
}

/** `select_model`：模型/供应商/推理档切换。 */
export interface DshSelectModelPayload {
  type: "select_model";
  provider: string;
  model: string;
  reasoningEffort: string | null;
}

/** `cancel_current`：打断当前回合。 */
export interface DshCancelCurrentPayload {
  type: "cancel_current";
}

/** `queue_edit`：队列改写。 */
export interface DshQueueEditPayload {
  type: "queue_edit";
  itemId: string;
  text: string;
}

/** `queue_remove`：队列撤回。 */
export interface DshQueueRemovePayload {
  type: "queue_remove";
  itemId: string;
}

/** `steer`：插队转向。 */
export interface DshSteerPayload {
  type: "steer";
  itemId: string;
}

/** `rename`：会话重命名。 */
export interface DshRenamePayload {
  type: "rename";
  title: string;
}

/** `fork`：会话分支（atSeq 可缺省 = 整段分支）。 */
export interface DshForkPayload {
  type: "fork";
  atSeq: number | null;
}

/** `archive`：归档会话。 */
export interface DshArchivePayload {
  type: "archive";
}

/** `unarchive`：取消归档。 */
export interface DshUnarchivePayload {
  type: "unarchive";
}

/** `pin`：置顶会话。 */
export interface DshPinPayload {
  type: "pin";
}

/** `unpin`：取消置顶。 */
export interface DshUnpinPayload {
  type: "unpin";
}

/** `command`：slash 命令执行（args 为原生 JSON object）。 */
export interface DshCommandPayload {
  type: "command";
  name: string;
  args: Record<string, unknown>;
  receiptIds: string[];
}

/** 动作载荷闭集（Rust `ActionPayload` 15 变体；tag = `type`，snake_case）。 */
export type DshActionPayload =
  | DshSendPayload
  | DshApprovalPayload
  | DshAnswerPayload
  | DshSelectModelPayload
  | DshCancelCurrentPayload
  | DshQueueEditPayload
  | DshQueueRemovePayload
  | DshSteerPayload
  | DshRenamePayload
  | DshForkPayload
  | DshArchivePayload
  | DshUnarchivePayload
  | DshPinPayload
  | DshUnpinPayload
  | DshCommandPayload;

/** 动作请求体（Rust `ActionRequest`；`POST /dsh/action` 的 body）。 */
export interface DshActionRequest {
  target: DshTarget;
  actionId: string;
  expectedRevision: number | null;
  payload: DshActionPayload;
}

// ============================================================
// 会话快照与队列
// ============================================================

/** 连接状态（Rust `ConnectionState`）。 */
export type DshConnectionState = "connected" | "disconnected";

/** 待处理交互类别（Rust `PendingKind`；未知原生事件 → unsupported）。 */
export type DshPendingKind = "approval" | "questions" | "unsupported";

/** 问答选项（Rust `PendingQuestionOption`）。 */
export interface DshPendingQuestionOption {
  label: string;
  description: string;
}

/** 问答单题投影（Rust `PendingQuestion`；形状 = waterfall 夹具）。 */
export interface DshPendingQuestion {
  id: string;
  question: string;
  header: string | null;
  options: DshPendingQuestionOption[];
  multiSelect: boolean;
}

/** 待处理交互（Rust `PendingInteraction`）。 */
export interface DshPendingInteraction {
  key: DshRequestKey;
  kind: DshPendingKind;
  allowedDecisions: string[];
  question: string | null;
  questions: DshPendingQuestion[];
  summary: string | null;
  resolved: boolean;
}

/** 队列项状态（Rust `QueueItemState`：区分 Tuvis 未派发与 DSH 原生 pending）。 */
export type DshQueueItemState = "local_pending" | "native_pending";

/** 队列项（Rust `QueueItem`）。 */
export interface DshQueueItem {
  itemId: string;
  actionId: string | null;
  text: string;
  state: DshQueueItemState;
}

/** 会话快照（Rust `SessionSnapshot`；`GET /dsh/session` 响应）。 */
export interface DshSessionSnapshot {
  target: DshTarget;
  revision: number;
  connectionState: DshConnectionState;
  effectiveOperations: DshEffectiveOperations;
  /** 统一消息条目（remote::content::SessionMessage 的既有 TS 镜像）。 */
  messages: SessionMessage[];
  pending: DshPendingInteraction[];
  queue: DshQueueItem[];
  model: DshModelSelection | null;
}

/** 模型选择（Rust `ModelSelection`）。 */
export interface DshModelSelection {
  provider: string;
  model: string;
  reasoningEffort: string | null;
}

// ============================================================
// 目录
// ============================================================

/** 目录类别（Rust `CatalogKind`）。 */
export type DshCatalogKind = "models" | "skills" | "commands" | "sessions";

/** 推理档说明（Rust `ReasoningSpec`）。 */
export interface DshReasoningSpec {
  efforts: string[];
  defaultEffort: string | null;
}

/** 模型目录条目（Rust `CatalogModel`）。 */
export interface DshCatalogModel {
  provider: string | null;
  model: string;
  reasoning: DshReasoningSpec | null;
}

/** 模型分组（Rust `ModelGroup`）。 */
export interface DshModelGroup {
  id: string;
  models: DshCatalogModel[];
}

/** Models 目录（Rust `ModelCatalog`）。 */
export interface DshModelCatalog {
  default: DshModelSelection | null;
  routableProviders: string[];
  groups: DshModelGroup[];
}

/** Skills/Commands/Sessions 的允许字段投影（Rust `CatalogEntry`）。 */
export interface DshCatalogEntry {
  id: string;
  name: string;
  description: string | null;
}

/** 目录结果（Rust `CatalogResult`；tag = `kind`）。 */
export type DshCatalogResult =
  | { kind: "models"; catalog: DshModelCatalog }
  | { kind: "skills"; items: DshCatalogEntry[] }
  | { kind: "commands"; items: DshCatalogEntry[] }
  | { kind: "sessions"; items: DshCatalogEntry[] };

// ============================================================
// 新建会话 / 上传 / 效果证据 / 动作视图
// ============================================================

/** 新建会话请求（Rust `CreateRequest`；projectId 由 Host 已授权工作区映射）。 */
export interface DshCreateRequest {
  hostId: string;
  projectId: string;
  actionId: string;
  initialText: string | null;
}

/** 新建会话回执（Rust `CreateReceipt`）。 */
export interface DshCreateReceipt {
  actionId: string;
  target: DshTarget;
  state: DshActionState;
  agentPreset: string | null;
}

/** 附件类别（Rust `UploadKind`；wire 词 lowercase）。 */
export type DshUploadKind = "file" | "image";

/** 上传回执（Rust `UploadReceipt`；读取另用原生 attachment_id，不把 receipt 当文件 ID）。 */
export interface DshUploadReceipt {
  target: DshTarget;
  receiptId: string;
  name: string;
  bytes: number;
  kind: DshUploadKind;
}

/** 效果类别（Rust `EffectKind`）。 */
export type DshEffectKind = "message" | "decision" | "queue_item" | "session_change";

/** 效果证据（Rust `EffectEvidence`）。 */
export interface DshEffectEvidence {
  operation: DshOperation;
  nativeId: string | null;
  epoch: number;
  effectKind: DshEffectKind;
}

/** 动作视图（Rust `ActionView`；`GET /dsh/action` 响应）。 */
export interface DshActionView {
  receipt: DshActionReceipt;
  target: DshTarget;
  updatedAtMs: number;
  evidence: DshEffectEvidence | null;
}

// ============================================================
// 管理状态与绑定（本机设置面）
// ============================================================

/** 安装状态（Rust `InstallationState`）。 */
export type DshInstallationState = "missing" | "installed" | "conflict" | "broken";

/** 激活状态（Rust `ActivationState`；保存选择 ≠ 实际运行）。 */
export type DshActivationState = "disabled" | "enabled" | "restart_required" | "running";

/** 授权状态（Rust `AuthorizationState`）。 */
export type DshAuthorizationState = "unbound" | "bound" | "revoked";

/** 管理作业类别（Rust `ManagementKind`）。 */
export type DshManagementKind = "install" | "repair" | "uninstall";

/** 管理作业阶段（Rust `ManagementPhase`；无「CLI exit 0 即成功」捷径）。 */
export type DshManagementPhase =
  | "queued"
  | "waiting_for_desktop_exit"
  | "applying"
  | "verifying"
  | "succeeded"
  | "failed"
  | "cancelled";

/** 管理作业（Rust `ManagementJob`）。 */
export interface DshManagementJob {
  jobId: string;
  kind: DshManagementKind;
  phase: DshManagementPhase;
  reasonCode: string | null;
}

/** 本机管理状态（Rust `ManagementStatus`；远程无此 DTO）。
 *  `autoAdaptEnabled` 与 `bindingId` 是 T05/T06 命令层（commands/dsh.rs 的
 *  `DshRemoteStatusDto`）flatten 叠加字段，不属 §2.2 固定字段集——设置页开关
 *  初值（缺省开）与撤权按钮的真实绑定 id（无活跃绑定 = null）。 */
export interface DshManagementStatus {
  installation: DshInstallationState;
  activation: DshActivationState;
  authorization: DshAuthorizationState;
  operation: DshManagementJob | null;
  desktopVersion: string | null;
  pluginVersion: string | null;
  reasonCode: string | null;
  autoAdaptEnabled: boolean;
  bindingId: string | null;
  /** 实例作用域查询附带；旧单例状态无此字段（legacy 兼容）。 */
  instanceId?: string;
}

/** 本机候选实例（Rust `DshInstanceCandidateDto`）：候选≠已授权。 */
export interface DshInstanceCandidate {
  instanceId: string;
  kind: "desktop" | "web";
  displayName: string;
  profileDir: string;
  /** 后端保存的实例选择。 */
  selected?: boolean;
  /** 已核验活体的宿主代际；离线或未核验时为空。 */
  hostEpoch?: string | null;
  /**
   * 该实例当代是否有活体。
   *
   * `false` = 已登记但当前没在运行（用户按提示退出 DSH 之后就是这种状态）。
   * 这类条目**只**用于让用户重开，不代表可用：管理/绑定/写入在它上面都会
   * 如实失败。列表里保留它们是为了让「重开」入口不至于随退出一起消失。
   */
  live: boolean;
}

/** 按稳定实例 ID 的本机请求（Rust `DshInstanceRequest`，拒绝未知字段）。 */
export interface DshInstanceRequest {
  instanceId: string;
}

/** `dsh_instance_status` 返回体：管理状态 + 该实例身份。 */
export interface DshInstanceStatus extends DshManagementStatus {
  instanceId: string;
}

// ============================================================
// 「远程控制 DSH」单开关（Task 4；Rust `switch.rs` 镜像）
// ============================================================

/** 开关推进相位（Rust `SwitchPhase`；snake_case wire 词逐字对应）。 */
export type DshSwitchPhase =
  | "checking"
  | "installing"
  | "awaiting_reopen"
  | "verifying_plugin"
  | "authorizing"
  | "rebind_required"
  | "ready"
  | "repairing"
  | "waiting_uninstall"
  | "failed"
  | "off";

/** 失败步骤（Rust `SwitchStep`；重试入口指向的线性步）。 */
export type DshSwitchStep =
  | "instance_check"
  | "install"
  | "reopen"
  | "verify"
  | "authorize"
  | "repair"
  | "revoke"
  | "uninstall"
  | "home_evidence";

/** 通道就绪三态（Rust `ChannelReadiness`；发送/控制台各自独立呈现）。 */
export type DshChannelReadiness = "ready" | "recovering" | "not_authorized";

/** 开关状态外表（Rust `SwitchView`；`dsh_switch_get/enable/disable/repair` 返回体）。
 *  后端按选定实例持久化开关与推进状态；前端不落 localStorage、不自行编排。 */
export interface DshSwitchView {
  instanceId: string;
  /** 开关勾选态（用户意图；后端持久值）。 */
  enabled: boolean;
  phase: DshSwitchPhase;
  failedStep: DshSwitchStep | null;
  reasonCode: string | null;
  /** 活跃管理作业（安装/卸载/修复；等待退出等阶段由 job 面透出）。 */
  job: DshManagementJob | null;
  pluginStatus?: {
    installation: DshInstallationState;
    activation: DshActivationState;
    authorization: DshAuthorizationState;
    version: string | null;
    reasonCode: string | null;
  } | null;
  /** 发送通道就绪结果。 */
  send: DshChannelReadiness | null;
  /** 控制台通道就绪结果。 */
  console: DshChannelReadiness | null;
  /** 插件身份变化：等待用户经可信重绑面板确认。 */
  rebindRequired: boolean;
}

/** `dsh_switch_enable/disable/repair` 请求体（Rust `DshInstanceRequest`）。 */
export interface DshSwitchRequest {
  instanceId: string;
}

/** 绑定挑战（Rust `BindingChallenge`；displayCode 用于两端核对，非长效凭据）。 */
export interface DshBindingChallenge {
  bindingId: string;
  displayCode: string;
  expiresAtMs: number;
}

/** `dsh_binding_start` 返回体（Rust `DshBindingStartDto`）：Challenge flatten +
 *  命令层叠加的双端身份核对面（§2.5「首次本机 UI 显示双方身份」；均为非秘密）。 */
export interface DshBindingStartResult extends DshBindingChallenge {
  /** 待批插件端可执行文件名。 */
  clientExecutable: string | null;
  /** 待批插件端 OS PID。 */
  clientPid: number | null;
  /** 本端（兔维斯）公钥指纹（SHA-256 hex 前 16 位）。 */
  rustFingerprint: string;
  instanceId?: string | null;
  oldFingerprint?: string | null;
  newFingerprint?: string | null;
}

// ============================================================
// 错误与运行时守卫
// ============================================================

/** 结构化错误镜像（Rust `DshError {code, detail}`；展示文案经 errors.<code> locale）。 */
export interface DshError {
  code: string;
  detail: string;
}

/** 动作载荷字段规格（键 → 期望形态；`?` 后缀 = 可 null）。 */
type DshFieldKind =
  | "string"
  | "string?"
  | "boolean"
  | "number"
  | "number?"
  | "string[]"
  | "object"
  | "requestKey"
  | "answers";

/** 15 变体的字段白名单（与 Rust ActionPayload 逐字段对应；拒绝未知字段）。 */
const DSH_PAYLOAD_FIELDS: Record<string, Record<string, DshFieldKind>> = {
  send: {
    text: "string",
    receiptIds: "string[]",
    queueOnly: "boolean",
    bypassPausedQueue: "boolean",
  },
  approval: { key: "requestKey", decision: "string" },
  answer: { key: "requestKey", answers: "answers" },
  select_model: { provider: "string", model: "string", reasoningEffort: "string?" },
  cancel_current: {},
  queue_edit: { itemId: "string", text: "string" },
  queue_remove: { itemId: "string" },
  steer: { itemId: "string" },
  rename: { title: "string" },
  fork: { atSeq: "number?" },
  archive: {},
  unarchive: {},
  pin: {},
  unpin: {},
  command: { name: "string", args: "object", receiptIds: "string[]" },
};

const isString = (v: unknown): v is string => typeof v === "string";
const isBoolean = (v: unknown): v is boolean => typeof v === "boolean";
const isNumber = (v: unknown): v is number => typeof v === "number" && Number.isFinite(v);
const isStringArray = (v: unknown): v is string[] => Array.isArray(v) && v.every(isString);

/** 可 null 亦可缺省（= undefined）：Rust Option 字段缺键即 None。 */
const nullable = (check: (v: unknown) => boolean) => (v: unknown) =>
  v === null || v === undefined || check(v);

const FIELD_CHECKS: Record<DshFieldKind, (v: unknown) => boolean> = {
  string: isString,
  "string?": nullable(isString),
  boolean: isBoolean,
  number: isNumber,
  "number?": nullable(isNumber),
  "string[]": isStringArray,
  object: (v) => typeof v === "object" && v !== null && !Array.isArray(v),
  requestKey: isRequestKey,
  answers: (v) =>
    Array.isArray(v) &&
    v.every(
      (a) =>
        typeof a === "object" &&
        a !== null &&
        isString((a as Record<string, unknown>).id) &&
        isStringArray((a as Record<string, unknown>).selected)
    ),
};

function isRequestKey(v: unknown): boolean {
  if (typeof v !== "object" || v === null) return false;
  const k = v as Record<string, unknown>;
  return (
    isString(k.hostId) &&
    isNumber(k.epoch) &&
    isString(k.sessionId) &&
    isString(k.clientId) &&
    isString(k.eventId) &&
    isString(k.agentId) &&
    isNumber(k.revision)
  );
}

/**
 * 解析远程动作载荷（动作闭集前端单点）。未知 `type` / 非对象 / 已知变体的
 * 缺字段、未知字段一律抛错——不接收任意 ns/method（与 Rust `codec::parse_action`
 * 同一纪律；完整校验以后端为准，此处是镜像侧守卫）。
 */
export function parseDshActionPayload(raw: unknown): DshActionPayload {
  if (typeof raw !== "object" || raw === null || Array.isArray(raw)) {
    throw new Error("dsh action payload is not a JSON object");
  }
  const obj = raw as Record<string, unknown>;
  const tag = obj.type;
  if (typeof tag !== "string" || !(tag in DSH_PAYLOAD_FIELDS)) {
    throw new Error(`unknown dsh action type: ${String(tag)}`);
  }
  const spec = DSH_PAYLOAD_FIELDS[tag];
  // deny_unknown_fields 纪律：已知变体不得携带未声明字段。
  for (const key of Object.keys(obj)) {
    if (key !== "type" && !(key in spec)) {
      throw new Error(`unknown field \`${key}\` for dsh action \`${tag}\``);
    }
  }
  const out: Record<string, unknown> = { type: tag };
  for (const [field, kind] of Object.entries(spec)) {
    const value = obj[field];
    if (!FIELD_CHECKS[kind](value)) {
      throw new Error(`field \`${field}\` of dsh action \`${tag}\` has invalid shape`);
    }
    if (value !== undefined) out[field] = value;
  }
  // 双段断言：Record<string, unknown> 与判别联合无充分重叠，直转会被 tsc 拒
  // （2026-10-29 T05 顺带修复：T01 起阻塞 `pnpm build` 的既有编译错）。
  return out as unknown as DshActionPayload;
}
