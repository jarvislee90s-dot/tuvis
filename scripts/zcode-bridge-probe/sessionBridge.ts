// scripts/zcode-bridge-probe/sessionBridge.ts — B2 客户端侧参照实现：单连接闸、
// 会话映射与单次发送闭环（工作包 B 计划 §B2；不在产品运行链上：不被 src/ 引用、
// 不挂应用启动/扫描）。
//
// wire 形状全部钉在固定官方源码 zai-org/ZCode @ 3c5b227649371507ddc4eb46ac5133afe6db1cab
// 的客户端侧实现（官方云 relay 服务端不在该仓库），逐条行号见各导出注释。决策文件
// docs/superpowers/decisions/2026-10-24-zcode-official-bridge-contract.md §7 为文字版。
//
// 探针红线（工作包 B 全局约束）：
//   - 单连接：SingleConnectionGate 是「同一 Tuvis 主机只允许一条 paired 上游连接」的
//     本地强制点——第二个连接请求在构造 socket 之前即被拒绝；
//   - 精确归属：发送目标只能来自「当前连接代际 listing 里精确存在的 taskId +
//     workspacePath」；**本模块没有任何按标题/cwd 查找会话的 API**（标题只能作为
//     操作员 designate 阶段的人读指称，归因永远用 listing 里的精确 id）；
//   - 代际绑定：映射绑定到解析时的连接代际（generation），换代后旧映射不得授权发送
//     （stale-epoch），必须用新代际 listing 重新解析；
//   - 不重发：SendOnceLoop 全生命周期只构造一个 rpc-frame；peer 受理（rpc-frame-ack）
//     后丢响应 → 结果记 unknown，**没有任何重发/退避计时器**；重复 ack / 重复事件
//     全部去重，不产生第二条发送。
//
// 语法纪律：可被 `node`（type-stripping）直接执行——仅可擦除 TS 语法。

import * as crypto from "node:crypto";

// ---------------------------------------------------------------------------
// 官方 wire 常量（行号 @ 固定 SHA 3c5b2276）
// ---------------------------------------------------------------------------

/** v4 wire 协议版本（packages/shared/src/zcode-protocol-v4/core.ts:7）。 */
export const V4_WIRE_PROTOCOL_VERSION = 3 as const;
/**
 * v4 传输限额（core.ts:64-66）：物理帧（rpc-frame 信封 JSON）上限 1 MiB。
 * 本参照实现只做单分片发送，消息字节预算据此收紧。
 */
export const PROTOCOL_V4_MAX_FRAME_BYTES = 1_048_576;
/** agent 服务 channel 名（packages/shared/src/channels.ts:114；描述符 zcodeAgent.ts:1029）。 */
export const AGENT_CHANNEL = "zcode-agent";
/** 握手两步（packages/services/src/zcode-agent/zcodeAgent.ts:881,883）。 */
export const AGENT_HELLO_METHOD = "helloConversationV4";
export const AGENT_CLIENT_HELLO_METHOD = "initializeConversationV4";
/** 发送命令方法（zcodeAgent.ts:937）。 */
export const AGENT_SEND_COMMAND_METHOD = "sendConversationCommandV4";
/** rpc 事件订阅请求类型（packages/rpc/src/channels.shared.ts:18-23）。 */
export const REQUEST_TYPE_EVENT_LISTEN = 102;
/**
 * 窗口 Host controller 服务频道与其任务索引订阅面（@ 3c5b2276）：
 * 频道名 `window-controller`（packages/shared/src/channels.ts:112
 * ServiceChannels.WindowController）；方法 `subscribeControllerV4` /
 * `unsubscribeControllerV4`、动态帧事件 `onDynamicControllerFrame`
 * （packages/services/src/window-controller/windowController.ts:75-78）。官方调用面 =
 * 桌面/手机任务列表 registry：先挂帧监听再订阅
 * （packages/ui/src/v4/windowControllerTaskListRegistry.ts:145,220-224；手机任务首页
 * 经 packages/ui/src/hooks/useGlobalTaskList.ts:45-47 消费同一 registry）。该服务只在
 * workspace bridge 的 relay 通道上发布（packages/client/src/remoteServiceAccess.ts
 * :181-185；websocket.ts:102-108；packages/web/src/web-remote-control/app/
 * webRemoteControlWorkspaceBridgeSession.ts:39-74）→ 枚举前必须先 workspace-bridge-open。
 */
export const WINDOW_CONTROLLER_CHANNEL = "window-controller";
export const CONTROLLER_SUBSCRIBE_METHOD = "subscribeControllerV4";
export const CONTROLLER_UNSUBSCRIBE_METHOD = "unsubscribeControllerV4";
/** 动态帧事件的 wire 事件名 = 属性名本体（packages/rpc/src/proxy-channel.ts:126-127）。 */
export const CONTROLLER_DYNAMIC_FRAME_EVENT = "onDynamicControllerFrame";
/** controller 订阅 topic（packages/shared/src/zcode-protocol-v4/controller.ts:9）。 */
export const CONTROLLER_TASKS_INDEX_TOPIC = "controller/tasks-index";
/** ControllerClient 请求/响应类型（packages/rpc/src/channels.shared.ts:13-23）。 */
export const REQUEST_TYPE_PROMISE = 100;
export const RESPONSE_TYPE_PROMISE_SUCCESS = 201;
export const RESPONSE_TYPE_PROMISE_ERROR = 202;
export const RESPONSE_TYPE_EVENT_FIRE = 204;

// ---------------------------------------------------------------------------
// 二进制序列化（packages/rpc/src/serialization.ts 的最小往返实现）
//
// 每条消息 = serialize(header) + serialize(body)：
//   请求 header = [RequestType, id, channelName, methodName]
//   （serialization.ts:169-175；channelClient.ts:182-190：serialize(writer,
//   [type, id, channelName, name]); serialize(writer, arg)，arg = 方法参数数组）
//   应答 header = [ResponseType, id]（channelClient.ts:199-202 对偶）。
// 格式：[1 字节类型标签][VQL 长度][数据]；标签：Undefined=0 / String=1 / Array=4 /
// Object=5(JSON) / Int=6(VQL)（serialization.ts:110-118）。
// ---------------------------------------------------------------------------

function writeVql(out: number[], value: number): void {
  if (value === 0) {
    out.push(0);
    return;
  }
  let v = value >>> 0;
  for (;;) {
    let byte = v & 0b01111111;
    v = v >>> 7;
    if (v > 0) byte |= 0b10000000;
    out.push(byte);
    if (v === 0) return;
  }
}

function serializeValue(value: unknown): Uint8Array {
  const out: number[] = [];
  const push = (chunk: Uint8Array) => {
    for (const byte of chunk) out.push(byte);
  };
  if (value === undefined) {
    out.push(0);
  } else if (typeof value === "number" && Number.isInteger(value)) {
    out.push(6);
    writeVql(out, value);
  } else if (typeof value === "string") {
    const buffer = new TextEncoder().encode(value);
    out.push(1);
    writeVql(out, buffer.byteLength);
    push(buffer);
  } else if (Array.isArray(value)) {
    out.push(4);
    writeVql(out, value.length);
    for (const item of value) push(serializeValue(item));
  } else {
    const buffer = new TextEncoder().encode(JSON.stringify(value));
    out.push(5);
    writeVql(out, buffer.byteLength);
    push(buffer);
  }
  return Uint8Array.from(out);
}

