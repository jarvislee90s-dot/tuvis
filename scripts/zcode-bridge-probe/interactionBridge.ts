// scripts/zcode-bridge-probe/interactionBridge.ts — B3 客户端侧参照实现：审批 / 问答 /
// 打断的 pending 绑定、单帧应答闭环与只读视图隔离（工作包 B 计划 §B3；不在产品运行链上：
// 不被 src/ 引用、不挂应用启动/扫描）。
//
// wire 形状全部钉在固定官方源码 zai-org/ZCode @ 3c5b227649371507ddc4eb46ac5133afe6db1cab
// 的客户端侧实现（官方云 relay 服务端不在该仓库），逐条行号见各导出注释。决策文件
// docs/superpowers/decisions/2026-10-24-zcode-official-bridge-contract.md §8 为文字版。
//
// 探针红线（工作包 B 全局约束 + B3 计划条目）：
//   - pending 只能来自本会话 conversation topic 的真实帧（snapshot.pendingInteractions /
//     deltas state.updated 键级整体替换），异会话 / 形状不符的帧一律拒收；
//   - 应答只允许携带 pending 卡上的原值（interactionId / optionId / 题干 / 选项 value），
//     绝不虚构字段；fullAccess 特权选项（snapshot.ts:234）被硬闸拒绝——探针永不走
//     resolveFullAccess 特权分支（interaction-background.ts:50-54）；
//   - 应答闸三查（session / generation / 存在性）过闸即消费：错 id、旧代际、已消费、
//     重复应答一律 fail-closed，绝不波及其他 pending；迟到应答没有路径再进网络；
//   - stop 只做有守卫的精确打断：payload 必带 control.activeWorks 观察到的
//     expectedForegroundExecutionId（snapshot.ts:53-64；command.ts:162；session-flow.ts
//     :292-360 守卫不匹配 → 服务端 noop 不误杀新执行）；守卫 noop 不当成功、不补发；
//   - 单帧纪律与 B2 相同：每个动作恰一个 rpc-frame，无重发计时器，unknown 只记账。
//
// 语法纪律：可被 `node`（type-stripping）直接执行——仅可擦除 TS 语法。

import * as crypto from "node:crypto";

import {
  AGENT_CHANNEL,
  AGENT_SEND_COMMAND_METHOD,
  RESPONSE_TYPE_PROMISE_ERROR,
  RESPONSE_TYPE_PROMISE_SUCCESS,
  RpcFrameAssembler,
  decodeChannelMessage,
  encodeChannelRequest,
  encodeRpcFrame,
  type BoundTarget,
  type DecodedChannelMessage,
  type ProbeConnection,
  type RpcFrameIdentity,
} from "./sessionBridge.ts";

// ---------------------------------------------------------------------------
// 官方 wire 常量（行号 @ 固定 SHA 3c5b2276）
// ---------------------------------------------------------------------------

/**
 * v4 命令类型（commandPayloadSchemas 的键，packages/shared/src/zcode-protocol-v4/
 * command.ts:52-293）。B3 面只触达 resolveInteraction（:221-234）/ stop（:161-164）/
 * snoozeInteractionAutoResolution（:250-253）；发送（sendText :95-135）沿用 B2 面这三者都**不在** COMMANDS_REQUIRING_BASE_REVISION
 * （command.ts:315-331）与 ROW_TARGETING_COMMANDS（:333-340）→ 免 baseRevision /
 * baseLogEpoch。
 */
export const INTERACTION_COMMAND_TYPES = {
  resolve: "resolveInteraction",
  stop: "stop",
  snooze: "snoozeInteractionAutoResolution",
} as const;
export type InteractionCommandType =
  | (typeof INTERACTION_COMMAND_TYPES)[keyof typeof INTERACTION_COMMAND_TYPES]
  /** B4：模型/思考强度切换（command.ts:254-258；CAS 必带 baseRevision :315-331）。 */
  | "switchModelConfig";

/**
 * fullAccess 特权选项 id（PERMISSION_FULL_ACCESS_OPTION_ID，snapshot.ts:234）。
 * 命中该 id 的应答在宿主侧走 resolveFullAccess 特权分支
 * （interaction-background.ts:50-54 → interaction-registry.ts:170-191
 * grantPermissionFullAccess）——探针红线：永不触达，工作包 B 计划全局约束
 * 「不使用 full-access／永久授权选项作审批成功捷径」。
 */
export const PERMISSION_FULL_ACCESS_OPTION_ID = "fullAccess";

/**
 * 迟到 stop 的服务端守卫 noop reasonCode（session-flow.ts:292-360：守卫 id 在手而
 * runtime idle / mismatch → V4CommandNoopError("guard.stopTargetChanged")——「继续
 * abort 外层 controller 会误杀用户没看到的新执行。execution id 不匹配只能 noop」）。
 */
