// @vitest-environment node
// tests/remote/zcodeBridgeInteraction.test.ts — B3 红测：审批 / 问答 / 打断的逐能力 fake
// 合同（工作包 B 计划 §B3：docs/superpowers/plans/2026-10-24-zcode-official-bridge-validation.md）。
//
// 被测客户端 = scripts/zcode-bridge-probe/interactionBridge.ts（Tuvis 侧参照实现）：
//   - PendingBook：pending 绑定 session / interaction / toolCall（锚定 turn 的工具行）与
//     连接代际（generation）；异会话 topic 帧不收录；错 id / 旧代际 / 已消费 / 重复应答
//     一律 fail-closed，绝不波及其他 pending；
//   - InteractionCommandLoop：resolveInteraction / stop 单帧发送闭环——恰一个 rpc-frame，
//     无重发计时器；CommandAck 状态 accepted|duplicate → accepted、noop（如
//     guard.stopTargetChanged）→ noop、rejected|stale|failed → rejected、超窗 → unknown；
//   - authorizePendingResolution：发送前的唯一闸（session / generation / 存在性三查，
//     过闸即消费——同一 pending 不会被两次应答）；
//   - 只读视图隔离：映射 listing 与用户草稿等只读状态在 pending / resolution / stop
//     全流程中结构上只读（深冻结后流程跑通即证未变）。
//
// wire 形状依据 = 固定官方源码 zai-org/ZCode @ 3c5b227649371507ddc4eb46ac5133afe6db1cab
// （行号 @ 该 SHA）：
//   - resolveInteraction {interactionId, answer{optionId?, freeText?, action?, content?}}
//     → packages/shared/src/zcode-protocol-v4/command.ts:221-234；先到先得、晚到 noop
//     （:221 注释 + apps/zcode-cli/.../interaction-registry.ts:155-167 未命中
//     delivered=false 幂等成功）；resolveInteraction ACK result
//     {type:"resolveInteraction", resolvedBy} → command.ts:403-409；ack status 枚举
//     accepted|rejected|stale|duplicate|noop|failed → command.ts:456；
//   - stop {expectedForegroundExecutionId?} → command.ts:161-164；handler
//     apps/zcode-cli/.../commands/handlers/session-flow.ts:292-360（守卫 id 在手而
//     runtime idle/mismatch → V4CommandNoopError("guard.stopTargetChanged")——迟到 stop
//     只 noop 不误杀新执行；stopped → 只 abort 本会话 record.activeAbortController）；
//     守卫 id 的客户端可见来源 = 快照 control.activeWorks[].foregroundExecutionId
//     （packages/shared/src/zcode-protocol-v4/snapshot.ts:53-64；command.ts:162
//     「来自 activeWorks」注释）；
//   - 两命令都不在 COMMANDS_REQUIRING_BASE_REVISION（command.ts:315-331）→ 免 baseRevision；
//   - pending 卡 = conversation topic（transport.ts:113-114,163-191,194-224）snapshot /
//     deltas（state.updated patch 键级整体替换 pendingInteractions，delta.ts:50-53 +
//     product-projection.ts:3354-3382,3385-3533）；PendingInteraction 形状 = snapshot.ts
//     :316-355（interactionId / kind / anchorRowId / payload.permission{toolCallId,
//     toolName,summary,options[{optionId,kind,...}]} / payload.userInput{questions
//     [{question,header,options[{value,label}],multiSelect}]}）；
//   - 权限应答 = {optionId, freeText?}（packages/ui/src/v4/V4InteractionDialogs.tsx
//     :209-213,358-363）；fullAccess 特权选项 id = "fullAccess"（snapshot.ts:234
//     PERMISSION_FULL_ACCESS_OPTION_ID；interaction-background.ts:50-54 走
//     resolveFullAccess 特权分支——探针红线禁触）；
//   - 问答应答 = {action:"accept"|"decline"|"cancel", content?}（packages/ui/src/
//     ElicitationDialog.tsx:27,273-303 buildElicitationResponseContent：content.answers
//     按题干全文为键、值 = 选项 value 逗号连接（多选数组经 broker
//     normalizeAnswerValue 归一 interaction-broker.ts:766-780），answer_<index> /
//     answer 兼容键；选项 value 语义 = mapAskUserQuestion 的 value: option.label，
//     interaction-broker.ts:505-517）；
//   - turn 生命周期到端侧 = conversation 行 / control patch：行 status 枚举含
//     "cancelled"（subagent 行 rows.ts:272；user 行含 pendingApproval/cancelled
//     rows.ts:212；行锚 turnId rows.ts:19-20）；TurnComplete resultType
//     "cancelled"（session.events.ts:650-657「用户主动中断属于正常结束，
//     复用 TurnComplete 上报」）。
//
// 全部凭据/标识为合成值（b3-fake-* / fake-bridge-*），不含真实 SID/JWT/token/链接/会话
// id。本文件不连任何真实服务。
import * as crypto from "node:crypto";
import * as http from "node:http";
import type { Socket } from "node:net";
import { afterAll, afterEach, beforeAll, describe, expect, it } from "vitest";

// 本文件跑真实 127.0.0.1 socket：文件级 beforeAll 关闭全局 MSW（同 B1/B2 用例口径，
// vitest 按文件隔离 worker，只影响本文件）。
import { server as mswServer } from "../msw/server";

beforeAll(() => {
  mswServer.close();
});
afterAll(() => {
  mswServer.listen({ onUnhandledRequest: "bypass" });
});

