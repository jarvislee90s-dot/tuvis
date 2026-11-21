// Codex 题卡跳过（受控原型，计划 Task3）：独立展示跳过账的远程 API 客户端 +
// 展示/历史事实（questionDisplayGroups / questionSkipHistory）的解析与跨帧收敛。
//
// **DTO 纪律**：本文件类型与 Rust `remote/codex_question_skip.rs` 的 serde 输出
// 逐字段对齐（camelCase；枚举值 snake_case）。回执是**安全边界**：actionId/group/
// decision/effect/revision 全部在客户端 runtime 复核，任何不匹配都按错误处理——
// 迟到的旧回执、伪造或漂移的载荷不得进入展示账（防「双向旧成功盖新 unknown」的
// 前端对应面）。
//
// **transport 纪律**：与 `mobile/api.ts` 同源 cookie 设备认证（默认 same-origin，
// 绝不手写 credentials 覆盖）；POST body 白名单序列化——请求对象原样 JSON，
// 后端 deny_unknown_fields，前端不夹带任何额外键。
import { ApiError } from "./api";

// ==== 类型（与 Rust serde 逐字段对齐，勿漂移）====

/** 组身份四元组（thread/来源 turn/item/内容代际）。展示账主键，全量相等才算同组。 */
export interface GroupIdentity4 {
  threadId: string;
  sourceTurnId: string;
  sourceItemId: string;
  contentGeneration: string;
}

/** 展示决策（serde snake_case）。 */
export type QuestionDisplayDecision = "active" | "skipped";

/** 原生效果（serde snake_case）。`closed_verified` 之外任何值都**不得**宣称已关闭。 */
export type QuestionNativeEffect =
  "closed_verified" | "not_present_verified" | "unresolved" | "unsupported";

/** 单组展示视图（服务端事实；revision 是单调收敛锚）。 */
export interface GroupDisplayView {
  group: GroupIdentity4;
  revision: number;
  decision: QuestionDisplayDecision;
  latestNativeEffect: QuestionNativeEffect | null;
  reasonCode: string | null;
}

/** Thread 投影附加字段：展示组视图（含来源完整性与来源题数——skip 资格判定用）。
 *  旧后端不下发 → undefined（不据此清账）；下发但形状脏 → 整帧按 invalid 处理。 */
export interface QuestionDisplayGroupView extends GroupDisplayView {
  sourceComplete: boolean;
  sourceQuestionCount: number;
}

/** 历史条目附带的题文快照（恢复入口的展示事实；可 null——恢复文案不得瞎猜）。 */
export interface QuestionSkipHistoryQuestion {
  questionKey: string;
  questionIndex: number;
  title: string;
  options: string[];
}

/** 历史视图（真实承接的跳过事件；restore 入口只来自这里，不来自 display 单独在场）。 */
export interface QuestionSkipHistoryView extends GroupDisplayView {
  question: QuestionSkipHistoryQuestion | null;
}

// ==== 请求 / 回执 DTO ====

export interface PreviewRequest {
  sessionId: string;
  group: GroupIdentity4;
}

/** 三项确认是**字面量类型**：客户端只发 true；后端按字面量校验（deny_unknown_fields +
 *  值必须为 true，任何「没确认也发 false」都在类型层就被禁止）。 */
export interface ConfirmRequest extends PreviewRequest {
  challengeId: string;
  candidateToken: string;
  sameThreadAndQuestion: true;
  nativePanelQuestionCount: 1;
  nativeDraftWillBeCleared: true;
}

export interface SkipRequest {
  sessionId: string;
  group: GroupIdentity4;
  /** 服务端签发的绑定租约。绑定路径（配对三确认后）必填；**display-only 路径省略**
   *  （coordinator-wiring-contract §6：仅 Tuvis 持久展示跳过，不关原生卡；同一次
   *  无绑定请求不得事后补 binding）。 */
  bindingId?: string;
  /** Tuvis 动作 UUID：一次用户动作一次；失败重试**沿同 ID 同包**（幂等锚）。 */
  actionId: string;
}