function readVql(bytes: Uint8Array, pos: number): { value: number; pos: number } | null {
  let value = 0;
  let shift = 0;
  let cursor = pos;
  for (;;) {
    if (cursor >= bytes.byteLength) return null;
    const byte = bytes[cursor];
    cursor += 1;
    value += (byte & 0b01111111) * Math.pow(2, shift);
    if ((byte & 0b10000000) === 0) return { value, pos: cursor };
    shift += 7;
  }
}

function deserializeValue(bytes: Uint8Array, pos: number): { value: unknown; pos: number } | null {
  if (pos >= bytes.byteLength) return null;
  const tag = bytes[pos];
  let cursor = pos + 1;
  if (tag === 0) return { value: undefined, pos: cursor };
  if (tag === 6) {
    const vql = readVql(bytes, cursor);
    return vql ? { value: vql.value, pos: vql.pos } : null;
  }
  if (tag === 1 || tag === 2 || tag === 3 || tag === 5) {
    const length = readVql(bytes, cursor);
    if (!length) return null;
    const slice = bytes.subarray(length.pos, length.pos + length.value);
    if (slice.byteLength !== length.value) return null;
    const text = Buffer.from(slice).toString("utf8");
    return { value: tag === 5 ? JSON.parse(text) : text, pos: length.pos + length.value };
  }
  if (tag === 4) {
    const length = readVql(bytes, cursor);
    if (!length) return null;
    cursor = length.pos;
    const items: unknown[] = [];
    for (let i = 0; i < length.value; i += 1) {
      const item = deserializeValue(bytes, cursor);
      if (!item) return null;
      items.push(item.value);
      cursor = item.pos;
    }
    return { value: items, pos: cursor };
  }
  return null;
}

export type DecodedChannelMessage = {
  headerType: number;
  /** 请求含 channelName/method；应答为 null。 */
  channelName: string | null;
  method: string | null;
  id: number | null;
  body: unknown;
};

/** 解码一条 Channel 消息字节（请求或应答）；形状不符返回 null。 */
export function decodeChannelMessage(bytes: Uint8Array): DecodedChannelMessage | null {
  const header = deserializeValue(bytes, 0);
  if (!header || !Array.isArray(header.value) || header.value.length < 2) return null;
  const [headerType, id, channelName, method] = header.value as unknown[];
  if (typeof headerType !== "number") return null;
  const body = deserializeValue(bytes, header.pos);
  return {
    headerType,
    id: typeof id === "number" ? id : null,
    channelName: typeof channelName === "string" ? channelName : null,
    method: typeof method === "string" ? method : null,
    body: body ? body.value : undefined,
  };
}

/** 编码一次 Channel 方法调用（RequestType.Promise=100 + 参数数组）。 */
export function encodeChannelRequest(args: {
  id: number;
  channel: string;
  method: string;
  params?: unknown;
}): Uint8Array {
  const header = serializeValue([REQUEST_TYPE_PROMISE, args.id, args.channel, args.method]);
  const body = serializeValue([args.params]);
  const out = new Uint8Array(header.byteLength + body.byteLength);
  out.set(header, 0);
  out.set(body, header.byteLength);
  return out;
}

/**
 * 编码一次 Channel 事件订阅（RequestType.EventListen=102；channelClient.ts:150-177
 * requestEvent → sendRequest :179-194：serialize([102, id, channel, event]) +
 * serialize(arg)）。后续帧以 EventFire(204) 按同一 id 回推（channelClient.ts:210-231
 * onBuffer → handlers.get(id)）。官方动态事件 wire 名 = 属性名本体
 * （proxy-channel.ts:126-127）。
 */
export function encodeChannelEventListen(args: {
  id: number;
  channel: string;
  eventName: string;
  arg?: unknown;
}): Uint8Array {
  const header = serializeValue([REQUEST_TYPE_EVENT_LISTEN, args.id, args.channel, args.eventName]);
  const body = serializeValue(args.arg);
  const out = new Uint8Array(header.byteLength + body.byteLength);
  out.set(header, 0);
  out.set(body, header.byteLength);
  return out;
}

/** 编码一条服务端→客户端消息（应答 / EventFire），fake 与探针解析对拍用。 */
export function encodeChannelResponse(args: {
  responseType: number;
  id: number;
  body?: unknown;
}): Uint8Array {
  const header = serializeValue([args.responseType, args.id]);
  const body = serializeValue(args.body === undefined ? undefined : args.body);
  const out = new Uint8Array(header.byteLength + body.byteLength);
  out.set(header, 0);
  out.set(body, header.byteLength);
  return out;
}

// ---------------------------------------------------------------------------
// CRC32（IEEE，与官方 crc32WireBytes 同语义）
//
// packages/shared/src/web-remote-control-rpc-transport.ts:83 checksum 字段 +
// zcode-protocol-v4/wire-binary.ts 的 crc32WireBytes。B1 实连 run
// macos-B120261024T105940Z-NarmSw 的 4 条帧 checksum 已按标准 IEEE CRC32 复算逐一命中。
// ---------------------------------------------------------------------------

const CRC32_TABLE = (() => {
  const table = new Uint32Array(256);
  for (let n = 0; n < 256; n += 1) {
    let c = n;
    for (let k = 0; k < 8; k += 1) {
      c = c & 1 ? 0xedb88320 ^ (c >>> 1) : c >>> 1;
    }
    table[n] = c >>> 0;
  }
  return table;
})();

export function crc32WireBytes(bytes: Uint8Array): string {
  let crc = 0xffffffff;
  for (let i = 0; i < bytes.byteLength; i += 1) {
    crc = CRC32_TABLE[(crc ^ bytes[i]) & 0xff] ^ (crc >>> 8);
  }
  return ((crc ^ 0xffffffff) >>> 0).toString(16).padStart(8, "0");
}

// ---------------------------------------------------------------------------
// rpc-frame（packages/shared/src/web-remote-control-rpc-transport.ts:60-107）
// ---------------------------------------------------------------------------

export type RpcFrameIdentity = {
  bridgeSessionId: string;
  bridgeGeneration?: number;
  recoveryId?: string;
};

export type RpcFramePayload = {
  zcode_type: "rpc-frame";
  bridgeSessionId: string;
  bridgeGeneration?: number;
  recoveryId?: string;
  seq: number;
  messageSeq: number;
  fragmentIndex: number;
  fragmentCount: number;
  messageBytes: number;
  checksum: { algorithm: "crc32"; value: string };
  dataBase64: string;
};

/**
 * 编码单分片 rpc-frame（探针消息远小于 1 MiB 预算；多分片不在本参照实现范围——
 * 超预算直接抛错，绝不静默截断）。字段与官方 frameShell 一一对应
 * （web-remote-control-rpc-transport.ts:293-314）。
 */