import { RelayTerminalConnection } from "../../scripts/zcode-bridge-probe/relayAuth";
import {
  AGENT_CHANNEL,
  AGENT_SEND_COMMAND_METHOD,
  REQUEST_TYPE_PROMISE,
  RESPONSE_TYPE_PROMISE_SUCCESS,
  decodeChannelMessage,
  encodeChannelResponse,
  encodeRpcFrame,
} from "../../scripts/zcode-bridge-probe/sessionBridge";
import {
  InteractionCommandLoop,
  PendingBook,
  assertNotFullAccessOption,
  authorizePendingResolution,
  buildPermissionAnswer,
  buildStopPayload,
  buildUserInputAnswer,
  decodeTopicFrameSubject,
  type ObservedPending,
} from "../../scripts/zcode-bridge-probe/interactionBridge";

// ---------------------------------------------------------------------------
// FAKE relay（仅测试；同 B1/B2 形状的最小 RFC6455 实现）。行为标签——凡注释标
// 「FAKE 策略」的处理均为本测试自定，不是官方服务端取证结论。
// ---------------------------------------------------------------------------

const WS_GUID = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";
const TOKEN_LAST_CHARS = "AQgw";

type StoredToken = { expiresAt: number; consumed: boolean };
type ClientSession = { socket: Socket; authenticated: boolean; alive: boolean };

class FakeRelay {
  readonly deviceSid = "b3-fake-device-sid-0001";
  readonly stats = { wsUpgrades: 0, dataAccepted: 0 };
  /** 客户端发来的每条 data payload（发送帧计数与形状断言用）。 */
  readonly clientDataPayloads: Array<Record<string, unknown>> = [];
  private readonly server: http.Server;
  private readonly tokens = new Map<string, StoredToken>();
  private readonly sessions = new Set<ClientSession>();

  constructor() {
    this.server = http.createServer((_req, res) => {
      res.writeHead(404).end();
    });
    this.server.on("upgrade", (req, socket) => {
      void this.handleUpgrade(req, socket);
    });
  }

  listen(): Promise<number> {
    return new Promise((resolve) => {
      this.server.listen(0, "127.0.0.1", () => {
        resolve((this.server.address() as AddressInfo).port);
      });
    });
  }

  get listenPort(): number {
    const address = this.server.address() as AddressInfo | null;
    return address ? address.port : 0;
  }

  async close(): Promise<void> {
    for (const session of this.sessions) {
      session.alive = false;
      session.socket.destroy();
    }
    this.sessions.clear();
    await new Promise<void>((resolve) => this.server.close(() => resolve()));
  }

  mintToken(ttlMs = 30_000): string {
    const body = crypto
      .randomBytes(16)
      .toString("base64url")
      .replace(/[^A-Za-z0-9_-]/g, "")
      .slice(0, 21);
    const token = body + TOKEN_LAST_CHARS[crypto.randomInt(TOKEN_LAST_CHARS.length)];
    this.tokens.set(token, { expiresAt: Date.now() + ttlMs, consumed: false });
    return token;
  }

  /** 测试直发：对已认证会话发任意 data payload（ack / CommandAck 注入）。 */
  pushPayload(payload: unknown): void {
    for (const session of this.sessions) {
      if (session.authenticated && session.alive) {
        this.write(session, encodeServerFrame(JSON.stringify({ type: "data", payload })));
      }
    }
  }

  private handleUpgrade(req: http.IncomingMessage, socket: Socket): void {
    const url = new URL(req.url ?? "/", "http://fake-relay.invalid");
    this.stats.wsUpgrades += 1;
    if (url.pathname !== "/api/v1/remote/ws2") {
      socket.destroy();
      return;
    }
    // FAKE 策略：升级必成，token 校验放消息层（同 B1/B2 fake 形状）。
    const key = req.headers["sec-websocket-key"];
    if (typeof key !== "string") {
      socket.destroy();
      return;
    }
    const accept = crypto
      .createHash("sha1")
      .update(key + WS_GUID)
      .digest("base64");
    socket.write(
      "HTTP/1.1 101 Switching Protocols\r\n" +
        "Upgrade: websocket\r\n" +
        "Connection: Upgrade\r\n" +
        `Sec-WebSocket-Accept: ${accept}\r\n\r\n`
    );
    const session: ClientSession = { socket, authenticated: false, alive: true };
    this.sessions.add(session);
    const token = url.searchParams.get("token");
    socket.on(
      "data",
      createFrameParser((text) => this.onClientText(session, token, text))
    );
    socket.on("close", () => {
      session.alive = false;
      this.sessions.delete(session);
    });
    socket.on("error", () => {
      session.alive = false;
    });
  }

  private onClientText(session: ClientSession, token: string | null, text: string): void {
    let msg: { type?: unknown; device_sid?: unknown; payload?: unknown };
    try {
      msg = JSON.parse(text);
    } catch {
      return;
    }
    if (msg.type === "auth_init") {
      const entry = token ? this.tokens.get(token) : undefined;
      const valid = !!entry && !entry.consumed && entry.expiresAt > Date.now() && !!msg.device_sid;
      if (!valid) {
        this.write(
          session,
          encodeServerFrame(
            JSON.stringify({ type: "error", code: "AUTH_FAILED", message: "fake-relay" })
          )
        );
        this.closeSession(session);
        return;
      }
      entry.consumed = true;
      session.authenticated = true;
      this.write(
        session,
        encodeServerFrame(
          JSON.stringify({
            type: "auth_ack",
            device_sid: msg.device_sid,
            pair_status: "matched",
            server_ts: Math.floor(Date.now() / 1000),
          })
        )
      );
      return;
    }
    if (msg.type === "data" && session.authenticated) {
      this.stats.dataAccepted += 1;
      if (msg.payload && typeof msg.payload === "object") {
        this.clientDataPayloads.push(msg.payload as Record<string, unknown>);
      }
    }
  }

  private write(session: ClientSession, frame: Buffer): void {
    if (!session.alive) return;
    try {
      session.socket.write(frame);
    } catch {
      session.alive = false;
    }
  }

