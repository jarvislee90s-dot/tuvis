// questionSkipOperations：Codex 题卡跳过的**已承诺操作控制器 + 分层事实账**。
//
// 三个寿命（committed-state-contract）：
// 1. CommittedGate（准入）：稳定 host 在 layout commit 发布 {scopeKey, disabled,
//    fact}；render 绝不写 gate。disabled/scope 变化推进 epoch；**每次新发前同步
//    check+issue（无 await）**；confirm continuation 二次准入用「配对开启时捕获的
//    epoch」比对——期间任何 disabled/scope 翻转都撤 prewrite（Pairing→Ready）。
//    disabled/unmount 只撤 prewrite+gate，**不撤 issued**。
// 2. Issued 请求：registry 按 (sessionId, full group4) 唯一持
//    Ready/Pairing/Writing{issued}/Unknown{issued}/Done{accepted}；控件只订阅。
//    issued {kind, actionId, frozen request, serializedBody} 写前同步锁存、此后
//    不可变；跨 disabled/unmount/A-B-A 存活。Unknown 只原包原 ID 重试；普通
//    restore/skip 在 Unknown 未决时**不得**签发同 kind 新 ID；反 kind 操作仅当
//    effective fact 已翻到其目标世界（更高 revision 且决策翻转）才放行。Done 跨
//    unmount/key 切换保留（accepted 结果是已受理事实缓存，不随挂载清）；旧 props
//    /handler 不得在 Done 世界重开同向操作。跨挂载的持久真值在 receipts 账 +
//    SessionDetail 分层事实（dock 隐藏/入口资格由统一决策驱动）。
// 3. Server 事实：FactLedger（display/history 各一）= lastLegalMembers/bestByGroup/
//    invalid。高水位 (revision, acceptedSeq) **先比较再构造 next**（不 clear 后比）；
//    合法全量成员增删与 absent 可区分；invalid 各账独立 sticky，仅对应账收到合法帧
//    才清。同 gid 的 decision/effect 由 display/history/已受理回执三源按
//    (revision, seq) 统一择新。每 source 仅当前 canonical 一个 gid（display 同源
//    多 group4 = 歧义，不跨 gen 比_revision）；gid↔题体观察 exclude token/runtime/
//    无关 source，暂缺不抹，真 body 矛盾解除旧 hide。
//
// 「明确未承接拒绝」必须语义证据（错误体机器码 ∈ 白名单），不得按 HTTP 4xx 类别猜；
// 其余一切失败一律 Unknown{issued}（原包重试）。
import {
  confirmQuestionSkip,
  previewQuestionSkip,
  restoreQuestion,
  skipQuestion,
  groupIdentityKey,
  type GroupIdentity4,
  type PreviewCandidate,
  type QuestionDisplayDecision,
  type QuestionDisplayGroupView,
  type QuestionNativeEffect,
  type QuestionSkipBodyObservation,
  type QuestionSkipFactFrame,
  type QuestionSkipFactsSnapshot,
  type QuestionSkipHistoryView,
  type QuestionSkipUnifiedFact,
  type RestoreRequest,
  type SkipRequest,
} from "./codexQuestionSkipApi";
import { ApiError } from "./api";
import { newActionId } from "./codexIpcApi";

// ==== scope key ====

export function questionSkipScopeKey(sessionId: string, group: GroupIdentity4): string {
  return `${sessionId}\u0000${groupIdentityKey(group)}`;
}

// ==== CommittedGate ====

export interface QuestionSkipGateState {
  scopeKey: string;
  disabled: boolean;
  epoch: number;
  /** host 以 props 发布的当前决策事实（issue 期的 expectedRevision 依据）。 */
  fact: { decision: QuestionDisplayDecision; revision: number } | null;
}

const gates = new Map<string, QuestionSkipGateState>();

/** 稳定 host（Controls 自身或 SessionDetail）在 **layout commit** 调用；render 期禁止。
 *  scopeKey/disabled 变化推进 epoch；fact 更新不推进。
 *  生命周期对账（在同一同步块内）：
 *  - disabled=true（含 unmount/key 切换的 withdrawal）→ 只撤 prewrite（Pairing→
 *    Ready）；issued 与 Done{accepted} 都保留（跨挂载）；
 *  - canonical fact 赶上/越过回执 revision → Done 退场回 Ready（新操作按 canonical
 *    世界准入；旧 props rev 落后不放行，事实账在 ledger 层不倒退）；
 *  - fact 决策翻转 ≠ 配对开启世界 → 配对现场作废（descriptor 换代不得沿用旧配对）。 */
export function publishQuestionSkipGate(
  key: string,
  input: {
    scopeKey: string;
    disabled: boolean;
    fact: { decision: QuestionDisplayDecision; revision: number } | null;
  }
): void {
  const current = gates.get(key);
  const bumped =
    current !== undefined &&
    (current.scopeKey !== input.scopeKey || current.disabled !== input.disabled);
  gates.set(key, {
    scopeKey: input.scopeKey,
    disabled: input.disabled,
    epoch: bumped ? current.epoch + 1 : (current?.epoch ?? 0),
    fact: input.fact,
  });
  const slot = registry.get(key);
  if (slot === undefined) return;
  if (input.disabled) {
    // disabled/unmount 只撤 prewrite（Pairing→Ready）；issued 与 Done{accepted} 保留。
    if (slot.state === "pairing") commitSlot(slot, { state: "ready", pairing: null });
    return;
  }
  if (
    slot.state === "done" &&
    slot.accepted !== null &&
    input.fact !== null &&
    input.fact.revision >= slot.accepted.resultRevision
  ) {
    // Done 后按 merged fact 放行新操作：canonical 投影已**赶上/越过**回执 revision
    // （同决策 = canonical 确证受理结果；异决策 = 外部新事件已接管）→ 操作缓存退场
    // 回 Ready，新 pair/skip/restore 按 canonical 世界准入（旧 props rev 落后时不放行）。
    commitSlot(slot, { state: "ready", accepted: null });
    return;
  }
  if (
    slot.state === "pairing" &&
    slot.pairing !== null &&
    input.fact !== null &&
    input.fact.decision !== slot.pairing.enteredDecision
  ) {
    commitSlot(slot, { state: "ready", pairing: null });
  }
}