export function encodeRpcFrame(args: {
  identity: RpcFrameIdentity;
  seq: number;
  messageSeq: number;
  bytes: Uint8Array;
}): RpcFramePayload {
  if (args.bytes.byteLength === 0) {
    throw new Error("rpc-frame: empty message");
  }
  const dataBase64 = Buffer.from(args.bytes).toString("base64");
  const payload: RpcFramePayload = {
    zcode_type: "rpc-frame",
    bridgeSessionId: args.identity.bridgeSessionId,
    ...(args.identity.bridgeGeneration !== undefined
      ? { bridgeGeneration: args.identity.bridgeGeneration }
      : {}),
    ...(args.identity.recoveryId !== undefined ? { recoveryId: args.identity.recoveryId } : {}),
    seq: args.seq,
    messageSeq: args.messageSeq,
    fragmentIndex: 0,
    fragmentCount: 1,
    messageBytes: args.bytes.byteLength,
    checksum: { algorithm: "crc32", value: crc32WireBytes(args.bytes) },
    dataBase64,
  };
  // 信封总字节预算（{type:"data", payload, client_ts} 全长，1 MiB）。
  const envelopeBytes = JSON.stringify({ type: "data", payload }).length;
  if (envelopeBytes > PROTOCOL_V4_MAX_FRAME_BYTES) {
    throw new Error(
      `rpc-frame: envelope ${envelopeBytes}B exceeds ${PROTOCOL_V4_MAX_FRAME_BYTES}B`
    );
  }
  return payload;
}

/**
 * 被动 rpc-frame 组装器（探针侧收帧用）：按 identity 绑定、物理 seq 连续去重、
 * CRC32 校验、messageSeq 单调。与官方 WebRemoteControlRpcTransportAssembler
 * （web-remote-control-rpc-transport.ts:521-766）的**语义子集**：
 *   - 首帧绑定 expectedSeq（探针可能从流中途开始收，官方从 1 起判 gap）；
 *   - 重复（seq < expected 或同 seq 同指纹）→ duplicate；
 *   - gap → fault（探针侧捕获容错：重置状态等下一帧重新绑定，官方为终态降级——
 *     差异仅影响被动捕获韧性，不影响发送路径：本模块发送侧从不消费本组装器）。
 */
export type AssemblerEvent =
  | { kind: "complete"; bytes: Uint8Array; messageSeq: number }
  | { kind: "duplicate" }
  | { kind: "incomplete"; messageSeq: number; receivedFragments: number }
  | { kind: "fault"; reason: string };

export class RpcFrameAssembler {
  private identity: RpcFrameIdentity | null;
  private expectedSeq: number | null = null;
  private lastMessageSeq = 0;
  private active: {
    messageSeq: number;
    fragmentCount: number;
    messageBytes: number;
    checksum: string;
    fragments: Uint8Array[];
  } | null = null;

  constructor(options: { identity?: RpcFrameIdentity | null } = {}) {
    // identity = null → 绑定到首条收到的帧（B1 式被动捕获）。
    this.identity = options.identity ?? null;
  }

  accept(candidate: unknown): AssemblerEvent {
    if (!candidate || typeof candidate !== "object") return { kind: "duplicate" };
    const frame = candidate as Partial<RpcFramePayload> & { checksum?: { value?: unknown } };
    if (frame.zcode_type !== "rpc-frame") return { kind: "duplicate" };
    if (
      typeof frame.bridgeSessionId !== "string" ||
      typeof frame.seq !== "number" ||
      typeof frame.messageSeq !== "number" ||
      typeof frame.dataBase64 !== "string"
    ) {
      return { kind: "fault", reason: "invalid-metadata" };
    }
    if (this.identity && frame.bridgeSessionId !== this.identity.bridgeSessionId) {
      return { kind: "fault", reason: "identity-mismatch" };
    }
    if (!this.identity) {
      this.identity = {
        bridgeSessionId: frame.bridgeSessionId,
        ...(typeof frame.bridgeGeneration === "number"
          ? { bridgeGeneration: frame.bridgeGeneration }
          : {}),
      };
    }
    const decoded = Uint8Array.from(Buffer.from(frame.dataBase64, "base64"));
    if (decoded.byteLength === 0) return { kind: "fault", reason: "invalid-base64" };
    if (this.expectedSeq !== null && frame.seq < this.expectedSeq) {
      return { kind: "duplicate" };
    }
    if (this.expectedSeq !== null && frame.seq > this.expectedSeq) {
      // 物理缺口：捕获态重置（见类注）。
      this.reset();
      this.identity = {
        bridgeSessionId: frame.bridgeSessionId,
        ...(typeof frame.bridgeGeneration === "number"
          ? { bridgeGeneration: frame.bridgeGeneration }
          : {}),
      };
    }
    if (this.expectedSeq === null) this.expectedSeq = frame.seq;

    if (this.lastMessageSeq !== 0 && frame.messageSeq <= this.lastMessageSeq) {
      return { kind: "duplicate" };
    }
    const fragmentCount = typeof frame.fragmentCount === "number" ? frame.fragmentCount : 1;
    const messageBytes =
      typeof frame.messageBytes === "number" ? frame.messageBytes : decoded.byteLength;
    const checksum = typeof frame.checksum?.value === "string" ? frame.checksum.value : "";
    if (!this.active || this.active.messageSeq !== frame.messageSeq) {
      this.active = {
        messageSeq: frame.messageSeq,
        fragmentCount,
        messageBytes,
        checksum,
        fragments: [],
      };
    }
    const active = this.active;
    const expectedFragmentIndex = active.fragments.length;
    if (
      (typeof frame.fragmentIndex === "number" ? frame.fragmentIndex : 0) !== expectedFragmentIndex
    ) {
      this.reset();
      return { kind: "fault", reason: "fragment-gap" };
    }
    active.fragments.push(decoded);
    if (active.fragments.length < active.fragmentCount) {
      return {
        kind: "incomplete",
        messageSeq: active.messageSeq,
        receivedFragments: active.fragments.length,
      };
    }
    const completed = new Uint8Array(active.messageBytes);
    let offset = 0;
    for (const fragment of active.fragments) {
      if (offset + fragment.byteLength > completed.byteLength) {
        this.reset();
        return { kind: "fault", reason: "length-mismatch" };
      }
      completed.set(fragment, offset);
      offset += fragment.byteLength;
    }
    if (offset !== completed.byteLength || crc32WireBytes(completed) !== active.checksum) {
      this.reset();
      return { kind: "fault", reason: "checksum-mismatch" };
    }
    this.lastMessageSeq = active.messageSeq;
    this.active = null;
    this.expectedSeq = (this.expectedSeq ?? frame.seq) + 1;
    return { kind: "complete", bytes: completed, messageSeq: active.messageSeq };
  }

  private reset(): void {
    this.active = null;
    this.expectedSeq = null;
  }
}

// ---------------------------------------------------------------------------
// 单连接闸（工作包 B「同一台 Tuvis 主机一条上游 terminal 连接」的本地强制点）
// ---------------------------------------------------------------------------

export type ProbeConnection = {
  sendData(payload: unknown): boolean;
};

export type AcquireResult<T> =
  { ok: true; connection: T } | { ok: false; reason: "already-connected" };

/**
 * 单连接闸：active 期间 acquire 直接本地拒绝（factory 不被调用 → 不会构造第二条
 * WS 升级）；release 后才允许下一次 acquire。gate 本身不建连接、不重连。
 */
export class SingleConnectionGate {
  private activeConnection: ProbeConnection | null = null;

  get active(): boolean {
    return this.activeConnection !== null;
  }