  private closeSession(session: ClientSession): void {
    if (!session.alive) return;
    session.alive = false;
    try {
      session.socket.write(Buffer.from([0x88, 0x02, 0x03, 0xe8]));
      session.socket.end();
    } catch {
      session.socket.destroy();
    }
  }
}

function createFrameParser(onText: (text: string) => void): (chunk: Buffer) => void {
  let buffer = Buffer.alloc(0);
  return (chunk: Buffer) => {
    buffer = Buffer.concat([buffer, chunk]);
    for (;;) {
      if (buffer.length < 2) return;
      const opcode = buffer[0] & 0x0f;
      const masked = (buffer[1] & 0x80) !== 0;
      let len = buffer[1] & 0x7f;
      let offset = 2;
      if (len === 126) {
        if (buffer.length < 4) return;
        len = buffer.readUInt16BE(2);
        offset = 4;
      } else if (len === 127) {
        if (buffer.length < 10) return;
        len = Number(buffer.readBigUInt64BE(2));
        offset = 10;
      }
      const maskLen = masked ? 4 : 0;
      if (buffer.length < offset + maskLen + len) return;
      let payload = buffer.subarray(offset + maskLen, offset + maskLen + len);
      if (masked) {
        const mask = buffer.subarray(offset, offset + 4);
        const unmasked = Buffer.alloc(len);
        for (let i = 0; i < len; i += 1) {
          unmasked[i] = payload[i] ^ mask[i % 4];
        }
        payload = unmasked;
      }
      buffer = buffer.subarray(offset + maskLen + len);
      if (opcode === 0x1) onText(payload.toString("utf8"));
      else if (opcode === 0x8) return;
    }
  };
}

function encodeServerFrame(text: string): Buffer {
  const payload = Buffer.from(text, "utf8");
  if (payload.length < 126) {
    return Buffer.concat([Buffer.from([0x81, payload.length]), payload]);
  }
  const header = Buffer.alloc(4);
  header[0] = 0x81;
  header[1] = 126;
  header.writeUInt16BE(payload.length, 2);
  return Buffer.concat([header, payload]);
}

// ---------------------------------------------------------------------------
// fake 固定值与脚手架
// ---------------------------------------------------------------------------

const FAKE_IDENTITY = { bridgeSessionId: "fake-bridge-b3-0001", bridgeGeneration: 1 };
const FAKE_TARGET = {
  taskId: "b3-fake-task-hi-0001",
  workspacePath: "/Users/fake/Travel-workspace",
  workspaceKey: "/Users/fake/Travel-workspace",
};
const SIBLING_SESSION = "b3-fake-task-sibling-0002";

/** 官方 snapshot / deltas 的最小会话帧（形状钉 source cites，见文件头）。 */
function topicFrame(args: {
  sessionId: string;
  pendingInteractions?: unknown[];
  control?: Record<string, unknown>;
}): Record<string, unknown> {
  return {
    topic: `conversation/${args.sessionId}`,
    subscriptionId: "fake-sub-b3",
    fromSeq: 0,
    toSeq: 1,
    sentAt: 0,
    payload: {
      kind: "snapshot",
      snapshot: {
        protocolVersion: 1,
        sessionId: args.sessionId,
        logEpoch: "fake-log-epoch",
        seq: 1,
        revision: 1,
        ...(args.control ? { control: args.control } : {}),
        ...(args.pendingInteractions ? { pendingInteractions: args.pendingInteractions } : {}),
      },
    },
  };
}

function deltasTopicFrame(args: {
  sessionId: string;
  patch: Record<string, unknown>;
}): Record<string, unknown> {
  return {
    topic: `conversation/${args.sessionId}`,
    subscriptionId: "fake-sub-b3",
    fromSeq: 1,
    toSeq: 2,
    sentAt: 0,
    payload: { kind: "deltas", deltas: [{ op: "state.updated", patch: args.patch }] },
  };
}

/** 权限 pending 卡（形状 = snapshot.ts:316-355 的最小子集；合成值）。 */
function permissionPending(args: {
  interactionId: string;
  toolCallId: string;
  anchorRowId: number;
}): Record<string, unknown> {
  return {
    interactionId: args.interactionId,
    kind: "permission",
    anchorRowId: args.anchorRowId,
    createdAt: 0,
    payload: {
      kind: "permission",
      toolCallId: args.toolCallId,
      toolName: "Write",
      summary: "fake: write a scratch file",
      detail: { file_path: "/tmp/b3-fake.md" },
      freeText: true,
      options: [
        { optionId: "allowOnce", label: "Allow once", kind: "allowOnce" },
        { optionId: "denyOnce", label: "Deny", kind: "deny" },
      ],
    },
  };
}

/** 问答 pending 卡（AskUserQuestion 形态；单选 / 多选两题）。 */
function userInputPending(args: {
  interactionId: string;
  toolCallId: string;
}): Record<string, unknown> {
  return {
    interactionId: args.interactionId,
    kind: "userInput",
    anchorRowId: 8,
    createdAt: 0,
    payload: {
      kind: "userInput",
      prompt: "fake: pick answers",
      freeText: true,
      toolCallId: args.toolCallId,
      toolName: "AskUserQuestion",
      questions: [
        {
          question: "Which color?",
          header: "Color",
          options: [
            { value: "Red", label: "Red", description: "fake" },
            { value: "Blue", label: "Blue", description: "fake" },
          ],
        },
        {
          question: "Which sizes?",
          header: "Size",
          multiSelect: true,
          options: [
            { value: "S", label: "S" },
            { value: "M", label: "M" },
          ],
        },
      ],
    },
  };
}

const realSleep = (ms: number) => new Promise((resolve) => setTimeout(resolve, ms));