function gateAdmitsNewSend(key: string, scopeKey: string): boolean {
  const gate = gates.get(key);
  return gate !== undefined && !gate.disabled && gate.scopeKey === scopeKey;
}

function gateEpoch(key: string): number {
  return gates.get(key)?.epoch ?? -1;
}

// ==== Issued 请求（不可变）====

export interface IssuedQuestionSkipRequest {
  kind: "skip" | "restore";
  actionId: string;
  request: SkipRequest | RestoreRequest;
  serializedBody: string;
}

function freezeIssued(
  kind: "skip" | "restore",
  request: SkipRequest | RestoreRequest
): IssuedQuestionSkipRequest {
  const frozen = Object.freeze({ ...request });
  return Object.freeze({
    kind,
    actionId: frozen.actionId,
    request: frozen,
    serializedBody: JSON.stringify(frozen),
  });
}

// ==== 受理结果 ====

export interface AcceptedQuestionSkipOutcome {
  kind: "skip" | "restore";
  actionId: string;
  decision: QuestionDisplayDecision;
  resultRevision: number;
  /** restore 回执不携带原生效果（恢复不关原生卡）→ null；skip 回执如实透传。 */
  nativeEffect: QuestionNativeEffect | null;
  /** skip 回执的机器码（如实展示/去重键的一部分）；restore 恒 null。 */
  reasonCode: string | null;
}

// ==== 语义拒绝（确证未承接才可解除 issued/pairing；不得按 4xx 类别猜）====

/** 服务端「写尝试开始前即拒绝」的机器码白名单（与既有 pending-response 前置拒绝
 *  词表同源口径 + question-skip 专属码）。错误体无机器码/词表外一律按 Unknown。 */
const QUESTION_SKIP_REFUSAL_CODES: ReadonlySet<string> = new Set([
  "bad_request",
  "no_session",
  "no_action",
  "forbidden",
  "action_mismatch",
  "binding_required",
  "challenge_unknown",
  "challenge_expired",
  "revision_conflict",
  "already_active",
  "unsupported_group",
  "unsupported_build",
  "invalid_group",
]);

export function isQuestionSkipDefiniteRefusal(e: unknown): boolean {
  if (!(e instanceof ApiError) || e.status === null) return false;
  const code = typeof e.data?.error === "string" ? e.data.error : null;
  return code !== null && QUESTION_SKIP_REFUSAL_CODES.has(code);
}

function errorReason(e: unknown): string {
  if (e instanceof ApiError) {
    const reason = typeof e.data?.reason === "string" ? e.data.reason : null;
    return reason ?? (e.status !== null ? String(e.status) : e.message);
  }
  return String(e);
}

// ==== registry / 快照 / 订阅 ====

export type QuestionSkipOpState = "ready" | "pairing" | "writing" | "unknown" | "done";

interface PairingSession {
  challengeId: string;
  candidates: PreviewCandidate[];
  selectedToken: string | null;
  acks: { thread: boolean; count: boolean; draft: boolean };
  /** 配对开启时的 gate epoch：confirm continuation 二次准入比对（期间任何
   *  disabled/scope 翻转 → 撤 prewrite 回 Ready）。 */
  gateEpoch: number;
  /** 配对开启时的决策世界（descriptor 翻转 → 配对作废）。 */
  enteredDecision: QuestionDisplayDecision;
}

export interface QuestionSkipSnapshot {
  key: string;
  state: QuestionSkipOpState;
  issued: { kind: "skip" | "restore"; actionId: string; serializedBody: string } | null;
  accepted: AcceptedQuestionSkipOutcome | null;
  pairing: PairingSession | null;
  previewing: boolean;
  error: { field: "pairing" | "skip" | "restore"; reason: string } | null;
}

interface Slot {
  key: string;
  sessionId: string;
  group: GroupIdentity4;
  state: QuestionSkipOpState;
  issued: IssuedQuestionSkipRequest | null;
  accepted: AcceptedQuestionSkipOutcome | null;
  pairing: PairingSession | null;
  previewing: boolean;
  /** 在飞 preview 尝试身份（nonce）：continuation 只认**当前**尝试——迟到的旧
   *  preview 不得复活 Pairing/清 accepted，也不得取消较新 preview。 */
  previewAttemptId: string | null;
  error: { field: "pairing" | "skip" | "restore"; reason: string } | null;
  snapshot: QuestionSkipSnapshot;
  listeners: Set<() => void>;
}

const registry = new Map<string, Slot>();
const EMPTY_SNAPSHOT: QuestionSkipSnapshot = Object.freeze({
  key: "",
  state: "ready" as const,
  issued: null,
  accepted: null,
  pairing: null,
  previewing: false,
  error: null,
});

function snapshotOf(slot: Slot): QuestionSkipSnapshot {
  return Object.freeze({
    key: slot.key,
    state: slot.state,
    issued:
      slot.issued === null
        ? null
        : Object.freeze({
            kind: slot.issued.kind,
            actionId: slot.issued.actionId,
            serializedBody: slot.issued.serializedBody,
          }),
    accepted: slot.accepted === null ? null : Object.freeze({ ...slot.accepted }),
    pairing:
      slot.pairing === null
        ? null
        : Object.freeze({ ...slot.pairing, acks: { ...slot.pairing.acks } }),
    previewing: slot.previewing,
    error: slot.error === null ? null : Object.freeze({ ...slot.error }),
  });
}