  acquire<T extends ProbeConnection>(factory: () => T): AcquireResult<T> {
    if (this.activeConnection !== null) {
      return { ok: false, reason: "already-connected" };
    }
    const connection = factory();
    this.activeConnection = connection;
    return { ok: true, connection };
  }

  release(): void {
    this.activeConnection = null;
  }
}

// ---------------------------------------------------------------------------
// 会话映射（精确归属 + 代际绑定）
//
// listing 行形状 = B1 实连帧实证的两个来源：
//   - workspaces[]：bootstrap-response / workspace-list-response 的
//     WebRemoteControlWorkspaceTarget {workspacePath, workspaceIdentity?, label, kind}
//     （packages/shared/src/web-remote-control.ts:628-635）；
//   - tasks[]：controller/tasks-index 帧的 WindowHostControllerTaskRow
//     {address:{workspacePath, taskId}, meta:{title,...}}
//     （packages/shared/src/zcode-protocol-v4/controller.ts:17-35,93-123；
//     B1 实连 run macos-B120261024T105940Z-NarmWs 帧内 task.upserted 增量同形状）。
// ---------------------------------------------------------------------------

export type WorkspaceListingEntry = {
  workspacePath: string;
  workspaceIdentity?: string;
  remoteSessionId?: string;
  label?: string;
  kind?: string;
  /** App 展示分类（web-remote-control.ts:633 WorkspacePurpose；B2 实连实证
   * project | conversation，部分旧条目缺席）。 */
  workspacePurpose?: string;
};

export type TaskListingRow = {
  address: { workspacePath: string; taskId: string; workspaceIdentity?: string };
  meta?: { title?: string; taskId?: string; workspacePath?: string };
};

/** 绑定产物：发送只允许携带这里的精确 id（workspaceKey 口径 = web-remote-control.ts:744-747）。 */
export type BoundTarget = {
  taskId: string;
  workspacePath: string;
  workspaceIdentity?: string;
  workspaceKey: string;
  /** 解析时的连接代际；authorizeSend 用它拒绝旧代际映射（stale-epoch）。 */
  generation: number;
};

export class SessionTargetMapper {
  private readonly workspaces: WorkspaceListingEntry[];
  private readonly tasks: TaskListingRow[];
  private readonly generation: number;

  private constructor(args: {
    workspaces: WorkspaceListingEntry[];
    tasks: TaskListingRow[];
    generation: number;
  }) {
    this.workspaces = args.workspaces;
    this.tasks = args.tasks;
    this.generation = args.generation;
  }

  static fromListings(args: {
    workspaces: WorkspaceListingEntry[];
    tasks: TaskListingRow[];
    generation: number;
  }): SessionTargetMapper {
    return new SessionTargetMapper(args);
  }

  get boundGeneration(): number {
    return this.generation;
  }

  /**
   * 解析发送目标：taskId 必须在当前 listing 里**精确存在**；指定 workspacePath 时
   * 必须与该行 address.workspacePath 严格相等。标题/cwd 只是操作员 designate 阶段的
   * 人读指称——本模块（以及整个发送面）没有、也不允许有按标题查找的入口。
   */
  resolveTarget(designated: { taskId: string; workspacePath?: string }): BoundTarget {
    const taskId = designated.taskId;
    if (typeof taskId !== "string" || taskId.trim().length === 0) {
      throw new Error("invalid-designation: taskId must be a non-empty exact id");
    }
    const rows = this.tasks.filter((row) => row.address.taskId === taskId);
    if (rows.length === 0) {
      throw new Error(
        `task-not-in-listing: taskId not present in current generation listing (refusing to send)`
      );
    }
    const addresses = new Set(rows.map((row) => row.address.workspacePath));
    if (addresses.size > 1) {
      throw new Error(
        "ambiguous: taskId maps to multiple workspacePaths in listing; refusing without exact workspace"
      );
    }
    const row = rows[0];
    const rowWorkspacePath = row.address.workspacePath;
    if (designated.workspacePath !== undefined && designated.workspacePath !== rowWorkspacePath) {
      throw new Error(
        "workspace-mismatch: designated workspacePath does not match listing row address"
      );
    }
    if (designated.workspacePath === undefined) {
      const matchingEntries = this.workspaces.filter(
        (entry) => entry.workspacePath === rowWorkspacePath
      );
      if (matchingEntries.length === 0) {
        throw new Error(
          "workspace-not-in-listing: task workspacePath absent from workspace listing"
        );
      }
    }
    const entry = this.workspaces.find((item) => item.workspacePath === rowWorkspacePath);
    const workspaceIdentity = row.address.workspaceIdentity ?? entry?.workspaceIdentity;
    return {
      taskId: row.address.taskId,
      workspacePath: rowWorkspacePath,
      ...(workspaceIdentity !== undefined ? { workspaceIdentity } : {}),
      // workspaceKey = workspaceIdentity?.trim() || workspacePath（web-remote-control.ts:744-747）
      workspaceKey: workspaceIdentity?.trim() || rowWorkspacePath,
      generation: this.generation,
    };
  }

  /**
   * 代际闸：映射绑定的 generation 必须等于当前存活连接的 generation，否则
   * stale-epoch——旧连接代际的映射不得授权新连接上的发送（必须用新代际 listing
   * 重新解析）。
   */
  authorizeSend(target: BoundTarget, liveGeneration: number): void {
    if (target.generation !== liveGeneration) {
      throw new Error(
        `stale-epoch: mapping bound to generation ${target.generation}, live connection is ${liveGeneration}`
      );
    }
  }
}

// ---------------------------------------------------------------------------
// workspace 指称唯一化（B2 实连 map-only 修复，2026-10-24）
//
// 实连 run macos-B220261024T121929Z-SnWWVA：bootstrap-response 与
// workspace-list-response 携带**同一份** workspaces[]（两份 14 条原样合并 28 条），
// 任何 label 的精确匹配都至少翻倍 → 「精确唯一」永假，map-only run 在 designate
// workspace-ambiguous fail-closed（未发送任何写向）。修复三件套：
//   1) 双 listing 去重（dedupeWorkspaceEntries）；
//   2) 同 label 多候选时按 workspacePurpose === "project" 收窄（web-remote-control.ts
//      :633 workspacePurpose；实连实证同 label 会以 project/conversation 各挂一条）；
//   3) 仍多候选时按 workspacePath === activeWorkspaceKey 收窄——activeWorkspaceKey
//      只读自 bootstrap.response result.mobileViewState.activeWorkspaceKey
//      （web-remote-control.ts:694-698 MobileViewState + :726-735 bootstrap result；
//      B2 实连已实证该字段存在），缺失时回落 workspace-list result.activeWorkspaceKey
//      （同文件 :672-676）。
// 每一步收窄只以**计数**进证据（绝不输出值）。发送授权仍只认 resolveTarget 产物：
// 本节函数只产出 listing 条目，不构造任何发送目标。
// ---------------------------------------------------------------------------

/** 双 listing 去重：bootstrap 与 workspace-list 的同一条目只保留一份。 */
export function dedupeWorkspaceEntries(entries: WorkspaceListingEntry[]): WorkspaceListingEntry[] {
  const seen = new Set<string>();
  const out: WorkspaceListingEntry[] = [];
  for (const entry of entries) {
    const key = JSON.stringify([
      entry.workspacePath,
      entry.workspaceIdentity ?? null,
      entry.remoteSessionId ?? null,
      entry.label ?? null,
      entry.kind ?? null,
      entry.workspacePurpose ?? null,
    ]);
    if (seen.has(key)) continue;
    seen.add(key);
    out.push(entry);
  }
  return out;
}