function decodeOutboundEnvelope(
  relay: FakeRelay,
  index: number
): {
  method: string | null;
  channelName: string | null;
  params: {
    workspacePath?: unknown;
    envelope?: {
      commandId: string;
      clientId: string;
      sessionId: string;
      type: string;
      payload: Record<string, unknown>;
      issuedAt: number;
    };
  };
} | null {
  const payload = relay.clientDataPayloads[index];
  if (!payload) return null;
  const bytes = Uint8Array.from(Buffer.from(String(payload.dataBase64), "base64"));
  const decoded = decodeChannelMessage(bytes);
  if (!decoded || decoded.headerType !== REQUEST_TYPE_PROMISE) return null;
  return {
    method: decoded.method,
    channelName: decoded.channelName,
    params: (decoded.body as unknown[])[0] as never,
  };
}

/** CommandAck 应答帧（rpc-frame 形态，CRC32 真值；status/reasonCode 按 commandAckSchema）。 */
function pushCommandAck(
  relay: FakeRelay,
  requestId: number,
  ack: { commandId: string; status: string; reasonCode?: string },
  seq: number,
  messageSeq: number
): void {
  const bytes = encodeChannelResponse({
    responseType: RESPONSE_TYPE_PROMISE_SUCCESS,
    id: requestId,
    body: { ...ack, revisionAtDecision: 1 },
  });
  relay.pushPayload(encodeRpcFrame({ identity: FAKE_IDENTITY, seq, messageSeq, bytes }));
}