function ensureSlot(key: string): Slot {
  let slot = registry.get(key);
  if (slot === undefined) {
    slot = {
      key,
      sessionId: "",
      group: { threadId: "", sourceTurnId: "", sourceItemId: "", contentGeneration: "" },
      state: "ready",
      issued: null,
      accepted: null,
      pairing: null,
      previewing: false,
      previewAttemptId: null,
      error: null,
      snapshot: EMPTY_SNAPSHOT,
      listeners: new Set(),
    };
    slot.snapshot = snapshotOf(slot);
    registry.set(key, slot);
  }
  return slot;
}

function commitSlot(
  slot: Slot,
  patch: Partial<Omit<Slot, "key" | "snapshot" | "listeners">>
): void {
  Object.assign(slot, patch);
  slot.snapshot = snapshotOf(slot);
  for (const listener of slot.listeners) listener();
}

export function subscribeQuestionSkip(key: string, listener: () => void): () => void {
  const slot = ensureSlot(key);
  slot.listeners.add(listener);
  return () => {
    slot.listeners.delete(listener);
  };
}

export function getQuestionSkipSnapshot(key: string): QuestionSkipSnapshot {
  return registry.get(key)?.snapshot ?? EMPTY_SNAPSHOT;
}

// ==== effective fact（accepted 回执 vs props 事实，按 revision 单调合并；同 rev 回执为准）====

export function resolveEffectiveQuestionSkipDecision(
  accepted: AcceptedQuestionSkipOutcome | null,
  propsFact: { decision: QuestionDisplayDecision; revision: number } | null
): { decision: QuestionDisplayDecision; revision: number } {
  if (accepted !== null && (propsFact === null || accepted.resultRevision >= propsFact.revision)) {
    return { decision: accepted.decision, revision: accepted.resultRevision };
  }
  if (propsFact !== null) return { decision: propsFact.decision, revision: propsFact.revision };
  return { decision: "active", revision: 0 };
}

// ==== 操作 ====

function baseGuard(slot: Slot, params: { sessionId: string; group: GroupIdentity4 }): void {
  slot.sessionId = params.sessionId;
  slot.group = params.group;
}

/** **统一接纳时钟**：帧事实与已受理回执共用同一单调序（回执与帧的先后用同一把
 *  尺子量，禁止两套计数器互相比较）。display/history 帧内各占一拍，回执在受理时
 *  占一拍——同 revision 下后来者胜。 */
let acceptanceClock = 0;
function nextAcceptanceSeq(): number {
  acceptanceClock += 1;
  return acceptanceClock;
}

/** 同 gid 已受理回执沉淀进统一事实层（SessionDetail 经会话订阅即时重算）。 */
export interface RecordedReceiptFact {
  sessionId: string;
  group: GroupIdentity4;
  decision: QuestionDisplayDecision;
  revision: number;
  nativeEffect: QuestionNativeEffect | null;
  reasonCode: string | null;
  seq: number;
}
const receiptFacts = new Map<string, RecordedReceiptFact>();
const receiptListeners = new Map<string, Set<() => void>>();

/** 受理回执（先落事实再通知）：每 gid 保持 revision 高水位——旧回执不得覆盖
 *  高 revision 事实；同 revision 后到者胜。 */
function recordAcceptedReceipt(slot: Slot, accepted: AcceptedQuestionSkipOutcome): void {
  const key = `${slot.sessionId}\u0000${groupIdentityKey(slot.group)}`;
  const existing = receiptFacts.get(key);
  if (existing !== undefined && existing.revision > accepted.resultRevision) return;
  receiptFacts.set(key, {
    sessionId: slot.sessionId,
    group: { ...slot.group },
    decision: accepted.decision,
    revision: accepted.resultRevision,
    nativeEffect: accepted.nativeEffect,
    reasonCode: accepted.reasonCode,
    seq: nextAcceptanceSeq(),
  });
  for (const listener of receiptListeners.get(slot.sessionId) ?? []) listener();
}

/** 会话级订阅：受理回执落账即通知父层重算（GET 刷新失败不掩盖已知成功）。 */
export function subscribeAcceptedQuestionSkipReceipts(
  sessionId: string,
  listener: () => void
): () => void {
  let listeners = receiptListeners.get(sessionId);
  if (listeners === undefined) {
    listeners = new Set();
    receiptListeners.set(sessionId, listeners);
  }
  listeners.add(listener);
  return () => {
    listeners.delete(listener);
    if (listeners.size === 0) receiptListeners.delete(sessionId);
  };
}

/** 统一事实合成用：本会话全部已受理回执（与帧事实同钟同台择新）。 */
export function getAcceptedQuestionSkipReceipts(sessionId: string): RecordedReceiptFact[] {
  const out: RecordedReceiptFact[] = [];
  for (const fact of receiptFacts.values()) {
    if (fact.sessionId === sessionId) out.push(fact);
  }
  return out;
}

/** 打开配对（**仅 Ready**→Pairing；Done{restore} 等 canonical 赶上由 gate 释放回
 *  Ready 后才可新配对——Done 世界里的旧 native intent 不得发 preview/清 accepted）；
 *  preview 在飞不锁 issued。每次 preview 持**独立尝试身份**（nonce）：continuation
 *  只认当前尝试 + 有效 Ready——display-only 签发 / 新 preview / disabled·scope·
 *  unmount 都使旧尝试失效，迟到的旧回包既不能复活 Pairing/清 accepted，也不能
 *  取消或污染较新尝试。 */