/**
 * APP 当前打开的 workspace key：优先 bootstrap.result.mobileViewState.activeWorkspaceKey
 * （web-remote-control.ts:694-698 + :726-735），缺失回落 workspace-list
 * result.activeWorkspaceKey（同文件 :672-676）；都缺失 → null（阶梯退化为只按
 * purpose 收窄）。
 */
export function readActiveWorkspaceKeyFromListings(args: {
  bootstrap: Record<string, unknown> | null;
  workspaceList: Record<string, unknown> | null;
}): string | null {
  const readFrom = (payload: Record<string, unknown> | null): string | null => {
    if (!payload) return null;
    const result = payload.result;
    if (!result || typeof result !== "object") return null;
    const record = result as Record<string, unknown>;
    const mobile = record.mobileViewState;
    const fromMobileViewState =
      mobile && typeof mobile === "object"
        ? (mobile as { activeWorkspaceKey?: unknown }).activeWorkspaceKey
        : undefined;
    const candidate =
      typeof fromMobileViewState === "string" && fromMobileViewState
        ? fromMobileViewState
        : record.activeWorkspaceKey;
    return typeof candidate === "string" && candidate ? candidate : null;
  };
  return readFrom(args.bootstrap) ?? readFrom(args.workspaceList);
}

/** designation 证据（全部为计数/布尔/步骤标签，绝不含真实值）。 */
export type WorkspaceDesignationEvidence = {
  /** label 精确相等的候选数。 */
  labelMatches: number;
  /** purpose=project 收窄后的候选数（该步未应用 = null）。 */
  afterPurposeFilter: number | null;
  /** activeWorkspaceKey 收窄后的候选数（该步未应用 = null）。 */
  afterActiveKeyFilter: number | null;
  /** bootstrap/workspace-list 是否提供了 activeWorkspaceKey。 */
  activeWorkspaceKnown: boolean;
  /** 唯一化发生在哪一步（中止 = null）。 */
  resolvedStep: "exact-label" | "purpose-project" | "active-workspace-key" | null;
  /** 解析产物 workspacePath === activeWorkspaceKey 的交叉确认。 */
  activeWorkspaceConfirmed: boolean;
};

export type WorkspaceDesignation =
  | { ok: true; entry: WorkspaceListingEntry; evidence: WorkspaceDesignationEvidence }
  | {
      ok: false;
      reason: "workspace-not-found" | "workspace-ambiguous";
      evidence: WorkspaceDesignationEvidence;
    };

/**
 * workspace 指称唯一化阶梯：label 精确匹配 → 唯一即定；多候选时优先
 * workspacePurpose === "project" → 仍多候选时优先 workspacePath ===
 * activeWorkspaceKey → 仍不唯一即 workspace-ambiguous（fail-closed）。
 * purpose 收窄为空集时保留原候选集继续走 activeWorkspaceKey（"prefer" 语义，
 * 不因空集放弃收窄机会）。
 */
export function resolveDesignatedWorkspace(args: {
  workspaces: WorkspaceListingEntry[];
  label: string;
  activeWorkspaceKey?: string | null;
}): WorkspaceDesignation {
  const activeWorkspaceKnown =
    typeof args.activeWorkspaceKey === "string" && args.activeWorkspaceKey.length > 0;
  const evidence: WorkspaceDesignationEvidence = {
    labelMatches: 0,
    afterPurposeFilter: null,
    afterActiveKeyFilter: null,
    activeWorkspaceKnown,
    resolvedStep: null,
    activeWorkspaceConfirmed: false,
  };
  const labelMatches = args.workspaces.filter((entry) => entry.label === args.label);
  evidence.labelMatches = labelMatches.length;
  if (labelMatches.length === 0) {
    return { ok: false, reason: "workspace-not-found", evidence };
  }
  if (labelMatches.length === 1) {
    evidence.resolvedStep = "exact-label";
    evidence.activeWorkspaceConfirmed =
      activeWorkspaceKnown && labelMatches[0].workspacePath === args.activeWorkspaceKey;
    return { ok: true, entry: labelMatches[0], evidence };
  }
  let candidates = labelMatches;
  const projects = candidates.filter((entry) => entry.workspacePurpose === "project");
  evidence.afterPurposeFilter = projects.length;
  if (projects.length === 1) {
    evidence.resolvedStep = "purpose-project";
    evidence.activeWorkspaceConfirmed =
      activeWorkspaceKnown && projects[0].workspacePath === args.activeWorkspaceKey;
    return { ok: true, entry: projects[0], evidence };
  }
  if (projects.length > 1) {
    candidates = projects;
  }
  if (activeWorkspaceKnown) {
    const active = candidates.filter((entry) => entry.workspacePath === args.activeWorkspaceKey);
    evidence.afterActiveKeyFilter = active.length;
    if (active.length === 1) {
      evidence.resolvedStep = "active-workspace-key";
      evidence.activeWorkspaceConfirmed = true;
      return { ok: true, entry: active[0], evidence };
    }
  }
  return { ok: false, reason: "workspace-ambiguous", evidence };
}

// ---------------------------------------------------------------------------
// controller tasks-index 主动枚举（B2 实连 map-only 修复，2026-10-24）
//
// 被动收帧窗在 B2 实连（macos-B220261024T121929Z-SnWWVA）8s 内 0 条 rpc-frame——
// 任务行必须主动订阅。wire 全部钉 @ 3c5b2276：
//   - channel "window-controller" 只在 workspace bridge 的 relay 通道上发布
//     （remoteServiceAccess.ts:181-185；websocket.ts:102-108；
//     webRemoteControlWorkspaceBridgeSession.ts:39-74）→ 先 workspace-bridge-open
//     （taskId 可选：web-remote-control.ts:435-447；枚举期省略）拿 bridge 身份；
//   - 帧监听 = EventListen(102) 事件 "onDynamicControllerFrame"
//     （windowController.ts:78；proxy-channel.ts:126-127 动态事件 → channel.listen；
//     channelClient.ts:150-177 requestEvent、:179-194 sendRequest、:210-231 按 id 回推
//     EventFire；channels.shared.ts:21）——官方 registry 在 subscribe 前先挂监听
//     （windowControllerTaskListRegistry.ts:145 → :220-224）；
//   - 主动枚举 = subscribeControllerV4{topic:"controller/tasks-index",
//     visibility:"foreground"}（windowController.ts:75；params controller.ts:258-271；
//     ack {subscriptionId, mode, logEpoch} transport.ts:125-131 + controller.ts:273-274）；
//   - 任务行从 EventFire(204) 帧回推，body = WindowHostControllerTaskFrame
//     （controller.ts:251-256）：snapshot.tasks（:155-164）或 deltas
//     task.upserted / task.removed（:187-191）；行形状 :93-123（address{workspacePath,
//     taskId, workspaceIdentity?} + meta.title）。官方消费面同款
//     （windowControllerTaskListRegistry.ts:176-198）。
// 本 reader 只读：全部出站帧都是读面控制帧（订阅/退订），不构造任何会话发送帧
// （maxSendFrames 闸只数会话发送帧，与此处正交）。
// ---------------------------------------------------------------------------