describe("zcode 官方桥接：审批/问答/打断逐能力 fake 合同（B3）", () => {
  let relay: FakeRelay;
  const connections: RelayTerminalConnection[] = [];

  afterEach(async () => {
    for (const conn of connections.splice(0)) {
      conn.close();
    }
    if (relay) await relay.close();
  });

  async function connectPaired(): Promise<RelayTerminalConnection> {
    relay = new FakeRelay();
    await relay.listen();
    const conn = new RelayTerminalConnection({
      relayWsUrl: `ws://127.0.0.1:${relay.listenPort}/api/v1/remote/ws2`,
      token: relay.mintToken(),
      deviceSid: relay.deviceSid,
    });
    connections.push(conn);
    expect(await conn.connect()).toBe("paired");
    return conn;
  }

  it("pending_binds_session_turn_interaction_epoch：pending 绑定 session/interaction/toolCall（turn 锚）与连接代际；异会话帧不收录；deltas 键级替换", async () => {
    const book = new PendingBook({ sessionId: FAKE_TARGET.taskId, generation: 1 });

    // snapshot：本会话 pending 卡 → 收录且记录绑定四元组。
    const snap = topicFrame({
      sessionId: FAKE_TARGET.taskId,
      pendingInteractions: [
        permissionPending({
          interactionId: "b3-fake-interaction-a",
          toolCallId: "b3-fake-toolcall-a",
          anchorRowId: 7,
        }),
      ],
    });
    const acceptedSnap = book.acceptTopicFrame(snap);
    expect(acceptedSnap.accepted).toBe(true);
    const pendingA = book.find("b3-fake-interaction-a");
    expect(pendingA).not.toBeNull();
    if (pendingA) {
      expect(pendingA.sessionId).toBe(FAKE_TARGET.taskId);
      expect(pendingA.generation).toBe(1);
      expect(pendingA.kind).toBe("permission");
      expect(pendingA.toolCallId).toBe("b3-fake-toolcall-a");
      expect(pendingA.anchorRowId).toBe(7);
    }

    // 兄弟会话 topic 帧：不收录（binding：异会话 pending 不进本册）。
    const foreign = book.acceptTopicFrame(
      topicFrame({
        sessionId: SIBLING_SESSION,
        pendingInteractions: [
          permissionPending({
            interactionId: "b3-fake-interaction-sibling",
            toolCallId: "b3-fake-toolcall-s",
            anchorRowId: 9,
          }),
        ],
      })
    );
    expect(foreign.accepted).toBe(false);
    expect(book.find("b3-fake-interaction-sibling")).toBeNull();
    expect(book.list()).toHaveLength(1);
    expect(book.counters.foreignTopicFrames).toBe(1);

    // deltas（state.updated 键级整体替换 pendingInteractions，delta.ts:50-53）：
    // 替换为 A + B 两张卡 → B 出现；再替换为空 → 全清（与官方投影同语义）。
    book.acceptTopicFrame(
      deltasTopicFrame({
        sessionId: FAKE_TARGET.taskId,
        patch: {
          pendingInteractions: [
            permissionPending({
              interactionId: "b3-fake-interaction-a",
              toolCallId: "b3-fake-toolcall-a",
              anchorRowId: 7,
            }),
            userInputPending({
              interactionId: "b3-fake-interaction-b",
              toolCallId: "b3-fake-toolcall-b",
            }),
          ],
        },
      })
    );
    expect(book.list()).toHaveLength(2);
    book.acceptTopicFrame(
      deltasTopicFrame({ sessionId: FAKE_TARGET.taskId, patch: { pendingInteractions: [] } })
    );
    expect(book.list()).toHaveLength(0);

    // 形状不符的帧（非 topic frame / 坏 topic 后缀）→ 拒收不致命。
    expect(book.acceptTopicFrame({ topic: "conversation/", payload: null }).accepted).toBe(false);
    expect(book.acceptTopicFrame(null).accepted).toBe(false);
    // topic 主体解析（探针订阅路由用）：conversation/<sessionId> 的精确后缀。
    expect(decodeTopicFrameSubject(`conversation/${FAKE_TARGET.taskId}`)).toBe(FAKE_TARGET.taskId);
    expect(decodeTopicFrameSubject("sessions-index/x")).toBeNull();
    expect(decodeTopicFrameSubject("conversation/")).toBeNull();
  });

  it("wrong_or_consumed_resolution_is_rejected：错 id / 错会话 / 已消费 / 重复应答不改其他 pending；另一张卡仍可应答", async () => {
    const conn = await connectPaired();
    const book = new PendingBook({ sessionId: FAKE_TARGET.taskId, generation: 1 });
    book.acceptTopicFrame(
      topicFrame({
        sessionId: FAKE_TARGET.taskId,
        pendingInteractions: [
          permissionPending({
            interactionId: "b3-fake-interaction-a",
            toolCallId: "b3-fake-toolcall-a",
            anchorRowId: 7,
          }),
          permissionPending({
            interactionId: "b3-fake-interaction-b",
            toolCallId: "b3-fake-toolcall-b",
            anchorRowId: 8,
          }),
        ],
      })
    );

    // 错 id：三查闸拒绝，零 data 帧。
    expect(() =>
      authorizePendingResolution(book, "b3-fake-interaction-wrong", {
        sessionId: FAKE_TARGET.taskId,
        liveGeneration: 1,
      })
    ).toThrow(/unknown-interaction/);
    expect(relay.clientDataPayloads.length).toBe(0);

    // 错会话：envelope.sessionId 与册绑定不符 → 拒绝（不触任何卡）。
    expect(() =>
      authorizePendingResolution(book, "b3-fake-interaction-a", {
        sessionId: SIBLING_SESSION,
        liveGeneration: 1,
      })
    ).toThrow(/session-mismatch/);
    expect(relay.clientDataPayloads.length).toBe(0);

    // 正路应答 A：过闸即消费 → 恰一帧；ack accepted。
    const answerA = buildPermissionAnswer({ optionId: "allowOnce" });
    const resolvedA = authorizePendingResolution(book, "b3-fake-interaction-a", {
      sessionId: FAKE_TARGET.taskId,
      liveGeneration: 1,
    });
    expect(resolvedA.interactionId).toBe("b3-fake-interaction-a");
    const loopA = new InteractionCommandLoop({
      sender: conn,
      identity: FAKE_IDENTITY,
      target: FAKE_TARGET,
      commandType: "resolveInteraction",
      payload: { interactionId: resolvedA.interactionId, answer: answerA },
      clientId: "b3-fake-client",
      ackMs: 1_500,
      sleep: realSleep,
    });
    conn.onData = (payload) => loopA.onData(payload);
    const outcomeAPromise = loopA.run();
    await realSleep(60);
    expect(relay.clientDataPayloads.length).toBe(1);
    pushCommandAck(relay, 1, { commandId: loopA.sentCommandId, status: "accepted" }, 1, 1);
    const outcomeA = await outcomeAPromise;
    expect(outcomeA).toEqual({ kind: "accepted", ackStatus: "accepted" });
    expect(loopA.framesSent).toBe(1);
    expect(book.find("b3-fake-interaction-a")).toBeNull();

    // 重复应答 A：已消费 → 闸拒绝，零增量帧。
    expect(() =>
      authorizePendingResolution(book, "b3-fake-interaction-a", {
        sessionId: FAKE_TARGET.taskId,
        liveGeneration: 1,
      })
    ).toThrow(/already-consumed/);
    expect(relay.clientDataPayloads.length).toBe(1);

    // 另一张卡 B 不被 A 的错 id / 重复应答波及：仍可应答，帧里带 B 的原题 interactionId。
    const resolvedB = authorizePendingResolution(book, "b3-fake-interaction-b", {
      sessionId: FAKE_TARGET.taskId,
      liveGeneration: 1,
    });
    expect(resolvedB.interactionId).toBe("b3-fake-interaction-b");
    const loopB = new InteractionCommandLoop({
      sender: conn,
      identity: FAKE_IDENTITY,
      target: FAKE_TARGET,
      commandType: "resolveInteraction",
      payload: {
        interactionId: resolvedB.interactionId,
        answer: buildPermissionAnswer({ optionId: "denyOnce", freeText: "not now" }),
      },
      clientId: "b3-fake-client",
      ackMs: 1_500,
      sleep: realSleep,
      channelRequestId: 2,
      firstSeq: 2,
      firstMessageSeq: 2,
    });
    // conn.onData 是单槽：换 loop 接管（B2 同款纪律）。
    conn.onData = (payload) => loopB.onData(payload);
    const outcomeBPromise = loopB.run();
    await realSleep(60);
    expect(relay.clientDataPayloads.length).toBe(2);
    pushCommandAck(relay, 2, { commandId: loopB.sentCommandId, status: "accepted" }, 2, 2);
    expect(await outcomeBPromise).toEqual({ kind: "accepted", ackStatus: "accepted" });
    const wireB = decodeOutboundEnvelope(relay, 1);
    expect(wireB?.method).toBe(AGENT_SEND_COMMAND_METHOD);
    expect(wireB?.channelName).toBe(AGENT_CHANNEL);
    expect(wireB?.params.workspacePath).toBe(FAKE_TARGET.workspacePath);
    expect(wireB?.params.envelope?.sessionId).toBe(FAKE_TARGET.taskId);
    expect(wireB?.params.envelope?.type).toBe("resolveInteraction");
    expect(wireB?.params.envelope?.payload).toEqual({
      interactionId: "b3-fake-interaction-b",
      answer: { optionId: "denyOnce", freeText: "not now" },
    });
  });

  it("late_response_cannot_release_new_epoch_pending：断开重连（新代际）后旧代际迟到应答不能释放新代际 pending", async () => {
    // 代际 1：连接 + pending 卡 + 发出应答（应答 ack 丢失 → unknown，不重发）。
    const conn1 = await connectPaired();
    const book1 = new PendingBook({ sessionId: FAKE_TARGET.taskId, generation: 1 });
    book1.acceptTopicFrame(
      topicFrame({
        sessionId: FAKE_TARGET.taskId,
        pendingInteractions: [
          permissionPending({
            interactionId: "b3-fake-interaction-re",
            toolCallId: "b3-fake-toolcall-re",
            anchorRowId: 7,
          }),
        ],
      })
    );
    const resolved1 = authorizePendingResolution(book1, "b3-fake-interaction-re", {
      sessionId: FAKE_TARGET.taskId,
      liveGeneration: 1,
    });
    const loop1 = new InteractionCommandLoop({
      sender: conn1,
      identity: FAKE_IDENTITY,
      target: FAKE_TARGET,
      commandType: "resolveInteraction",
      payload: {
        interactionId: resolved1.interactionId,
        answer: buildPermissionAnswer({ optionId: "allowOnce" }),
      },
      clientId: "b3-fake-client",
      ackMs: 150,
      sleep: realSleep,
    });
    conn1.onData = (payload) => loop1.onData(payload);
    expect(await loop1.run()).toEqual({ kind: "unknown" });
    expect(loop1.framesSent).toBe(1);
    const framesOnGen1 = relay.clientDataPayloads.length;
    expect(framesOnGen1).toBe(1);
    conn1.close();

    // 代际 2（重连）：同一 interactionId 重新出现（官方 broker 每秒按同一业务 requestId
    // 重发 pending，interaction-broker.ts:60,489-503——epoch 闸才是防线，不是 id）。
    const conn2 = new RelayTerminalConnection({
      relayWsUrl: `ws://127.0.0.1:${relay.listenPort}/api/v1/remote/ws2`,
      token: relay.mintToken(),
      deviceSid: relay.deviceSid,
    });
    connections.push(conn2);
    expect(await conn2.connect()).toBe("paired");
    const book2 = new PendingBook({ sessionId: FAKE_TARGET.taskId, generation: 2 });
    book2.acceptTopicFrame(
      topicFrame({
        sessionId: FAKE_TARGET.taskId,
        pendingInteractions: [
          permissionPending({
            interactionId: "b3-fake-interaction-re",
            toolCallId: "b3-fake-toolcall-re",
            anchorRowId: 7,
          }),
        ],
      })
    );

    // 旧代际应答闸：book1（generation 1）对 liveGeneration 2 的授权 → stale-epoch，
    // 连接 2 上零帧（迟到应答没有路径再进网络）。
    expect(() =>
      authorizePendingResolution(book1, "b3-fake-interaction-re", {
        sessionId: FAKE_TARGET.taskId,
        liveGeneration: 2,
      })
    ).toThrow(/stale-epoch/);
    expect(relay.clientDataPayloads.length).toBe(framesOnGen1);

    // 旧册的本地状态（已消费账）不影响新册：book1 再次消费 → already-consumed，
    // 而 book2 的同名 pending 仍在、仍可应答。
    expect(() => book1.consume("b3-fake-interaction-re")).toThrow(/already-consumed/);
    expect(book2.find("b3-fake-interaction-re")).not.toBeNull();

    // 新代际用自己的册重新过闸 + 应答 → accepted。
    const resolved2 = authorizePendingResolution(book2, "b3-fake-interaction-re", {
      sessionId: FAKE_TARGET.taskId,
      liveGeneration: 2,
    });
    expect(resolved2.generation).toBe(2);
    const loop2 = new InteractionCommandLoop({
      sender: conn2,
      identity: FAKE_IDENTITY,
      target: FAKE_TARGET,
      commandType: "resolveInteraction",
      payload: {
        interactionId: resolved2.interactionId,
        answer: buildPermissionAnswer({ optionId: "allowOnce" }),
      },
      clientId: "b3-fake-client",
      ackMs: 1_500,
      sleep: realSleep,
      channelRequestId: 2,
      firstSeq: 2,
      firstMessageSeq: 2,
    });
    conn2.onData = (payload) => loop2.onData(payload);
    const outcome2Promise = loop2.run();
    await realSleep(60);
    pushCommandAck(relay, 2, { commandId: loop2.sentCommandId, status: "accepted" }, 2, 2);
    expect(await outcome2Promise).toEqual({ kind: "accepted", ackStatus: "accepted" });
    expect(book2.find("b3-fake-interaction-re")).toBeNull();
    expect(relay.clientDataPayloads.length).toBe(framesOnGen1 + 1);
  });

  it("interrupt_scoped_to_target_turn：stop 恰好一帧、精确绑定目标会话与前台执行 id；兄弟会话零帧；守卫 noop 不重发；盲停拒绝", async () => {
    const conn = await connectPaired();

    // 守卫 id 的客户端可见来源 = control.activeWorks[].foregroundExecutionId
    // （snapshot.ts:53-64）。目标会话 control 带 primaryTurn 执行 id。
    const book = new PendingBook({ sessionId: FAKE_TARGET.taskId, generation: 1 });
    book.acceptTopicFrame(
      topicFrame({
        sessionId: FAKE_TARGET.taskId,
        control: {
          phase: "running",
          canStop: true,
          activeWorks: [
            { kind: "primaryTurn", foregroundExecutionId: "b3-fake-exec-turn-1", startedAt: 0 },
          ],
        },
      })
    );
    const stopTarget = book.observeStopTarget();
    expect(stopTarget?.foregroundExecutionId).toBe("b3-fake-exec-turn-1");
    const stopPayload = buildStopPayload(stopTarget);
    expect(stopPayload).toEqual({ expectedForegroundExecutionId: "b3-fake-exec-turn-1" });

    // 无执行 id 时的盲停拒绝（fail-closed：宁不 stop 也不错杀）。
    expect(() => buildStopPayload(null)).toThrow(/stop-requires-foreground-execution/);

    // stop 发送：恰一帧；envelope.sessionId === 目标会话；payload 携带执行 id；
    // 兄弟会话 id 不出现在任何出站帧字节里。
    const stopLoop = new InteractionCommandLoop({
      sender: conn,
      identity: FAKE_IDENTITY,
      target: FAKE_TARGET,
      commandType: "stop",
      payload: stopPayload,
      clientId: "b3-fake-client",
      ackMs: 1_500,
      sleep: realSleep,
    });
    conn.onData = (payload) => stopLoop.onData(payload);
    const stopPromise = stopLoop.run();
    await realSleep(60);
    expect(relay.clientDataPayloads.length).toBe(1);
    const wire = decodeOutboundEnvelope(relay, 0);
    expect(wire?.method).toBe(AGENT_SEND_COMMAND_METHOD);
    expect(wire?.params.envelope?.type).toBe("stop");
    expect(wire?.params.envelope?.sessionId).toBe(FAKE_TARGET.taskId);
    expect(wire?.params.envelope?.payload).toEqual({
      expectedForegroundExecutionId: "b3-fake-exec-turn-1",
    });
    for (const frame of relay.clientDataPayloads) {
      const raw = Buffer.from(String(frame.dataBase64), "base64").toString("utf8");
      expect(raw.includes(SIBLING_SESSION)).toBe(false);
    }
    pushCommandAck(relay, 1, { commandId: stopLoop.sentCommandId, status: "accepted" }, 1, 1);
    expect(await stopPromise).toEqual({ kind: "accepted", ackStatus: "accepted" });
    expect(stopLoop.framesSent).toBe(1);

    // 迟到 stop 守卫：runtime 已翻新执行 → ack noop（guard.stopTargetChanged，
    // session-flow.ts:292-360 的 V4CommandNoopError）→ 结果记 noop、恰好一帧、
    // 无重发（不把 noop 当成功，也不补发）。
    const lateLoop = new InteractionCommandLoop({
      sender: conn,
      identity: FAKE_IDENTITY,
      target: FAKE_TARGET,
      commandType: "stop",
      payload: { expectedForegroundExecutionId: "b3-fake-exec-turn-1" },
      clientId: "b3-fake-client",
      ackMs: 1_500,
      sleep: realSleep,
      channelRequestId: 2,
      firstSeq: 2,
      firstMessageSeq: 2,
    });
    conn.onData = (payload) => lateLoop.onData(payload);
    const latePromise = lateLoop.run();
    await realSleep(60);
    pushCommandAck(
      relay,
      2,
      { commandId: lateLoop.sentCommandId, status: "noop", reasonCode: "guard.stopTargetChanged" },
      2,
      3
    );
    expect(await latePromise).toEqual({ kind: "noop", reasonCode: "guard.stopTargetChanged" });
    expect(lateLoop.framesSent).toBe(1);
    await realSleep(80);
    expect(relay.clientDataPayloads.length).toBe(2);

    // 盲 stop（无 expectedForegroundExecutionId）在客户端侧即拒绝，零帧：
    // 本参照实现把「守卫 id 必带」定为探针纪律（源码允许缺省，但缺省语义 = 无守卫，
    // 误杀窗口不可控；探针只做有守卫的精确打断）。
    const blindLoop = new InteractionCommandLoop({
      sender: conn,
      identity: FAKE_IDENTITY,
      target: FAKE_TARGET,
      commandType: "stop",
      payload: {},
      clientId: "b3-fake-client",
      ackMs: 200,
      sleep: realSleep,
      channelRequestId: 3,
      firstSeq: 3,
      firstMessageSeq: 3,
    });
    await expect(blindLoop.run()).rejects.toThrow(/stop-requires-foreground-execution/);
    expect(relay.clientDataPayloads.length).toBe(2);
  });

  it("readonly_view_survives_interaction_flows：映射 listing 与用户草稿等只读状态在 pending/应答/stop 全流程中不变", async () => {
    const conn = await connectPaired();

    // 只读视图 = B2 会话映射的 listing 输入 + 用户草稿（结构占位，与 V4 UI 的
    // elicitation 草稿同角色）。深冻结后跑全流程——任何写入都会在 strict 模式抛
    // TypeError，流程能跑完即证零写入。
    const workspaces = [{ workspacePath: "/Users/fake/Travel", label: "Travel", kind: "local" }];
    const tasks = [
      {
        address: { workspacePath: "/Users/fake/Travel", taskId: FAKE_TARGET.taskId },
        meta: { title: "hi" },
      },
    ];
    const drafts = {
      elicitationDrafts: {
        "b3-fake-interaction-q": { selectedValues: ["Red"], notes: "draft survives" },
      },
      markerHistory: ["[tuvis-b2-marker:2026-10-24T00:00:00.000Z]"],
    };
    const draftsFrozen = structuredClone(drafts);
    Object.freeze(drafts);
    Object.freeze(drafts.elicitationDrafts);
    Object.freeze((drafts.elicitationDrafts as Record<string, unknown>)["b3-fake-interaction-q"]);
    Object.freeze(drafts.markerHistory);
    workspaces.forEach((entry) => Object.freeze(entry));
    Object.freeze(workspaces);
    tasks.forEach((entry) => Object.freeze(entry));
    Object.freeze(tasks);

    const book = new PendingBook({ sessionId: FAKE_TARGET.taskId, generation: 1 });
    book.acceptTopicFrame(
      topicFrame({
        sessionId: FAKE_TARGET.taskId,
        pendingInteractions: [
          userInputPending({
            interactionId: "b3-fake-interaction-q",
            toolCallId: "b3-fake-toolcall-q",
          }),
        ],
      })
    );

    // 问答应答（原题 id / 原选项 value）：过闸 → 单帧 → accepted。
    const resolved = authorizePendingResolution(book, "b3-fake-interaction-q", {
      sessionId: FAKE_TARGET.taskId,
      liveGeneration: 1,
    });
    const questions = (
      resolved.payload as { questions?: Array<{ question: string; multiSelect?: boolean }> }
    ).questions as Array<{ question: string; multiSelect?: boolean }>;
    const answer = buildUserInputAnswer([
      { questionText: questions[0]!.question, selectedValues: ["Red"], multiSelect: false },
      { questionText: questions[1]!.question, selectedValues: ["S", "M"], multiSelect: true },
    ]);
    expect(answer.action).toBe("accept");
    expect(answer.content.answers).toEqual({
      "Which color?": "Red",
      "Which sizes?": "S, M",
    });
    // 双题应答只有 answer_<index> 兼容键；单题才有 answer 键（ElicitationDialog.tsx:287-300）。
    expect("answer" in answer.content).toBe(false);
    expect(answer.content.answer_1).toEqual(["S", "M"]);

    const qLoop = new InteractionCommandLoop({
      sender: conn,
      identity: FAKE_IDENTITY,
      target: FAKE_TARGET,
      commandType: "resolveInteraction",
      payload: { interactionId: resolved.interactionId, answer },
      clientId: "b3-fake-client",
      ackMs: 1_500,
      sleep: realSleep,
    });
    conn.onData = (payload) => qLoop.onData(payload);
    const qPromise = qLoop.run();
    await realSleep(60);
    pushCommandAck(relay, 1, { commandId: qLoop.sentCommandId, status: "accepted" }, 1, 1);
    expect(await qPromise).toEqual({ kind: "accepted", ackStatus: "accepted" });

    // stop 也走一遍（同一会话另一伪执行 id）。
    const sLoop = new InteractionCommandLoop({
      sender: conn,
      identity: FAKE_IDENTITY,
      target: FAKE_TARGET,
      commandType: "stop",
      payload: { expectedForegroundExecutionId: "b3-fake-exec-turn-2" },
      clientId: "b3-fake-client",
      ackMs: 1_500,
      sleep: realSleep,
      channelRequestId: 2,
      firstSeq: 2,
      firstMessageSeq: 2,
    });
    conn.onData = (payload) => sLoop.onData(payload);
    const sPromise = sLoop.run();
    await realSleep(60);
    pushCommandAck(relay, 2, { commandId: sLoop.sentCommandId, status: "accepted" }, 2, 2);
    expect(await sPromise).toEqual({ kind: "accepted", ackStatus: "accepted" });

    // 只读视图逐字节不变。
    expect(drafts).toEqual(draftsFrozen);
    expect(Object.isFrozen(drafts)).toBe(true);
    expect(workspaces).toHaveLength(1);
    expect(tasks).toHaveLength(1);
  });
});