export interface RestoreRequest {
  sessionId: string;
  group: GroupIdentity4;
  /** 乐观锁：回执 revision 必须严格大于该值才接受（stale 恢复拒绝）。 */
  expectedRevision: number;
  actionId: string;
}

export interface PreviewCandidate {
  token: string;
  question: string;
  options: string[];
  /** 服务端生成的**人类可辨**窗口/上下文标签（可选 additive wire）。同题同选项多候选
   *  必须有互异 label 才允许 confirm；执行 scope（PID/HWND/RID）不向客户端开放。 */
  label?: string;
}

export interface PreviewResponse {
  challengeId: string;
  candidates: PreviewCandidate[];
}

export interface ConfirmResponse {
  bindingId: string;
  expiresAt: number;
}

export interface SkipResponse {
  actionId: string;
  group: GroupIdentity4;
  displayDecision: "skipped";
  nativeEffect: QuestionNativeEffect;
  reasonCode: string | null;
  revision: number;
}

export interface RestoreResponse {
  actionId: string;
  group: GroupIdentity4;
  displayDecision: "active";
  revision: number;
}

// ==== 端点路径（唯一来源）====

export const CODEX_QUESTION_SKIP_BASE = "/m/api/v1/codex/question-skip";
/** 权威路径（计划 Task3）：配对两端为 pair-preview / pair-confirm；函数名
 *  previewQuestionSkip / confirmQuestionSkip 保持不变。 */
export const CODEX_QUESTION_SKIP_PREVIEW_PATH = `${CODEX_QUESTION_SKIP_BASE}/pair-preview`;
export const CODEX_QUESTION_SKIP_CONFIRM_PATH = `${CODEX_QUESTION_SKIP_BASE}/pair-confirm`;
export const CODEX_QUESTION_SKIP_SKIP_PATH = `${CODEX_QUESTION_SKIP_BASE}/skip`;
export const CODEX_QUESTION_SKIP_RESTORE_PATH = `${CODEX_QUESTION_SKIP_BASE}/restore`;

// ==== runtime 校验（回执安全边界）====

const NATIVE_EFFECTS: ReadonlySet<string> = new Set([
  "closed_verified",
  "not_present_verified",
  "unresolved",
  "unsupported",
]);

/** 正 safe int（>=1）：skip 落账 revision 的合法域（active 默认 0 只属于初始投影，
 *  不属于任何写回执——写回执必然推进过至少一次）。 */
function isPositiveSafeInt(v: unknown): v is number {
  return typeof v === "number" && Number.isSafeInteger(v) && v >= 1;
}

/** 非负 safe int（>=0）：投影展示 revision 的合法域（新代际默认 active revision 0 合法）。 */
export function isNonNegativeSafeInt(v: unknown): v is number {
  return typeof v === "number" && Number.isSafeInteger(v) && v >= 0;
}

function isNonEmptyString(v: unknown): v is string {
  return typeof v === "string" && v.length > 0;
}

/** 组身份全等：四字段逐字符串比对（部分相等不算同组）。 */
export function sameGroupIdentity4(a: GroupIdentity4, b: GroupIdentity4): boolean {
  return (
    a.threadId === b.threadId &&
    a.sourceTurnId === b.sourceTurnId &&
    a.sourceItemId === b.sourceItemId &&
    a.contentGeneration === b.contentGeneration
  );
}

/** 组身份形状守卫：四字段全为字符串才接受（脏载荷整帧拒绝，不半渲染）。 */
export function asGroupIdentity4(v: unknown): GroupIdentity4 | null {
  if (v === null || typeof v !== "object") return null;
  const g = v as Record<string, unknown>;
  if (
    !isNonEmptyString(g.threadId) ||
    !isNonEmptyString(g.sourceTurnId) ||
    !isNonEmptyString(g.sourceItemId) ||
    !isNonEmptyString(g.contentGeneration)
  ) {
    return null;
  }
  return {
    threadId: g.threadId,
    sourceTurnId: g.sourceTurnId,
    sourceItemId: g.sourceItemId,
    contentGeneration: g.contentGeneration,
  };
}