export const STOP_GUARD_NOOP_REASON = "guard.stopTargetChanged";

/** 问答应答 action 闭集（command.ts:224-233 answer.action；ElicitationDialog.tsx:27）。 */
export type UserInputAnswerAction = "accept" | "decline" | "cancel";

// ---------------------------------------------------------------------------
// conversation topic 帧（transport.ts:163-191 帧信封 + snapshot.ts:316-355 pendingInteraction）
// ---------------------------------------------------------------------------

/**
 * "conversation/<sessionId>" topic 主体解析（transport.ts:113-114,205-224：topic 必须
 * 以 "conversation/" 开头且后缀非空；其余 topic 一律 null）。
 */
export function decodeTopicFrameSubject(topic: unknown): string | null {
  if (typeof topic !== "string") return null;
  const prefix = "conversation/";
  if (!topic.startsWith(prefix)) return null;
  const subject = topic.slice(prefix.length);
  return subject.length > 0 ? subject : null;
}

/** pending 卡记录：官方 PendingInteraction 原样 payload + 本册绑定四元组。 */
export type ObservedPending = {
  interactionId: string;
  kind: "permission" | "userInput" | "workspaceHookReview";
  /** 册绑定：只收录本会话（envelope.sessionId 命名空间，= BoundTarget.taskId）。 */
  sessionId: string;
  /** 册绑定：观察时的连接代际（跨代际应答在闸上被 stale-epoch 拒绝）。 */
  generation: number;
  /** turn 锚：工具行 id（行携带 turnId，rows.ts:19-20）+ 工具调用 id。 */
  anchorRowId: number | null;
  toolCallId: string | null;
  /**
   * 卡创建时间（snapshot.ts:316-337 PendingInteraction.createdAt，
   * timestampSchema = z.number() 即 epoch ms）。用于触发后关联：只有
   * createdAt >= 触发帧发送时刻的 pending 才属于本次触发（旧卡不得被误应答）；
   * 卡上缺该字段（旧快照/合成夹具）→ null，调用方回退为「触发后出现的首张卡」。
   */
  createdAt: number | null;
  /** AskUserQuestion 自动结束计时是否在场（interaction-registry.ts:30-31）。 */
  autoResolution: boolean;
  /** autoResolution 的 state 字面量（hiddenGrace / visibleCountdown / snoozed）。 */
  autoResolutionState: string | null;
  /** pendingInteraction payload 原样（应答字段只从这里取，绝不虚构）。 */
  payload: Record<string, unknown>;
};

export type PendingBookCounters = {
  topicFramesSeen: number;
  /** 非 conversation/<本会话> 的 topic 帧（含兄弟会话）——拒收计数。 */
  foreignTopicFrames: number;
  /** 形状不符的帧（缺 payload / 坏 kind）——拒收计数。 */
  malformedFrames: number;
  snapshotsApplied: number;
  deltasApplied: number;
  /** 形状校验不过的 pending 行（跳过，不致命）。 */
  invalidPendingSkipped: number;
};

export type TopicFrameAcceptResult = { accepted: boolean; reason: string };

/**
 * PendingBook：本会话 pending 卡的客户端账本。
 *
 * 收录面（唯一入口 acceptTopicFrame）：
 *   - snapshot（transport.ts:163-191 payload.kind="snapshot"）：整体替换为
 *     snapshot.pendingInteractions（snapshot.ts:511 数组必有）；
 *   - deltas（payload.kind="deltas"）：逐条 delta，op="state.updated"（delta.ts:40 注释
 *     「StatePatch：键级整体替换（Object.assign），键内绝不深合并」；op 字面量
 *     delta.ts:120）时按 patch.pendingInteractions
 *     整体替换（product-projection.ts:3364-3382,3563-3576 的 delta 形态）、
 *     patch.control 记为本会话 control 最新值（sessionControlSchema，snapshot.ts:133-148，
 *     activeWorks[].foregroundExecutionId = stop 守卫 id 的客户端可见来源
 *     snapshot.ts:53-64）。
 * 非 conversation/<本会话> 的帧（兄弟会话 / sessions-index 等）只计拒收，绝不进册。
 */
export class PendingBook {
  readonly counters: PendingBookCounters = {
    topicFramesSeen: 0,
    foreignTopicFrames: 0,
    malformedFrames: 0,
    snapshotsApplied: 0,
    deltasApplied: 0,
    invalidPendingSkipped: 0,
  };