/** 枚举全程计数（证据只落数值，绝不落真实 id/epoch）。 */
export type ControllerTaskIndexCounters = {
  eventListenRequestId: number;
  subscribeRequestId: number;
  unsubscribeRequestId: number | null;
  /** 出站 rpc-frame 总数（EventListen + subscribe + unsubscribe；读面控制帧）。 */
  controlFramesSent: number;
  /** peer 受理回执数（rpc-frame-ack；去重后）。 */
  peerAckedFrames: number;
  duplicateInboundFrames: number;
  /** topic = controller/tasks-index 的 EventFire 帧数。 */
  controllerFramesSeen: number;
  snapshotFrames: number;
  deltaFrames: number;
  /** 去重后保留的任务行数。 */
  rowsCollected: number;
  /** 超 maxRows 上限被丢弃的行数。 */
  rowsOverflow: number;
  /** 退订是否得到应答（尽力而为；失败不影响结果）。 */
  unsubscribed: boolean;
};

export type ControllerTaskIndexReadResult =
  | {
      kind: "ok";
      rows: TaskListingRow[];
      /** subscribe ack 的 mode（"snapshot" | "resume"；回显缺失时 null）。 */
      ackMode: string | null;
      counters: ControllerTaskIndexCounters;
    }
  | {
      kind: "send-failed" | "subscribe-timeout" | "subscribe-rejected";
      counters: ControllerTaskIndexCounters;
    };

export type ControllerTaskIndexReaderOptions = {
  sender: ProbeConnection;
  /** 已就绪 workspace bridge 的帧身份（bridge-open ready 回显）。 */
  identity: RpcFrameIdentity;
  timeouts: { subscribeMs: number; windowMs: number; unsubscribeMs: number };
  firstSeq?: number;
  firstMessageSeq?: number;
  /** 任务行收集上限（防巨库撑爆内存；超限丢弃并计数）。 */
  maxRows?: number;
  sleep?: (ms: number) => Promise<void>;
};

/**
 * 主动任务索引枚举器：EventListen → subscribeControllerV4 → 有界收帧窗 →
 * 尽力退订。行按 `workspacePath\u0000taskId` upsert 去重（snapshot 后的增量同键
 * 覆盖，task.removed 同键删除）。等待循环只有 sleep+判定，无任何重发路径。
 */
export class ControllerTaskIndexReader {
  readonly counters: ControllerTaskIndexCounters = {
    eventListenRequestId: 0,
    subscribeRequestId: 0,
    unsubscribeRequestId: null,
    controlFramesSent: 0,
    peerAckedFrames: 0,
    duplicateInboundFrames: 0,
    controllerFramesSeen: 0,
    snapshotFrames: 0,
    deltaFrames: 0,
    rowsCollected: 0,
    rowsOverflow: 0,
    unsubscribed: false,
  };

  private readonly sender: ProbeConnection;
  private readonly identity: RpcFrameIdentity;
  private readonly subscribeMs: number;
  private readonly windowMs: number;
  private readonly unsubscribeMs: number;
  private readonly maxRows: number;
  private readonly sleep: (ms: number) => Promise<void>;
  private readonly assembler: RpcFrameAssembler;
  private readonly rows = new Map<string, TaskListingRow>();
  private nextRequestId: number;
  private seq: number;
  private messageSeq: number;
  private eventListenRequestId = 0;
  private subscribeRequestId = 0;
  private unsubscribeRequestId = 0;
  private settledAck:
    | { status: "ok"; mode: string | null; subscriptionId: string | null }
    | { status: "rejected" }
    | null = null;
  private unsubscribeSettled = false;
  private lastPeerAckSeq = 0;

  constructor(options: ControllerTaskIndexReaderOptions) {
    this.sender = options.sender;
    this.identity = options.identity;
    this.subscribeMs = Math.max(1, options.timeouts.subscribeMs);
    this.windowMs = Math.max(1, options.timeouts.windowMs);
    this.unsubscribeMs = Math.max(1, options.timeouts.unsubscribeMs);
    this.maxRows = Math.max(1, options.maxRows ?? 1024);
    this.sleep = options.sleep ?? realSleepDefault;
    this.assembler = new RpcFrameAssembler({ identity: options.identity });
    this.nextRequestId = 1;
    this.seq = options.firstSeq ?? 1;
    this.messageSeq = options.firstMessageSeq ?? 1;
  }

  /**
   * 订阅并收集一个有界窗内的任务行。顺序 = 官方 registry 同款：帧监听先行 →
   * subscribeControllerV4 → 等窗 → unsubscribe（尽力）。subscribe 失败/超窗 =
   * 结构性失败（rows 为空，由调用方 fail-closed），不重试。
   */
  async run(): Promise<ControllerTaskIndexReadResult> {
    // 1) 帧监听（102；无应答帧——后续 EventFire 事件本体就是回执）。
    this.eventListenRequestId = this.nextRequestId;
    this.nextRequestId += 1;
    this.counters.eventListenRequestId = this.eventListenRequestId;
    if (
      !this.sendFrame(
        encodeChannelEventListen({
          id: this.eventListenRequestId,
          channel: WINDOW_CONTROLLER_CHANNEL,
          eventName: CONTROLLER_DYNAMIC_FRAME_EVENT,
        })
      )
    ) {
      return { kind: "send-failed", counters: this.counters };
    }

    // 2) 主动枚举订阅（100，{topic, visibility}；controller.ts:258-271）。
    this.subscribeRequestId = this.nextRequestId;
    this.nextRequestId += 1;
    this.counters.subscribeRequestId = this.subscribeRequestId;
    if (
      !this.sendFrame(
        encodeChannelRequest({
          id: this.subscribeRequestId,
          channel: WINDOW_CONTROLLER_CHANNEL,
          method: CONTROLLER_SUBSCRIBE_METHOD,
          params: { topic: CONTROLLER_TASKS_INDEX_TOPIC, visibility: "foreground" },
        })
      )
    ) {
      return { kind: "send-failed", counters: this.counters };
    }
    const subscribeDeadline = Date.now() + this.subscribeMs;
    while (this.settledAck === null && Date.now() < subscribeDeadline) {
      await this.sleep(Math.min(20, Math.max(1, subscribeDeadline - Date.now())));
    }
    const settled = this.settledAck;
    if (settled === null) {
      return { kind: "subscribe-timeout", counters: this.counters };
    }
    if (settled.status === "rejected") {
      return { kind: "subscribe-rejected", counters: this.counters };
    }

    // 3) 有界收帧窗：snapshot / deltas → 任务行（upsert 去重）。
    const windowDeadline = Date.now() + this.windowMs;
    while (Date.now() < windowDeadline) {
      await this.sleep(Math.min(20, Math.max(1, windowDeadline - Date.now())));
    }
    const rows = Array.from(this.rows.values());
    this.counters.rowsCollected = rows.length;

    // 4) 尽力退订（读面卫生；失败/超时不影响结果、不重试）。
    if (settled.subscriptionId) {
      const subscriptionId = settled.subscriptionId;
      this.unsubscribeRequestId = this.nextRequestId;
      this.nextRequestId += 1;
      this.counters.unsubscribeRequestId = this.unsubscribeRequestId;
      if (
        this.sendFrame(
          encodeChannelRequest({
            id: this.unsubscribeRequestId,
            channel: WINDOW_CONTROLLER_CHANNEL,
            method: CONTROLLER_UNSUBSCRIBE_METHOD,
            params: { subscriptionId },
          })
        )
      ) {
        const unsubscribeDeadline = Date.now() + this.unsubscribeMs;
        while (!this.unsubscribeSettled && Date.now() < unsubscribeDeadline) {
          await this.sleep(Math.min(20, Math.max(1, unsubscribeDeadline - Date.now())));
        }
        this.counters.unsubscribed = this.unsubscribeSettled;
      }
    }
    return { kind: "ok", rows, ackMode: settled.mode, counters: this.counters };
  }