/** 组身份稳定键（内部账与消费方指纹共用；\0 分隔避免字段拼接歧义）。 */
export function groupIdentityKey(group: GroupIdentity4): string {
  return `${group.threadId}\u0000${group.sourceTurnId}\u0000${group.sourceItemId}\u0000${group.contentGeneration}`;
}

function groupKey(group: GroupIdentity4): string {
  return groupIdentityKey(group);
}

/** 跳过回执 runtime 复核（skip）：任何不匹配抛错——调用方不得把不匹配回执当成功。 */
export function validateSkipReceipt(raw: unknown, request: SkipRequest): SkipResponse {
  if (raw === null || typeof raw !== "object") {
    throw new ApiError(200, "codex/question-skip/skip: malformed receipt payload");
  }
  const r = raw as Record<string, unknown>;
  const group = asGroupIdentity4(r.group);
  if (
    r.actionId !== request.actionId ||
    group === null ||
    !sameGroupIdentity4(group, request.group) ||
    r.displayDecision !== "skipped" ||
    typeof r.nativeEffect !== "string" ||
    !NATIVE_EFFECTS.has(r.nativeEffect) ||
    !isPositiveSafeInt(r.revision)
  ) {
    throw new ApiError(200, "codex/question-skip/skip: receipt mismatch");
  }
  return {
    actionId: r.actionId,
    group,
    displayDecision: "skipped",
    nativeEffect: r.nativeEffect as QuestionNativeEffect,
    reasonCode: typeof r.reasonCode === "string" ? r.reasonCode : null,
    revision: r.revision,
  };
}

/** 恢复回执 runtime 复核：revision 必须严格大于 expectedRevision（stale 恢复拒绝）。 */
export function validateRestoreReceipt(raw: unknown, request: RestoreRequest): RestoreResponse {
  if (raw === null || typeof raw !== "object") {
    throw new ApiError(200, "codex/question-skip/restore: malformed receipt payload");
  }
  const r = raw as Record<string, unknown>;
  const group = asGroupIdentity4(r.group);
  if (
    r.actionId !== request.actionId ||
    group === null ||
    !sameGroupIdentity4(group, request.group) ||
    r.displayDecision !== "active" ||
    !isNonNegativeSafeInt(r.revision) ||
    r.revision <= request.expectedRevision
  ) {
    throw new ApiError(200, "codex/question-skip/restore: receipt mismatch");
  }
  return {
    actionId: r.actionId,
    group,
    displayDecision: "active",
    revision: r.revision,
  };
}

// ==== fetch 基建（与 codexIpcApi 同构）====

/** 同源 POST：body 白名单序列化（请求对象原样 JSON），认证走同源 cookie（默认
 *  same-origin，不显式写 credentials——与 api.ts 全站纪律一致）。 */
async function postQuestionSkip(
  path: string,
  body: unknown,
  signal?: AbortSignal
): Promise<unknown> {
  let r: Response;
  try {
    r = await fetch(path, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify(body),
      signal,
    });
  } catch (e) {
    if (e instanceof ApiError) throw e;
    throw new ApiError(null, `codex/question-skip: 网络异常 ${String(e)}`);
  }
  if (!r.ok) {
    let data: Record<string, unknown> | null = null;
    try {
      data = (await r.json()) as Record<string, unknown>;
    } catch {
      /* 非 JSON 错误体：data 保持 null */
    }
    throw new ApiError(r.status, `codex/question-skip ${r.status}`, data);
  }
  try {
    return await r.json();
  } catch {
    throw new ApiError(200, "codex/question-skip: malformed JSON payload");
  }
}

// ==== 四个端点 ====

/** 配对预览：服务端只读列出验证窗口中的活卡候选（完整题干 + 有序 options）。
 *  本函数不做任何 Invoke/写操作；候选选择与三项确认在 UI 层强制。 */