  private readonly sessionId: string;
  private readonly generation: number;
  private readonly pendings = new Map<string, ObservedPending>();
  /** 已消费（authorizePendingResolution 过闸即消费）——重复应答给出精确的 already-consumed。 */
  private readonly consumedIds = new Set<string>();
  private lastControl: Record<string, unknown> | null = null;
  /**
   * 宿主最近一次宣告的 pending id 集合（消费前快照）。本地 `consume()` 只改本地册，
   * 不改这里——「已过三查闸」不等于「宿主已把卡撤掉」。终态确认必须看宿主集合。
   */
  private lastHostPendingIds = new Set<string>();
  /** 宿主 pending 集合的观察代数：每次 snapshot / pendingInteractions patch +1。 */
  private hostPendingRevisionCounter = 0;
  /**
   * control 观察代数：每次收到带 control 的 snapshot / state.updated patch +1。
   * interrupt 关联（I1）用它做基线——只有**触发帧发出之后**收到的 control 更新
   * 观察到的 stop 目标才可被采用（触发前的旧执行 id 不得被误停）。
   */
  private controlRevisionCounter = 0;
  /** 本会话最近观察到的 conversation topic（日志/证据用；恒等于绑定 sessionId）。 */
  lastTopicObserved = false;

  constructor(args: { sessionId: string; generation: number }) {
    if (typeof args.sessionId !== "string" || args.sessionId.length === 0) {
      throw new Error("pending-book: sessionId must be a non-empty exact id");
    }
    if (!Number.isInteger(args.generation) || args.generation < 1) {
      throw new Error("pending-book: generation must be a positive integer");
    }
    this.sessionId = args.sessionId;
    this.generation = args.generation;
  }

  get boundSessionId(): string {
    return this.sessionId;
  }

  get boundGeneration(): number {
    return this.generation;
  }

  /** 最近一次观察到的 control 状态（stop 守卫 id 来源；未观察 = null）。 */
  get control(): Record<string, unknown> | null {
    return this.lastControl;
  }

  /** 宿主 pending 集合的观察代数（ACK 前后各取一次，用来证明收到过新帧）。 */
  get hostPendingRevision(): number {
    return this.hostPendingRevisionCounter;
  }

  /**
   * 宿主最近一次宣告里是否仍有该卡。应答 ACK 后必须等到这里变 false（且代数前进）
   * 才算终态清除；本地 consume 不参与判断。
   */
  hostHasPending(interactionId: string): boolean {
    return this.lastHostPendingIds.has(interactionId);
  }

  /** control.activeWorks 里是否仍有该 foregroundExecutionId（stop 终态确认用）。 */
  hasForegroundExecution(executionId: string): boolean {
    if (!this.lastControl) return false;
    const activeWorks = (this.lastControl as { activeWorks?: unknown }).activeWorks;
    if (!Array.isArray(activeWorks)) return false;
    return activeWorks.some((work) => {
      if (!work || typeof work !== "object") return false;
      return (work as { foregroundExecutionId?: unknown }).foregroundExecutionId === executionId;
    });
  }

  /** control 观察代数（触发前取基线，observeStopTarget 只认基线之后的新值）。 */
  get controlRevision(): number {
    return this.controlRevisionCounter;
  }

  /**
   * 入站帧喂入口：只吃 conversation/<本会话> 的 topic frame（结构 = decodeChannelMessage
   * 解出的 EventFire body）。返回 accept 结果与原因（证据落账用）。
   */
  acceptTopicFrame(frame: unknown): TopicFrameAcceptResult {
    this.counters.topicFramesSeen += 1;
    if (!frame || typeof frame !== "object") {
      this.counters.malformedFrames += 1;
      return { accepted: false, reason: "malformed-frame" };
    }
    const record = frame as { topic?: unknown; payload?: unknown };
    const subject = decodeTopicFrameSubject(record.topic);
    if (subject !== this.sessionId) {
      this.counters.foreignTopicFrames += 1;
      return { accepted: false, reason: "foreign-topic" };
    }
    this.lastTopicObserved = true;
    const payload = record.payload as
      { kind?: unknown; snapshot?: unknown; deltas?: unknown } | undefined;
    if (!payload || typeof payload.kind !== "string") {
      this.counters.malformedFrames += 1;
      return { accepted: false, reason: "malformed-payload" };
    }
    if (payload.kind === "snapshot") {
      const snapshot = payload.snapshot as { pendingInteractions?: unknown; control?: unknown };
      if (!snapshot || typeof snapshot !== "object") {
        this.counters.malformedFrames += 1;
        return { accepted: false, reason: "malformed-snapshot" };
      }
      if (snapshot.control && typeof snapshot.control === "object") {
        this.lastControl = snapshot.control as Record<string, unknown>;
        this.controlRevisionCounter += 1;
      }
      const incoming = Array.isArray(snapshot.pendingInteractions)
        ? snapshot.pendingInteractions
        : [];
      this.replacePendings(incoming);
      this.counters.snapshotsApplied += 1;
      return { accepted: true, reason: "snapshot" };
    }
    if (payload.kind === "deltas") {
      if (!Array.isArray(payload.deltas)) {
        this.counters.malformedFrames += 1;
        return { accepted: false, reason: "malformed-deltas" };
      }
      let touched = false;
      for (const delta of payload.deltas) {
        if (!delta || typeof delta !== "object") continue;
        const record2 = delta as { op?: unknown; patch?: unknown };
        if (record2.op !== "state.updated") continue;
        if (!record2.patch || typeof record2.patch !== "object") continue;
        const patch = record2.patch as { pendingInteractions?: unknown; control?: unknown };
        if (patch.control && typeof patch.control === "object") {
          this.lastControl = patch.control as Record<string, unknown>;
          this.controlRevisionCounter += 1;
          touched = true;
        }
        if (Array.isArray(patch.pendingInteractions)) {
          this.replacePendings(patch.pendingInteractions);
          touched = true;
        }
      }
      if (touched) this.counters.deltasApplied += 1;
      return { accepted: true, reason: touched ? "deltas" : "deltas-untouched" };
    }
    this.counters.malformedFrames += 1;
    return { accepted: false, reason: "unknown-payload-kind" };
  }