export async function openQuestionSkipPairing(
  key: string,
  params: { sessionId: string; group: GroupIdentity4; effectiveDecision: QuestionDisplayDecision }
): Promise<void> {
  const slot = ensureSlot(key);
  baseGuard(slot, params);
  if (slot.previewing) return;
  if (slot.state !== "ready") return;
  if (params.effectiveDecision !== "active") return;
  if (!gateAdmitsNewSend(key, key)) return;
  // 请求前捕获 gate epoch + 签发尝试身份：响应回来必须「尝试仍是当前 + 有效 Ready +
  // 同 epoch + 仍准入」才进 Pairing——disabled/scope 翻转、display-only 抢先签发、
  // 新 preview、restore 受理都让旧 preview 作废。
  const epochAtRequest = gateEpoch(key);
  const attemptId = newActionId();
  commitSlot(slot, { previewing: true, previewAttemptId: attemptId, error: null });
  let challenge: Awaited<ReturnType<typeof previewQuestionSkip>>;
  try {
    challenge = await previewQuestionSkip({ sessionId: params.sessionId, group: params.group });
  } catch (e) {
    if (slot.previewAttemptId === attemptId) {
      if (gateAdmitsNewSend(key, key)) {
        commitSlot(slot, {
          previewing: false,
          error: { field: "pairing", reason: errorReason(e) },
        });
      } else {
        commitSlot(slot, { previewing: false });
      }
    } // 尝试已被替换：旧失败不得污染新 preview
    return;
  }
  // 二次准入：**尝试身份仍是当前**（未被 display-only 签发 / 新 preview / disabled
  // /restore 受理取代）+ epoch 未变 + 仍准入 + 槽仍处**有效 Ready**（restore 等
  // 写受理已把槽推离 Ready → 旧 preview 一律作废）。
  if (
    slot.previewAttemptId !== attemptId ||
    gateEpoch(key) !== epochAtRequest ||
    !gateAdmitsNewSend(key, key) ||
    slot.state !== "ready"
  ) {
    if (slot.previewAttemptId === attemptId) commitSlot(slot, { previewing: false });
    return; // 尝试已被取代：静默丢弃（不复活 Pairing/不清 accepted/不污染新 preview）
  }
  commitSlot(slot, {
    previewing: false,
    previewAttemptId: null, // 尝试已消费
    state: "pairing",
    accepted: null,
    error: null,
    pairing: {
      challengeId: challenge.challengeId,
      candidates: challenge.candidates,
      selectedToken: null,
      acks: { thread: false, count: false, draft: false },
      gateEpoch: epochAtRequest,
      enteredDecision: "active",
    },
  });
}

/** 选择候选：ack 与 challenge+candidate 绑定——换候选即清全部确认。 */
export function selectQuestionSkipCandidate(key: string, token: string): void {
  const slot = registry.get(key);
  if (slot === undefined || slot.state !== "pairing" || slot.pairing === null) return;
  if (!slot.pairing.candidates.some((candidate) => candidate.token === token)) return;
  commitSlot(slot, {
    pairing: {
      ...slot.pairing,
      selectedToken: token,
      acks: { thread: false, count: false, draft: false },
    },
  });
}

export function setQuestionSkipAck(
  key: string,
  field: "thread" | "count" | "draft",
  value: boolean
): void {
  const slot = registry.get(key);
  if (slot === undefined || slot.state !== "pairing" || slot.pairing === null) return;
  commitSlot(slot, {
    pairing: { ...slot.pairing, acks: { ...slot.pairing.acks, [field]: value } },
  });
}

/** 候选集**两两可辨**判定（confirm 准入供能与 handler 复核同用本函数）：
 *  - 单候选恒可辨（旧 fixture：无 label 单候选兼容）；
 *  - 多候选：任意「同题文且同 options」的候选对必须 label 非空且互不相等——
 *  题文不同本身即可辨；label 相同/缺失的不可辨对禁止 confirm（不猜窗口，
 *  PID/HWND/RID 仍 server-only）。 */
export function questionSkipCandidatesDistinguishable(candidates: PreviewCandidate[]): boolean {
  if (candidates.length <= 1) return true;
  const labelOf = (candidate: PreviewCandidate): string | null =>
    typeof candidate.label === "string" && candidate.label.trim().length > 0
      ? candidate.label.trim()
      : null;
  for (let i = 0; i < candidates.length; i += 1) {
    for (let j = i + 1; j < candidates.length; j += 1) {
      const a = candidates[i];
      const b = candidates[j];
      if (a.question !== b.question) continue; // 题文不同 → 天然可辨
      const sameOptions =
        a.options.length === b.options.length && a.options.every((o, k) => o === b.options[k]);
      if (!sameOptions) continue; // options 不同 → 可辨
      const labelA = labelOf(a);
      const labelB = labelOf(b);
      if (labelA === null || labelB === null || labelA === labelB) return false;
    }
  }
  return true;
}

/** 确认并跳过：pair-confirm → **二次准入（gate epoch + 配对现场未变）** → 同步 issue
 *  skip（check 与 fetch 之间无 await）。 */