export async function previewQuestionSkip(
  request: PreviewRequest,
  signal?: AbortSignal
): Promise<PreviewResponse> {
  const raw = await postQuestionSkip(CODEX_QUESTION_SKIP_PREVIEW_PATH, request, signal);
  if (raw === null || typeof raw !== "object") {
    throw new ApiError(200, "codex/question-skip/preview: malformed payload");
  }
  const v = raw as Record<string, unknown>;
  if (!isNonEmptyString(v.challengeId) || !Array.isArray(v.candidates)) {
    throw new ApiError(200, "codex/question-skip/preview: malformed payload");
  }
  const candidates: PreviewCandidate[] = [];
  for (const entry of v.candidates) {
    if (entry === null || typeof entry !== "object") continue;
    const c = entry as Record<string, unknown>;
    if (!isNonEmptyString(c.token) || typeof c.question !== "string") continue;
    if (!Array.isArray(c.options) || !c.options.every((o) => typeof o === "string")) continue;
    candidates.push({
      token: c.token,
      question: c.question,
      options: c.options as string[],
      ...(typeof c.label === "string" && c.label.length > 0 ? { label: c.label } : {}),
    });
  }
  return { challengeId: v.challengeId, candidates };
}

/** 配对确认：challengeId + 用户所选候选 token + 三项字面量确认 → 不可伪造 bindingId。 */
export async function confirmQuestionSkip(
  request: ConfirmRequest,
  signal?: AbortSignal
): Promise<ConfirmResponse> {
  const raw = await postQuestionSkip(CODEX_QUESTION_SKIP_CONFIRM_PATH, request, signal);
  if (raw === null || typeof raw !== "object") {
    throw new ApiError(200, "codex/question-skip/confirm: malformed payload");
  }
  const v = raw as Record<string, unknown>;
  if (!isNonEmptyString(v.bindingId) || typeof v.expiresAt !== "number") {
    throw new ApiError(200, "codex/question-skip/confirm: malformed payload");
  }
  return { bindingId: v.bindingId, expiresAt: v.expiresAt };
}

/** 跳过：先落展示账（skipped）再尝试原生关闭；回执 runtime 复核（actionId/group/
 *  decision/effect/revision 全匹配才接受）。结果未知/网络失败**不自动重试**——调用方
 *  沿原 actionId 原包重试（见 QuestionSkipControls 的 retry 纪律）。 */
export async function skipQuestion(
  request: SkipRequest,
  signal?: AbortSignal
): Promise<SkipResponse> {
  const raw = await postQuestionSkip(CODEX_QUESTION_SKIP_SKIP_PATH, request, signal);
  return validateSkipReceipt(raw, request);
}

/** 恢复：只把该代际展示决策置回 active（不重开原生卡、不撤销已发生的原生动作）；
 *  回执 revision 必须严格大于 expectedRevision。 */
export async function restoreQuestion(
  request: RestoreRequest,
  signal?: AbortSignal
): Promise<RestoreResponse> {
  const raw = await postQuestionSkip(CODEX_QUESTION_SKIP_RESTORE_PATH, request, signal);
  return validateRestoreReceipt(raw, request);
}

// ==== 投影附加字段的帧解析 ====
//
// GET/SSE 投影可选携带 questionDisplayGroups / questionSkipHistory（附加只读字段）。
// 本模块只做**单帧**解析（在场性/合法性判定 + WeakMap 登记）；跨帧收敛（高水位、
// 双账独立 sticky invalid、source descriptor、gid↔题体观察）在 questionSkipOperations
// 的 FactLedger 分层实现（useCodexThread 持有）。

export type QuestionSkipFactFrameState = "absent" | "valid" | "invalid";

/** 单帧解析结果：displayState/historyState 标记该字段在场性与合法性。 */
export interface QuestionSkipFactFrame {
  displayGroups: QuestionDisplayGroupView[];
  history: QuestionSkipHistoryView[];
  displayState: QuestionSkipFactFrameState;
  historyState: QuestionSkipFactFrameState;
}