  find(interactionId: string): ObservedPending | null {
    return this.pendings.get(interactionId) ?? null;
  }

  list(): ObservedPending[] {
    return [...this.pendings.values()];
  }

  get size(): number {
    return this.pendings.size;
  }

  /** 本地消费（authorizePendingResolution 过闸后调用；测试也可直接驱动）。 */
  consume(interactionId: string): ObservedPending {
    const pending = this.pendings.get(interactionId);
    if (!pending) {
      if (this.consumedIds.has(interactionId)) {
        throw new Error(`already-consumed: interaction ${interactionId} was already resolved`);
      }
      throw new Error(`unknown-interaction: no pending interaction ${interactionId} in this book`);
    }
    this.pendings.delete(interactionId);
    this.consumedIds.add(interactionId);
    return pending;
  }

  /**
   * stop 守卫 id 观察：control.activeWorks 里取 foregroundExecutionId（snapshot.ts:
   * 53-64 ActiveWorkSummary{kind, foregroundExecutionId?}）。只有 primaryTurn 条目
   * 可作为目标（前台子 agent / compact 等不是本轮 turn 的守卫点，宁不 stop 不误杀）。
   * afterRevision = 触发帧发出前的 controlRevision 基线：control 观察代数未前进
   * （没有触发后新 control）→ null——触发前的旧执行 id 绝不可被采用。
   * 未观察到 → null（调用方必须 fail-closed：没有守卫 id 就不做 stop）。
   */
  observeStopTarget(options?: {
    afterRevision?: number;
  }): { kind: string; foregroundExecutionId: string } | null {
    if (
      options?.afterRevision !== undefined &&
      this.controlRevisionCounter <= options.afterRevision
    ) {
      return null;
    }
    if (!this.lastControl) return null;
    const activeWorks = (this.lastControl as { activeWorks?: unknown }).activeWorks;
    if (!Array.isArray(activeWorks)) return null;
    for (const work of activeWorks) {
      if (!work || typeof work !== "object") continue;
      const record = work as { kind?: unknown; foregroundExecutionId?: unknown };
      if (typeof record.foregroundExecutionId !== "string" || !record.foregroundExecutionId) {
        continue;
      }
      if (record.kind === "primaryTurn") {
        return { kind: "primaryTurn", foregroundExecutionId: record.foregroundExecutionId };
      }
    }
    return null;
  }