export async function confirmQuestionSkipPairing(
  key: string,
  params: { sessionId: string; group: GroupIdentity4; onChanged?: () => void }
): Promise<void> {
  const slot = ensureSlot(key);
  baseGuard(slot, params);
  if (slot.state !== "pairing" || slot.pairing === null) return;
  const pairing = slot.pairing;
  if (!gateAdmitsNewSend(key, key)) return;
  if (pairing.selectedToken === null) return;
  if (!(pairing.acks.thread && pairing.acks.count && pairing.acks.draft)) return;
  if (!questionSkipCandidatesDistinguishable(pairing.candidates)) return;
  const candidate = pairing.candidates.find((c) => c.token === pairing.selectedToken);
  if (candidate === undefined) return;
  let binding: { bindingId: string };
  try {
    binding = await confirmQuestionSkip({
      sessionId: params.sessionId,
      group: params.group,
      challengeId: pairing.challengeId,
      candidateToken: candidate.token,
      sameThreadAndQuestion: true,
      nativePanelQuestionCount: 1,
      nativeDraftWillBeCleared: true,
    });
  } catch (e) {
    // 绑定未建立（无 actionId）：清 prewrite 回 Ready；语义拒绝/未知都允许重新配对。
    if (slot.state === "pairing" && slot.pairing === pairing) {
      commitSlot(slot, {
        state: "ready",
        pairing: null,
        error: { field: "pairing", reason: errorReason(e) },
      });
    }
    return;
  }
  // 二次准入：gate epoch 与配对开启时一致 + 配对现场未被替换（disabled/scope/descriptor
  // 任一翻转都撤 prewrite——不得在撤销后补发 skip）。
  if (
    !gateAdmitsNewSend(key, key) ||
    slot.state !== "pairing" ||
    slot.pairing !== pairing ||
    slot.pairing.gateEpoch !== gateEpoch(key)
  ) {
    if (slot.state === "pairing" && slot.pairing === pairing) {
      commitSlot(slot, { state: "ready", pairing: null });
    }
    return;
  }
  await issueQuestionSkip(key, { bindingId: binding.bindingId, onChanged: params.onChanged });
}

/** 已签发 skip：请求在 transition 到 Writing 的**同一同步块**内发出（准入→issue 无 await）。 */
async function issueQuestionSkip(
  key: string,
  params: { bindingId: string; onChanged?: () => void }
): Promise<void> {
  const slot = registry.get(key);
  if (slot === undefined || slot.state !== "pairing" || slot.pairing === null) return;
  if (!gateAdmitsNewSend(key, key)) return;
  const request: SkipRequest = Object.freeze({
    sessionId: slot.sessionId,
    group: { ...slot.group },
    bindingId: params.bindingId,
    actionId: newActionId(),
  });
  const issued = freezeIssued("skip", request);
  commitSlot(slot, {
    state: "writing",
    pairing: null,
    previewing: false,
    previewAttemptId: null,
    issued,
    error: null,
  });
  try {
    const receipt = await skipQuestion(request);
    settleIssuedSuccess(
      key,
      issued,
      {
        kind: "skip",
        actionId: receipt.actionId,
        decision: receipt.displayDecision,
        resultRevision: receipt.revision,
        nativeEffect: receipt.nativeEffect,
        reasonCode: receipt.reasonCode,
      },
      params.onChanged
    );
  } catch (e) {
    settleIssuedFailure(key, issued, e);
  }
}

/** Display-only skip（§6）：无绑定（bindingId 省略）——仅 Tuvis 持久展示跳过，
 *  不关原生卡；**绝不调用 preview/confirm/discovery/Native**。准入：ready/pairing
 *  （原生路径降级）/done 且 current effective active；issued 未决禁新。
 *  检查 → issued 锁存 → 发送同同步块（无 await）；同一次无绑定请求不得事后加
 *  binding（request 冻结）——未决 confirm 晚回 binding 也由 pairing 清空天然拒绝。 */
export async function issueQuestionSkipDisplayOnly(
  key: string,
  params: { sessionId: string; group: GroupIdentity4; onChanged?: () => void }
): Promise<void> {
  const slot = ensureSlot(key);
  baseGuard(slot, params);
  if (slot.state === "writing" || slot.state === "unknown") return; // issued 未决禁新
  // **仅 Ready/有效 Pairing**（不要 Done）：restore 受理后 canonical 未赶上时
  // （Done{restore} 保留），captured display-only intent 不得借 Done 世界新发 skip；
  // canonical 赶上后 publishGate 释放回 Ready，才按新世界准入。
  if (!(slot.state === "ready" || slot.state === "pairing")) return;
  const effective = resolveEffectiveQuestionSkipDecision(
    slot.accepted,
    gates.get(key)?.fact ?? null
  );
  if (effective.decision !== "active") return;
  if (!gateAdmitsNewSend(key, key)) return;
  const request: SkipRequest = Object.freeze({
    sessionId: slot.sessionId,
    group: { ...slot.group },
    actionId: newActionId(),
  });
  const issued = freezeIssued("skip", request);
  // pairing 降级：现场废弃（晚回 binding 不能补发第二 skip——confirm continuation
  // 因 state/pairing 引用不一致被拒）；同时在飞 preview 尝试一并撤销（旧回包不得
  // 在 display-only Done 之后复活 Pairing）。
  commitSlot(slot, {
    state: "writing",
    pairing: null,
    previewing: false,
    previewAttemptId: null,
    issued,
    error: null,
  });
  try {
    const receipt = await skipQuestion(request);
    settleIssuedSuccess(
      key,
      issued,
      {
        kind: "skip",
        actionId: receipt.actionId,
        decision: receipt.displayDecision,
        resultRevision: receipt.revision,
        nativeEffect: receipt.nativeEffect,
        reasonCode: receipt.reasonCode,
      },
      params.onChanged
    );
  } catch (e) {
    settleIssuedFailure(key, issued, e);
  }
}

/** 普通恢复：Ready/Done(merged skipped) → Writing{issued}；Unknown 未决时**禁止**
 *  同 kind 新 ID（原包重试是唯一出口）。 */