function asDisplayGroupEntry(entry: unknown): QuestionDisplayGroupView | null {
  if (entry === null || typeof entry !== "object") return null;
  const v = entry as Record<string, unknown>;
  const group = asGroupIdentity4(v.group);
  if (group === null || !isNonNegativeSafeInt(v.revision)) return null;
  if (v.decision !== "active" && v.decision !== "skipped") return null;
  if (
    v.latestNativeEffect !== null &&
    (typeof v.latestNativeEffect !== "string" || !NATIVE_EFFECTS.has(v.latestNativeEffect))
  ) {
    return null;
  }
  if (typeof v.sourceComplete !== "boolean" || !isNonNegativeSafeInt(v.sourceQuestionCount)) {
    return null;
  }
  return {
    group,
    revision: v.revision,
    decision: v.decision,
    latestNativeEffect:
      v.latestNativeEffect === null ? null : (v.latestNativeEffect as QuestionNativeEffect),
    reasonCode: typeof v.reasonCode === "string" ? v.reasonCode : null,
    sourceComplete: v.sourceComplete,
    sourceQuestionCount: v.sourceQuestionCount,
  };
}

function asHistoryEntry(entry: unknown): QuestionSkipHistoryView | null {
  if (entry === null || typeof entry !== "object") return null;
  const v = entry as Record<string, unknown>;
  const group = asGroupIdentity4(v.group);
  if (group === null || !isNonNegativeSafeInt(v.revision)) return null;
  if (v.decision !== "active" && v.decision !== "skipped") return null;
  if (
    v.latestNativeEffect !== null &&
    (typeof v.latestNativeEffect !== "string" || !NATIVE_EFFECTS.has(v.latestNativeEffect))
  ) {
    return null;
  }
  let question: QuestionSkipHistoryQuestion | null = null;
  if (v.question !== null && v.question !== undefined) {
    if (v.question === null || typeof v.question !== "object") return null;
    const q = v.question as Record<string, unknown>;
    if (
      !isNonEmptyString(q.questionKey) ||
      !isNonNegativeSafeInt(q.questionIndex) ||
      typeof q.title !== "string" ||
      !Array.isArray(q.options) ||
      !q.options.every((o) => typeof o === "string")
    ) {
      return null;
    }
    question = {
      questionKey: q.questionKey,
      questionIndex: q.questionIndex,
      title: q.title,
      options: q.options as string[],
    };
  }
  return {
    group,
    revision: v.revision,
    decision: v.decision,
    latestNativeEffect:
      v.latestNativeEffect === null ? null : (v.latestNativeEffect as QuestionNativeEffect),
    reasonCode: typeof v.reasonCode === "string" ? v.reasonCode : null,
    question,
  };
}

/** 解析一帧载荷中的两份账（附加字段：缺字段 = absent；在场但任一条目形状脏 =
 *  invalid——整帧拒绝不半渲染）。 */
export function parseQuestionSkipFactFrame(payload: unknown): QuestionSkipFactFrame {
  const frame: QuestionSkipFactFrame = {
    displayGroups: [],
    history: [],
    displayState: "absent",
    historyState: "absent",
  };
  if (payload === null || typeof payload !== "object") return frame;
  const p = payload as Record<string, unknown>;
  if (p.questionDisplayGroups !== undefined) {
    if (!Array.isArray(p.questionDisplayGroups)) {
      frame.displayState = "invalid";
    } else {
      const entries: QuestionDisplayGroupView[] = [];
      let valid = true;
      for (const entry of p.questionDisplayGroups) {
        const parsed = asDisplayGroupEntry(entry);
        if (parsed === null) {
          valid = false;
          break;
        }
        entries.push(parsed);
      }
      // 组身份去重（全 group4 唯一）：重复键是服务端账目脏——整帧按 invalid。
      const seen = new Set<string>();
      for (const entry of entries) {
        const key = groupKey(entry.group);
        if (seen.has(key)) {
          valid = false;
          break;
        }
        seen.add(key);
      }
      if (valid) {
        frame.displayGroups = entries;
        frame.displayState = "valid";
      } else {
        frame.displayState = "invalid";
      }
    }
  }
  if (p.questionSkipHistory !== undefined) {
    if (!Array.isArray(p.questionSkipHistory)) {
      frame.historyState = "invalid";
    } else {
      const entries: QuestionSkipHistoryView[] = [];
      let valid = true;
      for (const entry of p.questionSkipHistory) {
        const parsed = asHistoryEntry(entry);
        if (parsed === null) {
          valid = false;
          break;
        }
        entries.push(parsed);
      }
      const seen = new Set<string>();
      for (const entry of entries) {
        const key = groupKey(entry.group);
        if (seen.has(key)) {
          valid = false;
          break;
        }
        seen.add(key);
      }
      if (valid) {
        frame.history = entries;
        frame.historyState = "valid";
      } else {
        frame.historyState = "invalid";
      }
    }
  }
  return frame;
}