  /** 入站 data payload 喂入口（对接宿主 channel 的 onRpcFrame 单钩子）。 */
  onData(payload: unknown): void {
    if (!payload || typeof payload !== "object") return;
    const record = payload as { zcode_type?: unknown; ackMessageSeq?: unknown };
    if (record.zcode_type === "rpc-frame-ack") {
      // peer 受理：只计数（读面控制帧的回执），无任何副作用。
      const ackSeq = typeof record.ackMessageSeq === "number" ? record.ackMessageSeq : 0;
      if (ackSeq <= this.lastPeerAckSeq) {
        this.counters.duplicateInboundFrames += 1;
        return;
      }
      this.lastPeerAckSeq = ackSeq;
      this.counters.peerAckedFrames += 1;
      return;
    }
    if (record.zcode_type !== "rpc-frame") return;
    const event = this.assembler.accept(payload);
    if (event.kind === "duplicate") {
      this.counters.duplicateInboundFrames += 1;
      return;
    }
    if (event.kind === "fault" || event.kind === "incomplete") return;
    const decoded = decodeChannelMessage(event.bytes);
    if (!decoded) return;
    if (
      this.subscribeRequestId !== 0 &&
      decoded.id === this.subscribeRequestId &&
      (decoded.headerType === RESPONSE_TYPE_PROMISE_SUCCESS ||
        decoded.headerType === RESPONSE_TYPE_PROMISE_ERROR)
    ) {
      if (this.settledAck !== null) {
        this.counters.duplicateInboundFrames += 1;
        return;
      }
      if (decoded.headerType === RESPONSE_TYPE_PROMISE_ERROR) {
        this.settledAck = { status: "rejected" };
        return;
      }
      const body = decoded.body as
        { ack?: { subscriptionId?: unknown; mode?: unknown } } | undefined;
      const ack = body?.ack;
      this.settledAck = {
        status: "ok",
        mode: typeof ack?.mode === "string" ? ack.mode : null,
        subscriptionId: typeof ack?.subscriptionId === "string" ? ack.subscriptionId : null,
      };
      return;
    }
    if (
      this.unsubscribeRequestId !== 0 &&
      decoded.id === this.unsubscribeRequestId &&
      decoded.headerType === RESPONSE_TYPE_PROMISE_SUCCESS
    ) {
      if (!this.unsubscribeSettled) {
        this.unsubscribeSettled = true;
      } else {
        this.counters.duplicateInboundFrames += 1;
      }
      return;
    }
    if (decoded.headerType === RESPONSE_TYPE_EVENT_FIRE) {
      this.acceptControllerFrame(decoded.body);
    }
  }

  /** tasks-index 帧体解析（snapshot / deltas；形状松校验，坏行跳过不致命）。 */
  private acceptControllerFrame(body: unknown): void {
    if (!body || typeof body !== "object") return;
    const frame = body as {
      topic?: unknown;
      payload?: { kind?: unknown; snapshot?: { tasks?: unknown }; deltas?: unknown };
    };
    if (frame.topic !== CONTROLLER_TASKS_INDEX_TOPIC) return;
    this.counters.controllerFramesSeen += 1;
    if (frame.payload?.kind === "snapshot" && Array.isArray(frame.payload.snapshot?.tasks)) {
      this.counters.snapshotFrames += 1;
      for (const row of frame.payload.snapshot.tasks) this.upsertRow(row);
      return;
    }
    if (frame.payload?.kind === "deltas" && Array.isArray(frame.payload.deltas)) {
      this.counters.deltaFrames += 1;
      for (const delta of frame.payload.deltas) {
        if (!delta || typeof delta !== "object") continue;
        const record = delta as { op?: unknown; task?: unknown; address?: unknown };
        if (record.op === "task.upserted") {
          this.upsertRow(record.task);
        } else if (
          record.op === "task.removed" &&
          record.address &&
          typeof record.address === "object"
        ) {
          const address = record.address as { workspacePath?: unknown; taskId?: unknown };
          if (typeof address.workspacePath === "string" && typeof address.taskId === "string") {
            this.rows.delete(`${address.workspacePath}\u0000${address.taskId}`);
          }
        }
      }
    }
  }

  private upsertRow(candidate: unknown): void {
    if (!candidate || typeof candidate !== "object") return;
    const record = candidate as {
      address?: { workspacePath?: unknown; taskId?: unknown; workspaceIdentity?: unknown };
      meta?: { title?: unknown };
    };
    const address = record.address;
    if (
      !address ||
      typeof address.workspacePath !== "string" ||
      typeof address.taskId !== "string"
    ) {
      return;
    }
    const key = `${address.workspacePath}\u0000${address.taskId}`;
    if (this.rows.size >= this.maxRows && !this.rows.has(key)) {
      this.counters.rowsOverflow += 1;
      return;
    }
    const title =
      record.meta && typeof record.meta === "object" && typeof record.meta.title === "string"
        ? record.meta.title
        : undefined;
    this.rows.set(key, {
      address: {
        taskId: address.taskId,
        workspacePath: address.workspacePath,
        ...(typeof address.workspaceIdentity === "string"
          ? { workspaceIdentity: address.workspaceIdentity }
          : {}),
      },
      ...(title !== undefined ? { meta: { title } } : {}),
    });
  }

  private sendFrame(bytes: Uint8Array): boolean {
    const frame = encodeRpcFrame({
      identity: this.identity,
      seq: this.seq,
      messageSeq: this.messageSeq,
      bytes,
    });
    this.seq += 1;
    this.messageSeq += 1;
    this.counters.controlFramesSent += 1;
    return this.sender.sendData(frame);
  }
}

// ---------------------------------------------------------------------------
// 单次发送闭环（SendOnceLoop）
//
// 发送 wire（行号 @ 固定 SHA）：
//   rpc-frame（单分片，CRC32）→ relay data 信封 {type:"data", payload, client_ts}
//   （web-remote-control-rpc-transport.ts:60-107,133-143；dataBase64 = Channel 消息字节）
//   → serialize([100, id, "zcode-agent", "sendConversationCommandV4"]) +
//     serialize([{workspacePath, workspaceIdentity?, envelope}])
//   （rpc/channelClient.ts:182-190；channels.ts:114；zcodeAgent.ts:937,498-506）
//   envelope = {commandId(uuid), clientId, sessionId, type:"sendText",
//   payload:{text}, issuedAt}（zcode-protocol-v4/command.ts:343-356；sendText payload
//   :95-135——只携带 text，model/mode/plan 全部缺省=沿用会话现值；sendText 不在
//   COMMANDS_REQUIRING_BASE_REVISION（:315-331）→ 免 baseRevision）。
// ---------------------------------------------------------------------------