export async function beginQuestionSkipRestore(
  key: string,
  params: { sessionId: string; group: GroupIdentity4; onChanged?: () => void }
): Promise<void> {
  const slot = ensureSlot(key);
  baseGuard(slot, params);
  if (slot.state === "writing" || slot.state === "unknown") return; // issued 未决：禁新 ID
  if (!(slot.state === "ready" || slot.state === "done")) return;
  const effective = resolveEffectiveQuestionSkipDecision(
    slot.accepted,
    gates.get(key)?.fact ?? null
  );
  if (effective.decision !== "skipped") return;
  if (!gateAdmitsNewSend(key, key)) return;
  const request: RestoreRequest = Object.freeze({
    sessionId: slot.sessionId,
    group: { ...slot.group },
    expectedRevision: effective.revision,
    actionId: newActionId(),
  });
  const issued = freezeIssued("restore", request);
  commitSlot(slot, {
    state: "writing",
    pairing: null,
    previewing: false,
    previewAttemptId: null,
    issued,
    error: null,
  });
  try {
    const receipt = await restoreQuestion(request);
    settleIssuedSuccess(
      key,
      issued,
      {
        kind: "restore",
        actionId: receipt.actionId,
        decision: receipt.displayDecision,
        resultRevision: receipt.revision,
        nativeEffect: null,
        reasonCode: null,
      },
      params.onChanged
    );
  } catch (e) {
    settleIssuedFailure(key, issued, e);
  }
}

/** Unknown{issued} 的唯一出口：同包同 ID 重发（serializedBody 字节级一致）。
 *  readOnly（历史只读条目持有槽内 issued）时**免 gate 准入**：只读实例不发布也不
 *  撤销 gate，若同 scope 已无活动实例（或活动实例已退场留下撤准入），gate 缺席不
 *  得吞掉原包唯一出口。此处只重发**已冻结原包**，不新发任何动作；disabled（写锁/
 *  stale）与槽状态仍在同步守卫内，按钮 disabled 与本守卫同源。
 *  成功结算后的 onChanged：活动实例仍由 gate 准入触发（settleIssuedSuccess 原逻辑）；
 *  readOnly 无 gate 时**只在调用方确认实例仍挂载且未 disabled** 才由本函数补发一次
 *  （调用方已按 mounted/key/disabled 防护），否则不刷新。 */
export async function retryQuestionSkipIssued(
  key: string,
  params: { onChanged?: () => void; readOnly?: boolean; disabled?: boolean } = {}
): Promise<void> {
  const slot = ensureSlot(key);
  if (slot.state !== "unknown" || slot.issued === null) return;
  if (params.disabled === true) return;
  const readOnly = params.readOnly === true;
  if (!readOnly && !gateAdmitsNewSend(key, key)) return;
  const issued = slot.issued;
  commitSlot(slot, { state: "writing", previewing: false, previewAttemptId: null, error: null });
  // settleIssuedSuccess 只在 gate 准入时触发 onChanged；readOnly 且 gate 缺席/已撤
  // 时由本函数在**同一结算之后**补发一次（调用方的 onChanged 已按 mounted/key/
  // disabled 防护）。用标记避免重复通知。
  let gateNotified = false;
  const notify =
    params.onChanged === undefined
      ? undefined
      : () => {
          gateNotified = true;
          params.onChanged?.();
        };
  try {
    if (issued.kind === "skip") {
      const receipt = await skipQuestion(issued.request as SkipRequest);
      settleIssuedSuccess(
        key,
        issued,
        {
          kind: "skip",
          actionId: receipt.actionId,
          decision: receipt.displayDecision,
          resultRevision: receipt.revision,
          nativeEffect: receipt.nativeEffect,
          reasonCode: receipt.reasonCode,
        },
        notify
      );
    } else {
      const receipt = await restoreQuestion(issued.request as RestoreRequest);
      settleIssuedSuccess(
        key,
        issued,
        {
          kind: "restore",
          actionId: receipt.actionId,
          decision: receipt.displayDecision,
          resultRevision: receipt.revision,
          nativeEffect: null,
          reasonCode: null,
        },
        notify
      );
    }
    if (readOnly && params.onChanged !== undefined && !gateNotified) {
      const settled = registry.get(key);
      if (settled?.accepted != null && settled.accepted.actionId === issued.actionId) {
        params.onChanged();
      }
    }
  } catch (e) {
    settleIssuedFailure(key, issued, e);
  }
}

/** 回执只落**自己的槽**（actionId 比对 = 晚 A 只 A 槽）； onChanged 仅在 gate 准入时
 *  触发（stale scope 的回执静默沉淀，不惊动新 scope）。 */
function settleIssuedSuccess(
  key: string,
  issued: IssuedQuestionSkipRequest,
  accepted: AcceptedQuestionSkipOutcome,
  onChanged?: () => void
): void {
  const slot = registry.get(key);
  if (slot === undefined || slot.issued === null || slot.issued.actionId !== issued.actionId)
    return;
  // 先落事实（receipts 账 + 会话订阅者立即重算），再通知 slot（控件订阅面）。
  recordAcceptedReceipt(slot, accepted);
  commitSlot(slot, {
    state: "done",
    accepted,
    issued: null,
    error: null,
  });
  if (gateAdmitsNewSend(key, key)) onChanged?.();
}

function settleIssuedFailure(key: string, issued: IssuedQuestionSkipRequest, e: unknown): void {
  const slot = registry.get(key);
  if (slot === undefined || slot.issued === null || slot.issued.actionId !== issued.actionId)
    return;
  if (isQuestionSkipDefiniteRefusal(e)) {
    // 确证未承接（错误体机器码 ∈ 白名单）：解除 issued 回 Ready。
    commitSlot(slot, {
      state: "ready",
      issued: null,
      error: { field: issued.kind, reason: errorReason(e) },
    });
    return;
  }
  // 网络/5xx/载荷异常/回执不匹配 = 结果未知：原包原 ID 保持，只允许原包重试。
  commitSlot(slot, { state: "unknown", error: { field: issued.kind, reason: errorReason(e) } });
}