  /** 整体替换（键级语义：不深合并，delta.ts:8）；行形状校验不过的跳过计数。 */
  private replacePendings(incoming: unknown[]): void {
    // 先记宿主宣告的 id 集合（含已本地消费的 id）：这是终态清除的唯一 wire 证据。
    this.lastHostPendingIds = new Set(
      incoming
        .map((candidate) =>
          candidate && typeof candidate === "object"
            ? (candidate as { interactionId?: unknown }).interactionId
            : null
        )
        .filter((id): id is string => typeof id === "string" && id.length > 0)
    );
    this.hostPendingRevisionCounter += 1;
    this.pendings.clear();
    for (const candidate of incoming) {
      if (!candidate || typeof candidate !== "object") {
        this.counters.invalidPendingSkipped += 1;
        continue;
      }
      const record = candidate as {
        interactionId?: unknown;
        kind?: unknown;
        anchorRowId?: unknown;
        createdAt?: unknown;
        autoResolution?: unknown;
        payload?: unknown;
      };
      const interactionId = record.interactionId;
      const kind = record.kind;
      const payload = record.payload;
      if (
        typeof interactionId !== "string" ||
        interactionId.length === 0 ||
        (kind !== "permission" && kind !== "userInput" && kind !== "workspaceHookReview") ||
        !payload ||
        typeof payload !== "object" ||
        (payload as { kind?: unknown }).kind !== kind
      ) {
        // kind 与 payload.kind 必须一致（snapshot.ts:329-337 superRefine 同款判据）。
        this.counters.invalidPendingSkipped += 1;
        continue;
      }
      // 已消费的 interactionId 不得被后续 snapshot 复活（I2：迟到的快照仍可能携带
      // 已应答过的卡；重新入册会让同一 interaction 被二次应答）。
      if (this.consumedIds.has(interactionId)) {
        continue;
      }
      const payloadRecord = payload as Record<string, unknown>;
      const toolCallId =
        typeof payloadRecord.toolCallId === "string" ? payloadRecord.toolCallId : null;
      const anchorRowId = typeof record.anchorRowId === "number" ? record.anchorRowId : null;
      const autoResolutionRecord =
        record.autoResolution && typeof record.autoResolution === "object"
          ? (record.autoResolution as { state?: unknown })
          : null;
      this.pendings.set(interactionId, {
        interactionId,
        kind,
        sessionId: this.sessionId,
        generation: this.generation,
        anchorRowId,
        toolCallId,
        createdAt: typeof record.createdAt === "number" ? record.createdAt : null,
        autoResolution: autoResolutionRecord !== null,
        autoResolutionState:
          typeof autoResolutionRecord?.state === "string" ? autoResolutionRecord.state : null,
        payload: payloadRecord,
      });
    }
  }
}

// ---------------------------------------------------------------------------
// 应答闸：authorizePendingResolution（发送前唯一入口）
// ---------------------------------------------------------------------------

export type PendingResolutionGateArgs = {
  /** 应答信封的 sessionId（= 发送目标 taskId）；必须与本册绑定一致。 */
  sessionId: string;
  /** 当前存活连接代际；必须与本册绑定代际一致（like-for-like，同 B2 mapper.authorizeSend）。 */
  liveGeneration: number;
};

/**
 * 三查闸 + 消费：存在性（unknown-interaction）→ 会话一致（session-mismatch）→
 * 代际一致（stale-epoch）→ 过闸即消费（already-consumed 幂次保护）。
 * 顺序刻意如此：错会话/错代际的尝试即便 id 存在也绝不消费——迟到应答在客户端侧
 * 就没有路径进网络（服务端幂等 only 已是最后防线，interaction-registry.ts:155-167）。
 */
export function authorizePendingResolution(
  book: PendingBook,
  interactionId: string,
  args: PendingResolutionGateArgs
): ObservedPending {
  if (args.sessionId !== book.boundSessionId) {
    throw new Error(
      `session-mismatch: resolution targets session ${args.sessionId}, book is bound to ${book.boundSessionId}`
    );
  }
  if (args.liveGeneration !== book.boundGeneration) {
    throw new Error(
      `stale-epoch: pending book bound to generation ${book.boundGeneration}, live connection is ${args.liveGeneration}`
    );
  }
  if (book.find(interactionId) === null) {
    // 触发 consume 的精确错误（already-consumed / unknown-interaction）。
    book.consume(interactionId);
  }
  return book.consume(interactionId);
}

// ---------------------------------------------------------------------------
// 应答构造器（原值回传；绝不虚构字段）
// ---------------------------------------------------------------------------

/**
 * 权限应答：{optionId, freeText?}（官方 UI 的 onRespond 提交形状，
 * V4InteractionDialogs.tsx:358-363）。fullAccess 选项在客户端侧硬闸拒绝——
 * 探针永不走宿主 resolveFullAccess 特权分支（interaction-background.ts:50-54）。
 */
export function buildPermissionAnswer(args: { optionId: string; freeText?: string }): {
  optionId: string;
  freeText?: string;
} {
  assertNotFullAccessOption(args.optionId);
  if (typeof args.optionId !== "string" || args.optionId.length === 0) {
    throw new Error("invalid-answer: optionId must be a non-empty string from the pending card");
  }
  return {
    optionId: args.optionId,
    ...(typeof args.freeText === "string" && args.freeText.length > 0
      ? { freeText: args.freeText }
      : {}),
  };
}

/** 硬闸：fullAccess 特权选项（PERMISSION_FULL_ACCESS_OPTION_ID，snapshot.ts:234）。 */
export function assertNotFullAccessOption(optionId: string): void {
  if (optionId === PERMISSION_FULL_ACCESS_OPTION_ID) {
    throw new Error(
      "full-access-forbidden: probe must never answer the fullAccess privileged option " +
        "(work-package B global constraint; host-side branch = interaction-background.ts:50-54)"
    );
  }
}