// ---------------------------------------------------------------------------
// wire 形状自检与应答构造器（纯函数；不依赖 relay fixture）
// ---------------------------------------------------------------------------

describe("B3 wire 形状与应答构造器（@ 3c5b2276 行号锁形）", () => {
  it("fullAccess 特权选项：assertNotFullAccessOption 硬闸拒绝（探针红线，snapshot.ts:234）", () => {
    expect(() => assertNotFullAccessOption("fullAccess")).toThrow(/full-access-forbidden/);
    expect(() => assertNotFullAccessOption("allowOnce")).not.toThrow();
  });

  it("权限应答 = {optionId, freeText?}；拒绝路径带反馈 freeText（V4InteractionDialogs.tsx:358-363）", () => {
    expect(buildPermissionAnswer({ optionId: "allowOnce" })).toEqual({ optionId: "allowOnce" });
    expect(buildPermissionAnswer({ optionId: "denyOnce", freeText: "用别的名字" })).toEqual({
      optionId: "denyOnce",
      freeText: "用别的名字",
    });
  });

  it("问答应答 = {action, content}：answers 按题干全文为键、选项 value 原样回传；多选数组 / 单选字符串", () => {
    const multi = buildUserInputAnswer([
      { questionText: "Q1", selectedValues: ["A"], multiSelect: false },
      { questionText: "Q2", selectedValues: ["X", "Y"], multiSelect: true },
    ]);
    expect(multi).toEqual({
      action: "accept",
      content: {
        answers: { Q1: "A", Q2: "X, Y" },
        answer_0: "A",
        answer_1: ["X", "Y"],
      },
    });
    const single = buildUserInputAnswer([
      { questionText: "Only?", selectedValues: ["Blue"], multiSelect: false },
    ]);
    expect(single.content.answer).toBe("Blue");
    expect(single.content.answers).toEqual({ "Only?": "Blue" });
    expect(
      buildUserInputAnswer([{ questionText: "Q", selectedValues: [], multiSelect: false }])
    ).toEqual({ action: "accept", content: { answers: {} } });
  });

  it("ObservedPending 原样保留 pending 卡 payload（不虚构字段；应答字段全部来自卡上原值）", () => {
    const book = new PendingBook({ sessionId: FAKE_TARGET.taskId, generation: 3 });
    book.acceptTopicFrame(
      topicFrame({
        sessionId: FAKE_TARGET.taskId,
        pendingInteractions: [
          permissionPending({
            interactionId: "b3-fake-interaction-raw",
            toolCallId: "b3-fake-toolcall-raw",
            anchorRowId: 3,
          }),
        ],
      })
    );
    const pending: ObservedPending = book.find("b3-fake-interaction-raw")!;
    expect(pending.payload.toolCallId).toBe("b3-fake-toolcall-raw");
    expect(
      (pending.payload.options as Array<{ optionId: string }>).map((option) => option.optionId)
    ).toEqual(["allowOnce", "denyOnce"]);
    expect(pending.generation).toBe(3);
  });
});