// ==== hook 侧落地附件（分层快照 → 投影附加字段）====

/** FactLedger 快照 → ThreadProjection 附加字段。空账不产出数组字段（undefined =
 *  旧后端/尚无事实，不得伪造成 []）；questionSkipFacts 恒在场（SessionDetail 的
 *  分层消费面；displayByGid/historyByGid/unified/descriptor/observations/invalid）。 */
export function questionSkipFactsAttachment(facts: QuestionSkipFactsSnapshot): {
  questionDisplayGroups?: QuestionDisplayGroupView[];
  questionSkipHistory?: QuestionSkipHistoryView[];
  questionSkipInvalid?: boolean;
  questionSkipFacts: QuestionSkipFactsSnapshot;
} {
  const attachment: {
    questionDisplayGroups?: QuestionDisplayGroupView[];
    questionSkipHistory?: QuestionSkipHistoryView[];
    questionSkipInvalid?: boolean;
    questionSkipFacts: QuestionSkipFactsSnapshot;
  } = { questionSkipFacts: facts };
  if (facts.displayByGid.length > 0) {
    attachment.questionDisplayGroups = facts.displayByGid.map((entry) => entry.view);
  }
  if (facts.historyByGid.length > 0) {
    attachment.questionSkipHistory = facts.historyByGid.map((entry) => entry.view);
  }
  if (facts.displayInvalid || facts.historyInvalid) {
    attachment.questionSkipInvalid = true;
  }
  return attachment;
}

// ==== FactLedger（分层事实）====

interface FactEntry<V> {
  view: V;
  seq: number;
}

/** 单账三件套：`best`（每 gid 高水位 (revision, seq)，**永续保留**——合法清空成员
 *  也不删，防 stale 重现倒退）、`lastLegalMembers`（最近合法帧的全量成员，可空）、
 *  `invalidFlag`（独立 sticky，仅本账收到合法帧才清）。
 *  快照列表 = lastLegalMembers ∩ best（成员被合法帧移除 → 暂不可见；重现且不倒退
 *  才回到账面）。`rejectSameSourceGenerations`（display 专用）：同 source 多
 *  group4 = 服务端账目歧义 → 整帧按 invalid 处理（保 lastLegal/best，不猜当前代）。 */
export class QuestionSkipFactLedger<V extends { group: GroupIdentity4; revision: number }> {
  private readonly rejectSameSourceGenerations: boolean;
  private best = new Map<string, FactEntry<V>>();
  private lastLegalMembers = new Set<string>();
  private invalidFlag = false;

  constructor(rejectSameSourceGenerations = false) {
    this.rejectSameSourceGenerations = rejectSameSourceGenerations;
  }

  /** 合法帧准入并高水位合并（**best 永续保留**——合法 empty 清成员集但 gid 高水位
   *  跨成员退出保留，stale 重现不倒退）；lastLegalMembers 独立按本帧成员构造。
   *  返回本帧是否被本账实际接纳（valid 且未被同源多 gen 拒收）；absent/invalid/拒收
   *  → false。 */
  advance(entries: V[] | null, state: "absent" | "valid" | "invalid", seq: number): boolean {
    if (state === "invalid") {
      this.invalidFlag = true; // sticky；事实不动
      return false;
    }
    if (state === "absent") return false; // 保持；invalid 不清
    if (this.rejectSameSourceGenerations) {
      const seenSources = new Set<string>();
      for (const entry of entries ?? []) {
        const sourceKey = `${entry.group.sourceTurnId}|${entry.group.sourceItemId}`;
        if (seenSources.has(sourceKey)) {
          this.invalidFlag = true; // 同 source 多 gen：保 lastLegal/best，不替换、不猜当前代
          return false;
        }
        seenSources.add(sourceKey);
      }
    }
    // 高水位合并：只更新本帧出现的 gid，**绝不重建/清空 best**（成员退出的 gid
    // 保留高水位，stale 低 rev 重现不倒退）；合法成员增删只动 lastLegalMembers。
    for (const entry of entries ?? []) {
      const gid = groupIdentityKey(entry.group);
      const prev = this.best.get(gid);
      if (prev === undefined || entry.revision >= prev.view.revision) {
        this.best.set(gid, { view: entry, seq });
      }
    }
    this.lastLegalMembers = new Set((entries ?? []).map((entry) => groupIdentityKey(entry.group)));
    this.invalidFlag = false;
    return true;
  }

  /** 快照 = lastLegalMembers 的 best 视图（插入序 = 最近合法帧的成员序）。 */
  snapshot(): { gid: string; view: V; seq: number }[] {
    const out: { gid: string; view: V; seq: number }[] = [];
    for (const gid of this.lastLegalMembers) {
      const entry = this.best.get(gid);
      if (entry !== undefined) out.push({ gid, view: entry.view, seq: entry.seq });
    }
    return out;
  }

  isInvalid(): boolean {
    return this.invalidFlag;
  }
}

export function createQuestionSkipFactsStore(): QuestionSkipFactsStore {
  return new QuestionSkipFactsStore();
}

/** 帧事实快照（display/history 两账 + gid↔题体观察；**不含**回执/统一决策/描述符——
 *  那些由 buildQuestionSkipFactsSnapshot 用同一接纳时钟与 receipts 合成）。 */
export interface QuestionSkipFrameFacts {
  displayByGid: { gid: string; view: QuestionDisplayGroupView; seq: number }[];
  historyByGid: { gid: string; view: QuestionSkipHistoryView; seq: number }[];
  observations: { gid: string; bodies: QuestionSkipBodyObservation[] }[];
  displayInvalid: boolean;
  historyInvalid: boolean;
}