export type UserInputAnswerEntry = {
  /** 题干全文（content.answers 的键；broker 以题干全文回对答案槽位，
   * interaction-broker.ts:622-650）。 */
  questionText: string;
  /** 选中的选项 value 原值（AskUserQuestion 的 value = label，
   * interaction-broker.ts:505-517）。 */
  selectedValues: string[];
  multiSelect: boolean;
};

/**
 * 问答（AskUserQuestion / elicitation）应答：{action:"accept", content}。
 * content 形状 = buildElicitationResponseContent（ElicitationDialog.tsx:273-303）：
 *   - answers：{[题干全文]: 选中 value 用 ", " 连接}（多选数组由 broker
 *     normalizeAnswerValue 归一同形，interaction-broker.ts:766-780）；
 *   - answer_<index>：多选 = value 数组、单选 = 首个 value（同文件 :287-291）；
 *   - answer：仅单题时附（旧 agent 读取路径兼容，同文件 :293-300）；
 *   - 全部为空时只提交 {answers:{}}（「部分或空 answers 由 Agent 使用最佳判断继续」，
 *     同文件 :277-278——不用空字符串伪造偏好）。
 */
export function buildUserInputAnswer(entries: UserInputAnswerEntry[]): {
  action: "accept";
  content: Record<string, unknown>;
} {
  const answers: Record<string, string> = {};
  entries.forEach((entry) => {
    if (entry.selectedValues.length > 0) {
      answers[entry.questionText] = entry.selectedValues.join(", ");
    }
  });
  const content: Record<string, unknown> = { answers };
  entries.forEach((entry, index) => {
    if (entry.selectedValues.length > 0) {
      content[`answer_${index}`] = entry.multiSelect
        ? entry.selectedValues
        : entry.selectedValues[0];
    }
  });
  if (entries.length === 1 && entries[0]!.selectedValues.length > 0) {
    const only = entries[0]!;
    content.answer = only.multiSelect ? only.selectedValues : only.selectedValues[0];
  }
  return { action: "accept", content };
}

/** 问答拒绝：{action:"decline"}（ElicitationDialog.tsx:529 的取消路径同族）。 */
export function buildDeclineAnswer(): { action: "decline" } {
  return { action: "decline" };
}

/**
 * stop payload：{expectedForegroundExecutionId}（command.ts:161-164）。守卫 id 只允许
 * 来自 control.activeWorks 的观察（PendingBook.observeStopTarget）；缺席 = 盲停窗口
 * 不可控 → fail-closed 拒绝构造（源码允许缺省，探针纪律收紧为必带）。
 */
export function buildStopPayload(target: { kind: string; foregroundExecutionId: string } | null): {
  expectedForegroundExecutionId: string;
} {
  if (
    !target ||
    typeof target.foregroundExecutionId !== "string" ||
    !target.foregroundExecutionId
  ) {
    throw new Error(
      "stop-requires-foreground-execution: no foregroundExecutionId observed in " +
        "control.activeWorks; refusing to blind-stop (session-flow.ts:292-360 guard semantics)"
    );
  }
  return { expectedForegroundExecutionId: target.foregroundExecutionId };
}

// ---------------------------------------------------------------------------
// 单帧应答 / 打断闭环（InteractionCommandLoop——SendOnceLoop 的 B3 同族）
// ---------------------------------------------------------------------------

export type InteractionOutcome =
  | { kind: "accepted"; ackStatus: string }
  | { kind: "noop"; reasonCode: string | null }
  | { kind: "rejected"; reasonCode: string | null }
  | { kind: "send-failed" }
  | { kind: "unknown" };

export type InteractionCommandLoopOptions = {
  sender: ProbeConnection;
  identity: RpcFrameIdentity;
  /** 发送目标（B2 精确归属产物）：envelope.sessionId = target.taskId。 */
  target: Pick<BoundTarget, "taskId" | "workspacePath" | "workspaceIdentity">;
  commandType: InteractionCommandType;
  /** CAS：switchModelConfig 必带（command.ts:315-331,343-357）；其他命令不传。 */
  baseRevision?: number;
  /**
   * resolveInteraction → {interactionId, answer}（command.ts:221-234）；
   * stop → {expectedForegroundExecutionId}（:161-164，必带——run() 开头强制）；
   * snooze → {interactionId}（:250-253）。
   */
  payload: Record<string, unknown>;
  clientId: string;
  /** CommandAck 有界等待窗；超窗 = unknown，绝不重发。 */
  ackMs: number;
  sleep?: (ms: number) => Promise<void>;
  now?: () => number;
  /** bridge rpc 通道上的请求 id 与起始 seq/messageSeq（接续 hello/subscribe 已占用的序号）。 */
  channelRequestId?: number;
  firstSeq?: number;
  firstMessageSeq?: number;
};