export type SendOutcome =
  | { kind: "completed"; activityFrames: number }
  | { kind: "accepted" }
  | { kind: "rejected"; reasonCode: string | null }
  | { kind: "send-failed" }
  | { kind: "unknown" };

export type SendOnceLoopOptions = {
  sender: ProbeConnection;
  identity: RpcFrameIdentity;
  /**
   * 只接受 SessionTargetMapper.resolveTarget 的产物；调用方必须先以 authorizeSend
   * 通过代际闸（stale-epoch 判据 = 映射代际 vs 存活连接代际）。构造不做代际校验。
   */
  target: BoundTarget;
  markerText: string;
  clientId: string;
  timeouts: { ackMs: number; activityMs: number };
  sleep?: (ms: number) => Promise<void>;
  /** 轮次活动判据（探针注入；缺省 = 任何该会话的入站 Channel 帧）。 */
  isTurnActivity?: (decoded: DecodedChannelMessage | null, rawJson: string) => boolean;
  now?: () => number;
  /**
   * 本发送在 bridge rpc 通道上的请求 id 与起始 seq/messageSeq。官方对端
   * （AcknowledgedRelayProtocol 组装面）按物理 seq 连续判重：hello/clientHello/
   * subscribe 已占用前几个序号时，发送帧必须续号而不是从 1 重来（seq 回退会被
   * 对端当 duplicate 丢弃）。缺省 1/1/1（单独使用本循环时的同值语义）。
   */
  channelRequestId?: number;
  firstSeq?: number;
  firstMessageSeq?: number;
};

export class SendOnceLoop {
  /** 生命周期内构造的 rpc-frame 数：结构性恒 ≤1。 */
  framesSent = 0;
  /** 组装器判定的重复入站帧数（重复 ack / 重复应答 / 重复事件）。 */
  duplicateInboundFrames = 0;
  /** 触发活动判据的入站帧数（去重后）。 */
  activityFrames = 0;

  private readonly sender: ProbeConnection;
  private readonly identity: RpcFrameIdentity;
  private readonly target: BoundTarget;
  private readonly markerText: string;
  private readonly clientId: string;
  private readonly ackMs: number;
  private readonly activityMs: number;
  private readonly sleep: (ms: number) => Promise<void>;
  private readonly isTurnActivity: (
    decoded: DecodedChannelMessage | null,
    rawJson: string
  ) => boolean;
  private readonly now: () => number;
  private readonly assembler: RpcFrameAssembler;
  private readonly channelRequestId: number;
  private readonly firstSeq: number;
  private readonly firstMessageSeq: number;
  private settledResponse:
    { status: "accepted" } | { status: "rejected"; reasonCode: string | null } | null = null;
  private requestId: number;
  private commandId = "";
  private lastPeerAckSeq = 0;
  /** peer 是否曾受理我们的 rpc-frame（rpc-frame-ack 到达过；仅供证据记录）。 */
  peerAcknowledged = false;

  constructor(options: SendOnceLoopOptions) {
    // 代际闸唯一住在 SessionTargetMapper.authorizeSend（映射代际 vs 存活连接代际，
    // like-for-like）。identity.bridgeGeneration 是宿主 workspace-bridge-ready 回显的
    // **bridge 通道代际**，与探针的连接代际（target.generation）不同命名空间——拿两者
    // 比较属类别错误（评审 Important 修复：宿主回显 ≠1 时构造曾在 try/catch 之外抛出，
    // 探针裸 crash 丢证据）。本字段仅信息性：随帧回传 + 落证据，不作闸。
    this.sender = options.sender;
    this.identity = options.identity;
    this.target = options.target;
    this.markerText = options.markerText;
    this.clientId = options.clientId;
    this.ackMs = options.timeouts.ackMs;
    this.activityMs = options.timeouts.activityMs;
    this.sleep = options.sleep ?? realSleepDefault;
    this.isTurnActivity = options.isTurnActivity ?? (() => true);
    this.now = options.now ?? Date.now;
    this.assembler = new RpcFrameAssembler({ identity: options.identity });
    this.channelRequestId = options.channelRequestId ?? 1;
    this.firstSeq = options.firstSeq ?? 1;
    this.firstMessageSeq = options.firstMessageSeq ?? 1;
    this.requestId = this.channelRequestId;
  }

  /**
   * 发送一次并等待结果。全生命周期恰好一个 rpc-frame；等待循环只有 sleep+判定，
   * **没有任何重发路径**。结果：
   *   completed = CommandAck accepted 且窗口内观察到该会话的轮次活动帧（去重后）；
   *   accepted  = CommandAck accepted，活动窗口静默；
   *   rejected  = 显式 CommandAck 拒绝 / PromiseError；
   *   send-failed = 客户端发送门禁挡下（未 paired / socket 非 OPEN）；
   *   unknown   = 窗口内未收到命令应答（peer 是否受理只看 rpc-frame-ack，绝不据此重发）。
   */
  async run(): Promise<SendOutcome> {
    const envelope = {
      // uuid（官方 commandId 为 uuid v7；探针取运行期随机 uuid，重试不变性不适用——
      // 本循环没有重试）。
      commandId: crypto.randomUUID(),
      clientId: this.clientId,
      sessionId: this.target.taskId,
      type: "sendText" as const,
      payload: { text: this.markerText },
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
    const ackDeadline = this.now() + this.ackMs;
    while (this.settledResponse === null && this.now() < ackDeadline) {
      await this.sleep(Math.min(20, Math.max(1, ackDeadline - this.now())));
    }
    if (this.settledResponse === null) {
      return { kind: "unknown" };
    }
    if (this.settledResponse.status === "rejected") {
      return {
        kind: "rejected",
        reasonCode: this.settledResponse.reasonCode,
      };
    }

    // 活动窗口（有界）：观察该会话的轮次活动帧；重复帧已被组装器去重，不重复计数。
    const activityDeadline = this.now() + this.activityMs;
    while (this.activityFrames === 0 && this.now() < activityDeadline) {
      await this.sleep(Math.min(20, Math.max(1, activityDeadline - this.now())));
    }
    return this.activityFrames > 0
      ? { kind: "completed", activityFrames: this.activityFrames }
      : { kind: "accepted" };
  }

  /** 入站 data payload 喂入口（对接 conn.onData 单槽）。 */
  onData(payload: unknown): void {
    if (!payload || typeof payload !== "object") return;
    const record = payload as { zcode_type?: unknown; ackMessageSeq?: unknown };
    if (record.zcode_type === "rpc-frame-ack") {
      // peer 受理：只记录事实（重复 ack 计入去重计数），无任何副作用，更不触发重发。
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
    if (event.kind === "fault") {
      return;
    }
    if (event.kind === "incomplete") return;
    const decoded = decodeChannelMessage(event.bytes);
    const rawJson = Buffer.from(event.bytes).toString("utf8");
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
      if (status === "accepted" || status === "duplicate" || status === "noop") {
        this.settledResponse = { status: "accepted" };
      } else {
        this.settledResponse = {
          status: "rejected",
          reasonCode: typeof ack?.reasonCode === "string" ? ack.reasonCode : status || null,
        };
      }
      return;
    }
    if (this.settledResponse !== null && this.isTurnActivity(decoded, rawJson)) {
      this.activityFrames += 1;
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