export class QuestionSkipFactsStore {
  readonly display = new QuestionSkipFactLedger<QuestionDisplayGroupView>(true);
  readonly history = new QuestionSkipFactLedger<QuestionSkipHistoryView>(false);
  private readonly observations = new Map<string, QuestionSkipBodyObservation[]>();

  /** 仅在排序守卫批准落地后调用（迟到旧帧不进账）；seq 取自**统一接纳时钟**
   *  （与已受理回执同钟——同 revision 先后用同一把尺子量）。 */
  advance(
    frame: QuestionSkipFactFrame,
    asyncQuestions:
      | {
          sourceTurnId: string;
          sourceItemId: string;
          questionKey: string;
          questionIndex: number;
          title: string;
          options: string[];
        }[]
      | undefined
  ): void {
    const displaySeq = nextAcceptanceSeq();
    const historySeq = nextAcceptanceSeq();
    const displayAccepted = this.display.advance(
      frame.displayGroups,
      frame.displayState,
      displaySeq
    );
    this.history.advance(frame.history, frame.historyState, historySeq);
    // gid ↔ 题体观察：**只从被 display 账实际接纳的帧**取（同源多 gen 拒收帧不得
    // 用新 body 覆盖旧 gid 的已证观察——旧 skip 不得借矛盾 body 重新藏新题）；
    // 同 gid 首证即冻结（内容 generation 不可变，矛盾 body 属异常换代信号，由
    // bodyConsistent 闸在 hide/start/seed 处解除，不改写观察）；暂缺不抹。
    if (displayAccepted && asyncQuestions !== undefined) {
      for (const entry of frame.displayGroups) {
        const gid = groupIdentityKey(entry.group);
        if (this.observations.has(gid)) continue; // 已证 body 不覆盖
        const members = asyncQuestions.filter(
          (question) =>
            question.sourceTurnId === entry.group.sourceTurnId &&
            question.sourceItemId === entry.group.sourceItemId
        );
        if (members.length === 0) continue; // 暂缺不抹
        const sorted = [...members].sort((a, b) => a.questionIndex - b.questionIndex);
        this.observations.set(
          gid,
          sorted.map((question) => ({
            questionKey: question.questionKey,
            questionIndex: question.questionIndex,
            title: question.title,
            orderedOptions: [...question.options],
          }))
        );
      }
    }
  }

  snapshot(): QuestionSkipFrameFacts {
    return {
      displayByGid: this.display.snapshot(),
      historyByGid: this.history.snapshot(),
      observations: [...this.observations.entries()].map(([gid, bodies]) => ({ gid, bodies })),
      displayInvalid: this.display.isInvalid(),
      historyInvalid: this.history.isInvalid(),
    };
  }
}

/** 分层快照合成（纯函数）：统一决策 = display/history/已受理回执三源按
 *  (revision, 同钟 seq) 择新——旧 skip 不遮新 restore、旧 closed 不遮新 unknown、
 *  旧 display 不压合法 history 更新；descriptor 每 source 唯一 canonical gid
 *  （display 账同源多 gen 已在 ledger 拒收，不猜当前代）。 */
export function buildQuestionSkipFactsSnapshot(
  frameFacts: QuestionSkipFrameFacts,
  receipts: RecordedReceiptFact[]
): QuestionSkipFactsSnapshot {
  const unified = new Map<string, { fact: QuestionSkipUnifiedFact; seq: number }>();
  const consider = (gid: string, fact: QuestionSkipUnifiedFact, seq: number): void => {
    const existing = unified.get(gid);
    if (
      existing === undefined ||
      fact.revision > existing.fact.revision ||
      (fact.revision === existing.fact.revision && seq > existing.seq)
    ) {
      unified.set(gid, { fact, seq });
    }
  };
  for (const entry of frameFacts.displayByGid) {
    consider(
      entry.gid,
      {
        group: entry.view.group,
        decision: entry.view.decision,
        revision: entry.view.revision,
        latestNativeEffect: entry.view.latestNativeEffect,
        reasonCode: entry.view.reasonCode,
      },
      entry.seq
    );
  }
  for (const entry of frameFacts.historyByGid) {
    consider(
      entry.gid,
      {
        group: entry.view.group,
        decision: entry.view.decision,
        revision: entry.view.revision,
        latestNativeEffect: entry.view.latestNativeEffect,
        reasonCode: entry.view.reasonCode,
      },
      entry.seq
    );
  }
  for (const receipt of receipts) {
    consider(
      groupIdentityKey(receipt.group),
      {
        group: receipt.group,
        decision: receipt.decision,
        revision: receipt.revision,
        latestNativeEffect: receipt.nativeEffect,
        reasonCode: receipt.reasonCode,
      },
      receipt.seq
    );
  }
  const descriptorBySource = frameFacts.displayByGid.map((entry) => ({
    sourceTurnId: entry.view.group.sourceTurnId,
    sourceItemId: entry.view.group.sourceItemId,
    gid: entry.gid,
    view: entry.view,
  }));
  return {
    displayByGid: frameFacts.displayByGid,
    historyByGid: frameFacts.historyByGid,
    unifiedByGid: [...unified.entries()]
      .map(([gid, value]) => ({ gid, fact: value.fact, seq: value.seq }))
      .sort((a, b) => a.gid.localeCompare(b.gid)),
    descriptorBySource: descriptorBySource.sort((a, b) =>
      `${a.sourceTurnId}|${a.sourceItemId}`.localeCompare(`${b.sourceTurnId}|${b.sourceItemId}`)
    ),
    observations: frameFacts.observations,
    displayInvalid: frameFacts.displayInvalid,
    historyInvalid: frameFacts.historyInvalid,
  };
}