/** 单帧闭环的一个步骤（命令类型 + 原样 payload）。 */
export type InteractionCommandStep = {
  commandType: InteractionCommandType;
  payload: Record<string, unknown>;
};

/**
 * 应答命令序列（I4）：卡上 autoResolution 在场 → 先 snoozeInteractionAutoResolution
 * （{interactionId}，command.ts:250-253：首次有效操作永久暂停本次自动结束；重复/迟到
 * 幂等 noop）再 resolve；否则只有 resolve。顺序即发送顺序，每步各恰一帧、无重发。
 * 调用方按步骤逐个跑 InteractionCommandLoop（snooze 失败只记录、继续 resolve：
 * resolve 本身先到先得，自动结束已收口时回 proto.alreadyResolved noop）。
 */
export function planInteractionCommands(args: {
  pending: Pick<ObservedPending, "interactionId" | "autoResolution">;
  commandType: InteractionCommandType;
  payload: Record<string, unknown>;
}): InteractionCommandStep[] {
  const steps: InteractionCommandStep[] = [];
  if (args.pending.autoResolution) {
    steps.push({
      commandType: INTERACTION_COMMAND_TYPES.snooze,
      payload: { interactionId: args.pending.interactionId },
    });
  }
  steps.push({ commandType: args.commandType, payload: args.payload });
  return steps;
}

/**
 * resolveInteraction / stop / snooze 的单帧闭环。全生命周期恰好一个 rpc-frame；
 * 等待循环只有 sleep+判定，**没有任何重发路径**。CommandAck 分类（command.ts:456
 * status 枚举）：
 *   accepted | duplicate → {kind:"accepted"}（duplicate = 回放缓存结果，等价收口）；
 *   noop                 → {kind:"noop"}（如 guard.stopTargetChanged——不是成功）；
 *   rejected|stale|failed → {kind:"rejected"}；PromiseError → rejected(message)；
 *   超窗                  → unknown（peer 受理与否只看 rpc-frame-ack）。
 */
export class InteractionCommandLoop {
  /** 生命周期内构造的 rpc-frame 数：结构性恒 ≤1。 */
  framesSent = 0;
  /** 组装器判定的重复入站帧数。 */
  duplicateInboundFrames = 0;
  /** peer 是否曾受理我们的 rpc-frame（rpc-frame-ack 到达过；仅供证据记录）。 */
  peerAcknowledged = false;

  private readonly sender: ProbeConnection;
  private readonly identity: RpcFrameIdentity;
  private readonly target: Pick<BoundTarget, "taskId" | "workspacePath" | "workspaceIdentity">;
  private readonly commandType: InteractionCommandType;
  private readonly baseRevision?: number;
  private readonly payload: Record<string, unknown>;
  private readonly clientId: string;
  private readonly ackMs: number;
  private readonly sleep: (ms: number) => Promise<void>;
  private readonly now: () => number;
  private readonly assembler: RpcFrameAssembler;
  private readonly channelRequestId: number;
  private readonly firstSeq: number;
  private readonly firstMessageSeq: number;
  private settledResponse:
    | { status: "accepted"; ackStatus: string }
    | { status: "noop"; reasonCode: string | null }
    | { status: "rejected"; reasonCode: string | null }
    | null = null;
  private readonly requestId: number;
  private commandId = "";
  private lastPeerAckSeq = 0;

  constructor(options: InteractionCommandLoopOptions) {
    this.sender = options.sender;
    this.identity = options.identity;
    this.target = options.target;
    this.commandType = options.commandType;
    this.baseRevision = options.baseRevision;
    this.payload = options.payload;
    this.clientId = options.clientId;
    this.ackMs = options.ackMs;
    this.sleep = options.sleep ?? realSleepDefault;
    this.now = options.now ?? Date.now;
    this.assembler = new RpcFrameAssembler({ identity: options.identity });
    this.channelRequestId = options.channelRequestId ?? 1;
    this.firstSeq = options.firstSeq ?? 1;
    this.firstMessageSeq = options.firstMessageSeq ?? 1;
    this.requestId = this.channelRequestId;
  }