// ==== 分层事实快照类型（跨帧收敛实现见 questionSkipOperations.ts）====
//
// FactLedger（display/history 各一）持 lastLegalMembers/bestByGroup/invalid 三件：
// 高水位 (revision, acceptedSeq) 先比较再构造 next（不 clear 后比）；合法全量成员
// 增删合法、与 absent 可区分；invalid 各账独立 sticky，仅对应账收到合法帧才清。
// 同 gid 的 decision/effect 由 display/history/已受理回执三源按 (revision, seq) 统一
// 择新——旧 skip 不得遮新 restore、旧 historyClosed 不得遮新 Unknown、旧 display
// 不得压合法 history 更新。

/** gid ↔ 题体观察（qkey/index/完整题干/有序 options；exclude token/runtime/无关 source）。 */
export interface QuestionSkipBodyObservation {
  questionKey: string;
  questionIndex: number;
  title: string;
  orderedOptions: string[];
}

/** 同 gid 统一择新后的决策事实。 */
export interface QuestionSkipUnifiedFact {
  group: GroupIdentity4;
  decision: QuestionDisplayDecision;
  revision: number;
  latestNativeEffect: QuestionNativeEffect | null;
  reasonCode: string | null;
}

/** hook 侧分层事实快照（SessionDetail 唯一消费面；Client-derived 附加字段）。 */
export interface QuestionSkipFactsSnapshot {
  displayByGid: { gid: string; view: QuestionDisplayGroupView; seq: number }[];
  historyByGid: { gid: string; view: QuestionSkipHistoryView; seq: number }[];
  unifiedByGid: { gid: string; fact: QuestionSkipUnifiedFact; seq: number }[];
  /** 每 source 仅当前 server canonical 一个 gid（display 事实；合法帧唯一才在场）。 */
  descriptorBySource: {
    sourceTurnId: string;
    sourceItemId: string;
    gid: string;
    view: QuestionDisplayGroupView;
  }[];
  observations: { gid: string; bodies: QuestionSkipBodyObservation[] }[];
  displayInvalid: boolean;
  historyInvalid: boolean;
}

// ==== 帧状态登记（投影解析器 ↔ hook 落地守卫的通道）====
//
// asThreadProjection 是纯函数（无跨帧记忆），而「在场但非法 → 保最后合法 + 禁新
// skip」是跨帧语义。约定：投影解析器把每帧的解析状态按对象登记到 WeakMap，
// useCodexThread 在排序守卫批准落地后读取并收敛（迟到旧帧不进账；对象被 GC 登记随
// 之失效，无泄漏面）。

const frameReport = new WeakMap<object, QuestionSkipFactFrame>();

/** 投影解析器调用：登记该投影对象对应帧的 question-skip 字段解析状态。 */
export function recordQuestionSkipFactFrame(
  projection: object,
  frame: QuestionSkipFactFrame
): void {
  frameReport.set(projection, frame);
}

/** hook 落地时读取帧状态；未登记（手工构造的投影）→ undefined（按无事实处理）。 */
export function questionSkipFactFrameOf(projection: object): QuestionSkipFactFrame | undefined {
  return frameReport.get(projection);
}