  async run(): Promise<InteractionOutcome> {
    // 探针纪律：stop 必带守卫 id（源码允许缺省 = 无守卫盲停；探针只做有守卫的精确打断）。
    if (this.commandType === INTERACTION_COMMAND_TYPES.stop) {
      const guard = this.payload.expectedForegroundExecutionId;
      if (typeof guard !== "string" || guard.length === 0) {
        throw new Error(
          "stop-requires-foreground-execution: expectedForegroundExecutionId must be " +
            "observed from control.activeWorks before stop (session-flow.ts:292-360)"
        );
      }
    }
    const envelope = {
      commandId: crypto.randomUUID(),
      clientId: this.clientId,
      sessionId: this.target.taskId,
      ...(this.baseRevision !== undefined ? { baseRevision: this.baseRevision } : {}),
      type: this.commandType,
      payload: this.payload,
      issuedAt: this.now(),
    };
    this.commandId = envelope.commandId;
    const params: Record<string, unknown> = {
      workspacePath: this.target.workspacePath,
      ...(this.target.workspaceIdentity !== undefined
        ? { workspaceIdentity: this.target.workspaceIdentity }
        : {}),
      envelope,
    };
    const bytes = encodeChannelRequest({
      id: this.requestId,
      channel: AGENT_CHANNEL,
      method: AGENT_SEND_COMMAND_METHOD,
      params,
    });
    const frame = encodeRpcFrame({
      identity: this.identity,
      seq: this.firstSeq,
      messageSeq: this.firstMessageSeq,
      bytes,
    });
    this.framesSent = 1; // 恒 1：下面再无任何发送语句
    const sent = this.sender.sendData(frame);
    if (!sent) {
      return { kind: "send-failed" };
    }
    // ACK 窗口（有界；只等待、不重发）。
    const deadline = this.now() + this.ackMs;
    while (this.settledResponse === null && this.now() < deadline) {
      await this.sleep(Math.min(20, Math.max(1, deadline - this.now())));
    }
    const settled = this.settledResponse;
    if (settled === null) return { kind: "unknown" };
    if (settled.status === "accepted") return { kind: "accepted", ackStatus: settled.ackStatus };
    if (settled.status === "noop") return { kind: "noop", reasonCode: settled.reasonCode };
    return { kind: "rejected", reasonCode: settled.reasonCode };
  }

  /** 入站 data payload 喂入口（对接 conn.onData / channel.onRpcFrame 单槽）。 */
  onData(payload: unknown): void {
    if (!payload || typeof payload !== "object") return;
    const record = payload as { zcode_type?: unknown; ackMessageSeq?: unknown };
    if (record.zcode_type === "rpc-frame-ack") {
      const ackSeq = typeof record.ackMessageSeq === "number" ? record.ackMessageSeq : 0;
      if (ackSeq <= this.lastPeerAckSeq) {
        this.duplicateInboundFrames += 1;
        return;
      }
      this.lastPeerAckSeq = ackSeq;
      this.peerAcknowledged = true;
      return;
    }
    if (record.zcode_type !== "rpc-frame") return;
    const event = this.assembler.accept(payload);
    if (event.kind === "duplicate") {
      this.duplicateInboundFrames += 1;
      return;
    }
    if (event.kind === "fault" || event.kind === "incomplete") return;
    const decoded: DecodedChannelMessage | null = decodeChannelMessage(event.bytes);
    if (
      decoded &&
      (decoded.headerType === RESPONSE_TYPE_PROMISE_SUCCESS ||
        decoded.headerType === RESPONSE_TYPE_PROMISE_ERROR) &&
      decoded.id === this.requestId
    ) {
      if (this.settledResponse !== null) {
        this.duplicateInboundFrames += 1;
        return;
      }
      if (decoded.headerType === RESPONSE_TYPE_PROMISE_ERROR) {
        this.settledResponse = { status: "rejected", reasonCode: firstMessage(decoded.body) };
        return;
      }
      const ack = decoded.body as { status?: unknown; reasonCode?: unknown } | undefined;
      const status = typeof ack?.status === "string" ? ack.status : "";
      if (status === "accepted" || status === "duplicate") {
        this.settledResponse = { status: "accepted", ackStatus: status };
      } else if (status === "noop") {
        this.settledResponse = {
          status: "noop",
          reasonCode: typeof ack?.reasonCode === "string" ? ack.reasonCode : null,
        };
      } else {
        this.settledResponse = {
          status: "rejected",
          reasonCode: typeof ack?.reasonCode === "string" ? ack.reasonCode : status || null,
        };
      }
    }
  }

  /** 供证据/调试：本循环发出的命令 id（uuid）。 */
  get sentCommandId(): string {
    return this.commandId;
  }
}

function firstMessage(body: unknown): string | null {
  if (body && typeof body === "object") {
    const record = body as { message?: unknown };
    if (typeof record.message === "string") return record.message;
  }
  return null;
}

const realSleepDefault = (ms: number): Promise<void> =>
  new Promise((resolve) => setTimeout(resolve, ms));
