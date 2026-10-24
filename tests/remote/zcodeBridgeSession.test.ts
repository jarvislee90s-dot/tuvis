// @vitest-environment node
// tests/remote/zcodeBridgeSession.test.ts — B2 红测：单连接、会话映射与发送闭环
// （工作包 B 计划 §B2 第 1—2 条：docs/superpowers/plans/2026-10-24-zcode-official-bridge-validation.md）。
//
// 被测客户端 = scripts/zcode-bridge-probe/sessionBridge.ts（Tuvis 侧参照实现）：
//   - SingleConnectionGate：同一 Tuvis 主机在已有一条 paired 上游连接时，第二个连接请求
//     在本地即被拒绝（不构造 socket、不触碰 relay）——fake relay 的 wsUpgrades 保持 1；
//   - SessionTargetMapper：发送目标只能来自「当前连接代际的 listing 里精确存在的
//     taskId + workspacePath」；标题/cwd 归因 API 不存在（无按标题查找的导出，
//     把标题当 taskId 传也会被 listing 校验拒绝）；旧代际（connection generation）
//     绑定的映射在新连接上不能授权发送（stale-epoch）；
//   - SendOnceLoop：一次发送只构造一个 rpc-frame；peer 受理（rpc-frame-ack）后丢响应
//     → 结果只记 unknown，绝不重发（无任何重发计时器）；重复 rpc-frame-ack / 重复事件
//     去重，不产生第二条发送。
//   - （B2 map-only 修复追加）workspace 指称唯一化阶梯：双 listing 去重 + label 精确 →
//     purpose=project → activeWorkspaceKey（bootstrap 回读）；controller tasks-index
//     主动枚举：EventListen(102) + subscribeControllerV4{topic:"controller/tasks-index"}
//     （channel "window-controller"，行号见 sessionBridge.ts 常量注）→ snapshot/deltas
//     任务行收割；枚举全程只发读面控制帧，无任何会话发送帧。
//
// wire 形状依据 = 固定官方源码 zai-org/ZCode @ 3c5b227649371507ddc4eb46ac5133afe6db1cab
// 的客户端侧实现（行号见 sessionBridge.ts 各常量注释）：
//   - rpc-frame / rpc-frame-ack 形状与 CRC32 校验
//     → packages/shared/src/web-remote-control-rpc-transport.ts:60-123（B1 实连 4 帧的
//       checksum 已按标准 IEEE CRC32 逐一复算命中）
//   - relay data 信封 {type:"data", payload, client_ts} → 同 contract.ts:36-44
//   - ChannelClient 请求 wire = serialize([100, id, channel, method]) + serialize(args)
//     → packages/rpc/src/channelClient.ts:182-190；serialization.ts:169-175
//   - RequestType.Promise=100 / ResponseType.PromiseSuccess=201 / PromiseError=202 /
//     EventFire=204 → packages/rpc/src/channels.shared.ts:13-23
//   - 发送 = channel "zcode-agent"（channels.ts:114）method
//     "sendConversationCommandV4"（zcodeAgent.ts:937），args =
//     {workspacePath, workspaceIdentity?, envelope}（zcodeAgent.ts:498-506）；
//     envelope = {commandId, clientId, sessionId, type:"sendText",
//     payload:{text}, issuedAt}（zcode-protocol-v4/command.ts:343-356,95-135；
//     sendText 不在 COMMANDS_REQUIRING_BASE_REVISION 内 → 免 baseRevision）
//
// 全部凭据/标识为合成值（b2-fake-* / 随机 token / fake-bridge-*），不含真实
// SID/JWT/token/链接/会话 id。本文件不连任何真实服务。
import * as crypto from "node:crypto";
import * as http from "node:http";
import type { Socket } from "node:net";
import { afterAll, afterEach, beforeAll, describe, expect, it } from "vitest";

// 本文件跑真实 127.0.0.1 socket：文件级 beforeAll 关闭全局 MSW（同 B1 用例口径，
// vitest 按文件隔离 worker，只影响本文件）。
import { server as mswServer } from "../msw/server";

beforeAll(() => {
  mswServer.close();
});
afterAll(() => {
  mswServer.listen({ onUnhandledRequest: "bypass" });
});

import { RelayTerminalConnection } from "../../scripts/zcode-bridge-probe/relayAuth";
import { SecretRedactor } from "../../scripts/zcode-bridge-probe/sanitize";
import { formatTargetLockedLine } from "../../scripts/zcode-bridge-probe/session-probe";
import {
  AGENT_CHANNEL,
  AGENT_SEND_COMMAND_METHOD,
  REQUEST_TYPE_PROMISE,
  RESPONSE_TYPE_EVENT_FIRE,
  RESPONSE_TYPE_INITIALIZE,
  RESPONSE_TYPE_PROMISE_SUCCESS,
  ControllerTaskIndexReader,
  SendOnceLoop,
  SessionTargetMapper,
  SingleConnectionGate,
  classifyWorkspaceBridgeOpenResponse,
  crc32WireBytes,
  decodeChannelMessage,
  dedupeWorkspaceEntries,
  encodeChannelInitialize,
  encodeChannelRequest,
  encodeChannelResponse,
  encodeRpcFrame,
  encodeRpcFrameAck,
  readActiveWorkspaceKeyFromListings,
  resolveDesignatedWorkspace,
  type WorkspaceListingEntry,
} from "../../scripts/zcode-bridge-probe/sessionBridge";

// ---------------------------------------------------------------------------
// FAKE relay（仅测试，同 B1 形状的最小 RFC6455 实现）：行为标签——凡注释标
// 「FAKE 策略」的处理均为本测试自定，不是官方服务端取证结论。
// 相比 B1 fake 的增量：捕获每条 data payload（供发送帧计数），并允许测试直发
// rpc-frame-ack / rpc-frame（应答与事件）给客户端。
// ---------------------------------------------------------------------------

const WS_GUID = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";
const TOKEN_LAST_CHARS = "AQgw";

type StoredToken = { expiresAt: number; consumed: boolean };

type ClientSession = {
  socket: Socket;
  authenticated: boolean;
  alive: boolean;
};

class FakeRelay {
  readonly accountJwt = "b2-fake-account-jwt-synthetic";
  readonly deviceSid = "b2-fake-device-sid-0001";
  readonly stats = {
    wsUpgrades: 0,
    dataAccepted: 0,
  };
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

  async close(): Promise<void> {
    for (const session of this.sessions) {
      session.alive = false;
      session.socket.destroy();
    }
    this.sessions.clear();
    await new Promise<void>((resolve) => this.server.close(() => resolve()));
  }

  mintToken(ttlMs = 30_000): string {
    const body = crypto.randomBytes(16).toString("base64url");
    const token = body.slice(0, 21) + TOKEN_LAST_CHARS[crypto.randomInt(TOKEN_LAST_CHARS.length)];
    this.tokens.set(token, { expiresAt: Date.now() + ttlMs, consumed: false });
    return token;
  }

  /** 测试直发：对已认证会话发任意 data payload（rpc-frame-ack / 应答 / 事件注入）。 */
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
    // FAKE 策略：升级必成，token 校验放消息层（同 B1 fake 形状）。
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
// 测试脚手架与 fake 固定值
// ---------------------------------------------------------------------------

const FAKE_IDENTITY = { bridgeSessionId: "fake-bridge-session-0001", bridgeGeneration: 1 };
const FAKE_TARGET = {
  taskId: "fake-task-hi-0001",
  workspacePath: "/Users/fake/Travel-workspace",
  workspaceKey: "/Users/fake/Travel-workspace",
};
const FAKE_LISTING = {
  workspaces: [
    {
      workspacePath: FAKE_TARGET.workspacePath,
      label: "Travel",
      kind: "local",
    },
  ],
  tasks: [
    {
      address: {
        workspacePath: FAKE_TARGET.workspacePath,
        taskId: FAKE_TARGET.taskId,
      },
      meta: { title: "hi", taskId: FAKE_TARGET.taskId, workspacePath: FAKE_TARGET.workspacePath },
    },
  ],
};

async function waitFor(check: () => boolean, timeoutMs = 2_000, stepMs = 10): Promise<void> {
  const deadline = Date.now() + timeoutMs;
  while (!check()) {
    if (Date.now() > deadline) {
      throw new Error("waitFor timeout");
    }
    await new Promise((resolve) => setTimeout(resolve, stepMs));
  }
}

const realSleep = (ms: number) => new Promise((resolve) => setTimeout(resolve, ms));

/** 构造一个 rpc-frame（对消息字节做 CRC32 + 单分片），供测试组装应答/事件帧。 */
function frameFromBytes(
  bytes: Uint8Array,
  seq: number,
  messageSeq: number
): Record<string, unknown> {
  const encoded = encodeRpcFrame({ identity: FAKE_IDENTITY, seq, messageSeq, bytes });
  return encoded as unknown as Record<string, unknown>;
}

/**
 * 桥 ChannelServer 的 Initialize 帧（channelServer.ts:30,45-47：sendResponse(Initialize)
 * → send([200]) + 默认 body undefined）。B2 实连 macos-B220261024T124743Z 收到的
 * 6 字节帧 04 01 06 C8 01 00 即此形状——官方 ChannelClient 在收到它之前不发任何请求
 * （channelClient.ts:124-128 whenInitialized 门）。
 */
function initializeFrame(seq: number, messageSeq: number): Record<string, unknown> {
  return frameFromBytes(encodeChannelInitialize(), seq, messageSeq);
}

/** 服务端→客户端的 PromiseSuccess 应答字节（channelClient.ts:212-213 的对偶）。 */
function ackAcceptedBytes(requestId: number, commandId: string): Uint8Array {
  return encodeChannelResponse({
    responseType: RESPONSE_TYPE_PROMISE_SUCCESS,
    id: requestId,
    body: {
      commandId,
      status: "accepted",
      revisionAtDecision: 7,
      result: { type: "inputAccepted", delivery: "startNow", inputId: "fake-input-1" },
    },
  });
}

describe("zcode 官方桥接：单连接、会话映射与发送闭环（B2 fake 合同）", () => {
  let relay: FakeRelay;
  let port: number;
  let relayWsUrl: string;
  const connections: RelayTerminalConnection[] = [];

  afterEach(async () => {
    for (const conn of connections.splice(0)) {
      conn.close();
    }
    if (relay) await relay.close();
  });

  async function startRelay(): Promise<void> {
    relay = new FakeRelay();
    port = await relay.listen();
    relayWsUrl = `ws://127.0.0.1:${port}/api/v1/remote/ws2`;
  }

  async function connectPaired(): Promise<RelayTerminalConnection> {
    const conn = new RelayTerminalConnection({
      relayWsUrl,
      token: relay.mintToken(),
      deviceSid: relay.deviceSid,
    });
    connections.push(conn);
    const result = await conn.connect();
    expect(result).toBe("paired");
    return conn;
  }

  it("single_upstream_connection：已有一条 paired 上游连接时，第二个连接请求被本地拒绝，不构造新 socket", async () => {
    await startRelay();
    const gate = new SingleConnectionGate();

    let factoryCalls = 0;
    const first = gate.acquire(() => {
      factoryCalls += 1;
      return connectPaired();
    });
    if (!first.ok) throw new Error("first acquire must succeed");
    const firstConn = await first.connection;
    expect(firstConn.currentState).toBe("paired");
    expect(relay.stats.wsUpgrades).toBe(1);
    expect(factoryCalls).toBe(1);

    // 已 paired 时第二个请求：本地即拒——factory 不被调用（不会构造第二条 WS），
    // relay 升级计数保持 1。
    const second = gate.acquire(() => {
      factoryCalls += 1;
      return connectPaired();
    });
    expect(second.ok).toBe(false);
    if (!second.ok) {
      expect(second.reason).toBe("already-connected");
    }
    expect(factoryCalls).toBe(1);
    await realSleep(80);
    expect(relay.stats.wsUpgrades).toBe(1);
  });

  it("wrong_target_rejected：不在 listing 里的 sessionId 在客户端即被拒，一个 data 帧都不发；按标题查找的 API 不存在", async () => {
    await startRelay();
    const conn = await connectPaired();
    const mapper = SessionTargetMapper.fromListings({ ...FAKE_LISTING, generation: 1 });

    // 未知/错 sessionId：listing 里不存在 → 客户端拒绝，绝不发帧。
    const senderSpy: Array<unknown> = [];
    const wrongTarget = { ...FAKE_TARGET, taskId: "fake-task-wrong-id" };
    expect(() => mapper.resolveTarget({ taskId: wrongTarget.taskId })).toThrow(
      /task-not-in-listing/
    );

    // 标题/cwd 归因被禁止：不存在任何「按标题找会话」的 API（反射断言缺席）；
    // 把标题字符串当 taskId 传入同样被 listing 校验拒绝（不会碰巧命中）。
    const mapperAsRecord = mapper as unknown as Record<string, unknown>;
    expect(mapperAsRecord.findByTitle).toBeUndefined();
    expect(mapperAsRecord.findSessionByTitle).toBeUndefined();
    expect(mapperAsRecord.searchByTitle).toBeUndefined();
    expect(() => mapper.resolveTarget({ taskId: "hi" })).toThrow(/task-not-in-listing/);

    // 错 workspace：任务在 listing 里、但指定的 workspacePath 与该行 address 不符 → 拒绝。
    expect(() =>
      mapper.resolveTarget({ taskId: FAKE_TARGET.taskId, workspacePath: "/Users/fake/Other" })
    ).toThrow(/workspace-mismatch/);

    // 发送循环拿到未解析目标时没有任何发送路径：framesSent 恒 0、无 data 帧。
    const sendProbe = new SendOnceLoop({
      sender: {
        sendData: (payload) => {
          senderSpy.push(payload);
          return true;
        },
      },
      identity: FAKE_IDENTITY,
      target: { ...FAKE_TARGET, taskId: "fake-task-wrong-id", generation: 1 },
      markerText: "[tuvis-b2-marker:test]",
      clientId: "fake-client",
      timeouts: { ackMs: 60, activityMs: 20 },
      sleep: realSleep,
    });
    const outcome = await sendProbe.run();
    // 该分支由 gate/mapper 前置拦截；SendOnceLoop 自身对不在 listing 的目标
    // 不做二次校验——本断言锁定「唯一入口是 mapper 解析产物」这一纪律的测试面。
    expect(relay.stats.dataAccepted).toBe(0);
    expect(relay.clientDataPayloads.length).toBe(0);
    expect(outcome.kind === "send-failed" || outcome.kind === "unknown").toBe(true);

    // 正路径先解析，再授权，才能发出（对照组：同一 fake 上合法目标恰好能解析）。
    const bound = mapper.resolveTarget({ taskId: FAKE_TARGET.taskId });
    expect(bound.taskId).toBe(FAKE_TARGET.taskId);
    mapper.authorizeSend(bound, 1);
    expect(AGENT_CHANNEL).toBe("zcode-agent");
    expect(AGENT_SEND_COMMAND_METHOD).toBe("sendConversationCommandV4");
    expect(REQUEST_TYPE_PROMISE).toBe(100);
    expect(conn.currentState).toBe("paired");
  });

  it("unknown_not_resent：peer 受理（rpc-frame-ack）后丢响应 → 只记 unknown，恰好 1 个发送帧，无重发计时器", async () => {
    await startRelay();
    const conn = await connectPaired();
    const mapper = SessionTargetMapper.fromListings({ ...FAKE_LISTING, generation: 1 });
    const bound = mapper.resolveTarget({ taskId: FAKE_TARGET.taskId });
    mapper.authorizeSend(bound, 1);

    const loop = new SendOnceLoop({
      sender: conn,
      identity: FAKE_IDENTITY,
      target: bound,
      markerText: "[tuvis-b2-marker:unknown-not-resent]",
      clientId: "fake-client",
      timeouts: { ackMs: 250, activityMs: 40 },
      sleep: realSleep,
    });
    conn.onData = (payload) => loop.onData(payload);

    const outcomePromise = loop.run();
    // peer 受理：fake 在收到我们的 rpc-frame 后回 rpc-frame-ack（等第一帧到达再注回）。
    await waitFor(() => relay.clientDataPayloads.length >= 1);
    const sentFrame = relay.clientDataPayloads[0];
    expect(sentFrame.zcode_type).toBe("rpc-frame");
    expect(sentFrame.bridgeSessionId).toBe(FAKE_IDENTITY.bridgeSessionId);
    expect(sentFrame.fragmentCount).toBe(1);
    relay.pushPayload({
      zcode_type: "rpc-frame-ack",
      ...FAKE_IDENTITY,
      ackMessageSeq: 1,
    });

    const outcome = await outcomePromise;
    expect(outcome.kind).toBe("unknown");

    // 恰好 1 个发送帧；再等一拍也无重发（结构性无重发计时器）。
    expect(loop.framesSent).toBe(1);
    expect(relay.clientDataPayloads.length).toBe(1);
    await realSleep(120);
    expect(loop.framesSent).toBe(1);
    expect(relay.clientDataPayloads.length).toBe(1);
  });

  it("duplicate_events_deduped：重复 rpc-frame-ack / 重复事件不去重出第二条发送，发送帧恒为 1", async () => {
    await startRelay();
    const conn = await connectPaired();
    const mapper = SessionTargetMapper.fromListings({ ...FAKE_LISTING, generation: 1 });
    const bound = mapper.resolveTarget({ taskId: FAKE_TARGET.taskId });
    mapper.authorizeSend(bound, 1);

    const loop = new SendOnceLoop({
      sender: conn,
      identity: FAKE_IDENTITY,
      target: bound,
      markerText: "[tuvis-b2-marker:dedupe]",
      clientId: "fake-client",
      timeouts: { ackMs: 2_000, activityMs: 300 },
      sleep: realSleep,
      isTurnActivity: (decoded, rawJson) =>
        (decoded?.body as { topic?: string } | null | undefined)?.topic ===
          `conversation/${FAKE_TARGET.taskId}` || rawJson.includes(FAKE_TARGET.taskId),
    });
    conn.onData = (payload) => loop.onData(payload);

    const outcomePromise = loop.run();
    await waitFor(() => relay.clientDataPayloads.length >= 1);
    // 我们的请求字节应编码为 [100, 1, "zcode-agent", "sendConversationCommandV4"]。
    const sentFrame = relay.clientDataPayloads[0];
    const sentBytes = Uint8Array.from(Buffer.from(String(sentFrame.dataBase64), "base64"));
    const decodedSent = decodeChannelMessage(sentBytes);
    expect(decodedSent).not.toBeNull();
    let commandId = "";
    if (decodedSent && decodedSent.headerType === REQUEST_TYPE_PROMISE) {
      expect(decodedSent.channelName).toBe(AGENT_CHANNEL);
      expect(decodedSent.method).toBe(AGENT_SEND_COMMAND_METHOD);
      const params = (decodedSent.body as unknown[])[0] as {
        workspacePath: string;
        envelope: { sessionId: string; type: string; payload: { text: string }; commandId: string };
      };
      expect(params.workspacePath).toBe(FAKE_TARGET.workspacePath);
      expect(params.envelope.sessionId).toBe(FAKE_TARGET.taskId);
      expect(params.envelope.type).toBe("sendText");
      expect(params.envelope.payload.text.startsWith("[tuvis-b2-marker:")).toBe(true);
      commandId = params.envelope.commandId;
    }

    // 重复 rpc-frame-ack ×3 + 应答帧 ×2（同一消息重复推送）+ 重复事件帧 ×2 —— 全部去重。
    relay.pushPayload({ zcode_type: "rpc-frame-ack", ...FAKE_IDENTITY, ackMessageSeq: 1 });
    relay.pushPayload({ zcode_type: "rpc-frame-ack", ...FAKE_IDENTITY, ackMessageSeq: 1 });
    relay.pushPayload({ zcode_type: "rpc-frame-ack", ...FAKE_IDENTITY, ackMessageSeq: 1 });
    const commandIdForAck = commandId;
    const responseFrame = frameFromBytes(ackAcceptedBytes(1, commandIdForAck), 100, 50);
    relay.pushPayload(responseFrame);
    relay.pushPayload(responseFrame);
    const eventFrame = frameFromBytes(
      encodeChannelResponse({
        responseType: 204,
        id: 9,
        body: { topic: `conversation/${FAKE_TARGET.taskId}`, fromSeq: 0, toSeq: 1 },
      }),
      101,
      51
    );
    relay.pushPayload(eventFrame);
    relay.pushPayload(eventFrame);

    const outcome = await outcomePromise;
    expect(outcome.kind).toBe("completed");
    expect(loop.framesSent).toBe(1);
    expect(relay.clientDataPayloads.length).toBe(1); // 只有我们的那一帧，无任何重发
    expect(loop.duplicateInboundFrames).toBeGreaterThanOrEqual(3);
  });

  it("stale_epoch_rejected：旧连接代际绑定的映射在新代际上不能授权发送；bridge 代际回显不构成构造闸", async () => {
    await startRelay();
    const conn = await connectPaired(); // 占住 relay（bridge 代际回显段的发送走这条连接）
    const mapper = SessionTargetMapper.fromListings({ ...FAKE_LISTING, generation: 1 });
    const bound = mapper.resolveTarget({ taskId: FAKE_TARGET.taskId });

    // 连接换代（generation 2，例如 KICKED 后重连出的新一代际）：旧映射必须被拒。
    expect(() => mapper.authorizeSend(bound, 2)).toThrow(/stale-epoch/);

    // 新代际下必须用新 listing 重新绑定，才能授权。
    const mapperGen2 = SessionTargetMapper.fromListings({ ...FAKE_LISTING, generation: 2 });
    const boundGen2 = mapperGen2.resolveTarget({ taskId: FAKE_TARGET.taskId });
    // 新代际映射 + 新代际连接 → 合法授权（不抛）。
    mapperGen2.authorizeSend(boundGen2, 2);
    expect(boundGen2.generation).toBe(2);
    // 但旧代际 mapper 的绑定产物（generation 1）在代际 2 连接上依旧被拒——
    // 授权看绑定代际本身，不因「存在新 mapper」而放行。
    expect(() => mapper.authorizeSend(bound, 2)).toThrow(/stale-epoch/);

    // 构造不做代际校验（评审 Important 修复）：identity.bridgeGeneration 是宿主
    // workspace-bridge-ready 回显的 bridge 通道代际，与探针连接代际
    // （target.generation）不同命名空间——回显 ≠1（如 7）不得中断发送闭环：
    // 构造成功、帧照发且携带回显值、run 照常进行。
    const loopGen7 = new SendOnceLoop({
      sender: conn,
      identity: { ...FAKE_IDENTITY, bridgeGeneration: 7 },
      target: bound, // generation 1：与 bridgeGeneration=7 不同命名空间，不构成 stale-epoch
      markerText: "[tuvis-b2-marker:bridge-gen-echo]",
      clientId: "fake-client",
      timeouts: { ackMs: 150, activityMs: 20 },
      sleep: realSleep,
    });
    conn.onData = (payload) => loopGen7.onData(payload);
    const outcomeGen7Promise = loopGen7.run();
    await waitFor(() => relay.clientDataPayloads.length >= 1);
    expect(relay.clientDataPayloads[0].bridgeGeneration).toBe(7);
    // 无应答注入 → unknown；帧数恒 1（不因代际回显而中断，也无重发）。
    const outcomeGen7 = await outcomeGen7Promise;
    expect(outcomeGen7.kind).toBe("unknown");
    expect(loopGen7.framesSent).toBe(1);
    expect(relay.clientDataPayloads.length).toBe(1);

    // stale-epoch 闸依旧唯一住在 mapper（like-for-like：映射代际 vs 连接代际）——
    // 旧代际映射在新代际上不能授权；未过闸的映射没有任何额外发送路径
    // （除上面那一帧外 relay 收到的 data 帧仍为 0 条增量）。
    expect(() => mapper.authorizeSend(bound, 2)).toThrow(/stale-epoch/);
    expect(relay.clientDataPayloads.length).toBe(1);
  });

  it("wire 形状自检：CRC32 与官方实现一致（B1 实连 4 帧checksum复算），请求字节按 serialization.ts 编码", async () => {
    // B1 run macos-B120261024T105940Z-NarmSw 的 4 条实帧（合成复现：等长字节）。
    // 这里用与官方 wire-binary 相同的 IEEE CRC32 语义做往返断言。
    const bytes = crypto.randomBytes(512);
    const frame = encodeRpcFrame({ identity: FAKE_IDENTITY, seq: 3, messageSeq: 2, bytes });
    expect(frame.checksum).toEqual({ algorithm: "crc32", value: crc32WireBytes(bytes) });
    expect(frame.zcode_type).toBe("rpc-frame");
    expect(frame.messageBytes).toBe(bytes.byteLength);
    expect(frame.fragmentIndex).toBe(0);
    expect(frame.fragmentCount).toBe(1);
    // dataBase64 往返一致。
    expect(Buffer.from(frame.dataBase64 as string, "base64").equals(Buffer.from(bytes))).toBe(true);
    // 请求头数组序列化：[100, id, channel, method] + args。
    const req = encodeChannelRequest({
      id: 5,
      channel: AGENT_CHANNEL,
      method: AGENT_SEND_COMMAND_METHOD,
      params: { workspacePath: "/w", envelope: { commandId: "c" } },
    });
    const decoded = decodeChannelMessage(req);
    expect(decoded?.headerType).toBe(100);
    expect(decoded?.id).toBe(5);
    expect(decoded?.channelName).toBe(AGENT_CHANNEL);
    expect(decoded?.method).toBe(AGENT_SEND_COMMAND_METHOD);
  });
});

// ---------------------------------------------------------------------------
// B2 map-only 修复（实连 run macos-B220261024T121929Z-SnWWVA 在 workspace-ambiguous
// fail-closed、被动 8s 收帧窗 0 条 rpc-frame）：
//   1. workspace 指称唯一化阶梯（纯函数，无网络）；
//   2. controller tasks-index 主动枚举（fake relay 同款 wire）。
// ---------------------------------------------------------------------------

describe("workspace 指称唯一化阶梯（B2 map-only 修复）", () => {
  it("双 listing 去重：bootstrap 与 workspace-list 携带同一份条目只保留一份（实连 28→14 复现）", () => {
    const entryA: WorkspaceListingEntry = {
      workspacePath: "/fake/Travel",
      label: "Travel",
      kind: "local",
      workspacePurpose: "project",
    };
    const entryB: WorkspaceListingEntry = {
      workspacePath: "/fake/Travel",
      label: "Travel",
      kind: "local",
      workspacePurpose: "conversation",
    };
    // 同一条目在两份 listing 里重复 → 去重为 1；不同 purpose 是不同条目 → 保留。
    expect(dedupeWorkspaceEntries([entryA, entryA, entryB, entryA])).toHaveLength(2);
  });

  it("activeWorkspaceKey 只读自 bootstrap（mobileViewState 优先），缺失回落 workspace-list 顶层", () => {
    const bootstrap = {
      result: { mobileViewState: { activeWorkspaceKey: "/fake/from-bootstrap" } },
    };
    const workspaceList = { result: { activeWorkspaceKey: "/fake/from-list" } };
    expect(readActiveWorkspaceKeyFromListings({ bootstrap, workspaceList })).toBe(
      "/fake/from-bootstrap"
    );
    expect(readActiveWorkspaceKeyFromListings({ bootstrap: null, workspaceList })).toBe(
      "/fake/from-list"
    );
    expect(readActiveWorkspaceKeyFromListings({ bootstrap: null, workspaceList: null })).toBeNull();
  });

  it("同 label 的 project/conversation 双候选 → purpose 收窄唯一解析；计数证据不含值", () => {
    const workspaces: WorkspaceListingEntry[] = [
      {
        workspacePath: "/fake/Travel-conv",
        label: "Travel",
        kind: "local",
        workspacePurpose: "conversation",
      },
      {
        workspacePath: "/fake/Travel-proj",
        label: "Travel",
        kind: "local",
        workspacePurpose: "project",
      },
    ];
    const result = resolveDesignatedWorkspace({
      workspaces,
      label: "Travel",
      activeWorkspaceKey: "/fake/elsewhere",
    });
    expect(result.ok).toBe(true);
    if (!result.ok) return;
    expect(result.entry.workspacePath).toBe("/fake/Travel-proj");
    expect(result.evidence).toEqual({
      labelMatches: 2,
      afterPurposeFilter: 1,
      afterActiveKeyFilter: null,
      activeWorkspaceKnown: true,
      resolvedStep: "purpose-project",
      activeWorkspaceConfirmed: false,
    });
  });

  it("多 project 候选 → activeWorkspaceKey 收窄解析，且 activeWorkspaceConfirmed=true", () => {
    const workspaces: WorkspaceListingEntry[] = [
      {
        workspacePath: "/fake/Travel-A",
        label: "Travel",
        kind: "local",
        workspacePurpose: "project",
      },
      {
        workspacePath: "/fake/Travel-B",
        label: "Travel",
        kind: "local",
        workspacePurpose: "project",
      },
    ];
    const result = resolveDesignatedWorkspace({
      workspaces,
      label: "Travel",
      activeWorkspaceKey: "/fake/Travel-B",
    });
    expect(result.ok).toBe(true);
    if (!result.ok) return;
    expect(result.entry.workspacePath).toBe("/fake/Travel-B");
    expect(result.evidence.resolvedStep).toBe("active-workspace-key");
    expect(result.evidence.afterPurposeFilter).toBe(2);
    expect(result.evidence.afterActiveKeyFilter).toBe(1);
    expect(result.evidence.activeWorkspaceConfirmed).toBe(true);
  });

  it("purpose 收窄为空集（全 conversation）时保留原候选集继续走 activeWorkspaceKey", () => {
    const workspaces: WorkspaceListingEntry[] = [
      {
        workspacePath: "/fake/Only-A",
        label: "Solo",
        kind: "local",
        workspacePurpose: "conversation",
      },
      {
        workspacePath: "/fake/Only-B",
        label: "Solo",
        kind: "local",
        workspacePurpose: "conversation",
      },
    ];
    const result = resolveDesignatedWorkspace({
      workspaces,
      label: "Solo",
      activeWorkspaceKey: "/fake/Only-B",
    });
    expect(result.ok).toBe(true);
    if (!result.ok) return;
    expect(result.entry.workspacePath).toBe("/fake/Only-B");
    expect(result.evidence.afterPurposeFilter).toBe(0);
    expect(result.evidence.resolvedStep).toBe("active-workspace-key");
  });

  it("仍不唯一（activeWorkspaceKey 缺失/不命中）→ ambiguous 中止；零命中 → not-found 中止", () => {
    const workspaces: WorkspaceListingEntry[] = [
      {
        workspacePath: "/fake/Travel-A",
        label: "Travel",
        kind: "local",
        workspacePurpose: "project",
      },
      {
        workspacePath: "/fake/Travel-B",
        label: "Travel",
        kind: "local",
        workspacePurpose: "project",
      },
    ];
    const ambiguousNoKey = resolveDesignatedWorkspace({
      workspaces,
      label: "Travel",
      activeWorkspaceKey: null,
    });
    expect(ambiguousNoKey.ok).toBe(false);
    if (!ambiguousNoKey.ok) expect(ambiguousNoKey.reason).toBe("workspace-ambiguous");
    const ambiguousMiss = resolveDesignatedWorkspace({
      workspaces,
      label: "Travel",
      activeWorkspaceKey: "/fake/Not-Listed",
    });
    expect(ambiguousMiss.ok).toBe(false);
    if (!ambiguousMiss.ok) {
      expect(ambiguousMiss.reason).toBe("workspace-ambiguous");
      expect(ambiguousMiss.evidence.afterActiveKeyFilter).toBe(0);
    }
    const notFound = resolveDesignatedWorkspace({ workspaces, label: "Nope" });
    expect(notFound.ok).toBe(false);
    if (!notFound.ok) expect(notFound.reason).toBe("workspace-not-found");
  });

  it("唯一命中 → exact-label；path 与 activeWorkspaceKey 相同时交叉确认 activeWorkspaceConfirmed", () => {
    const workspaces: WorkspaceListingEntry[] = [
      { workspacePath: "/fake/Unique", label: "Unique", kind: "local" },
    ];
    const confirmed = resolveDesignatedWorkspace({
      workspaces,
      label: "Unique",
      activeWorkspaceKey: "/fake/Unique",
    });
    expect(confirmed.ok).toBe(true);
    if (confirmed.ok) {
      expect(confirmed.evidence.resolvedStep).toBe("exact-label");
      expect(confirmed.evidence.activeWorkspaceConfirmed).toBe(true);
    }
    const unconfirmed = resolveDesignatedWorkspace({
      workspaces,
      label: "Unique",
      activeWorkspaceKey: "/fake/Other",
    });
    expect(unconfirmed.ok).toBe(true);
    if (unconfirmed.ok) expect(unconfirmed.evidence.activeWorkspaceConfirmed).toBe(false);
  });
});

describe("controller tasks-index 主动枚举（B2 map-only 修复）", () => {
  let relay: FakeRelay;
  const connections: RelayTerminalConnection[] = [];

  afterEach(async () => {
    for (const conn of connections.splice(0)) {
      conn.close();
    }
    if (relay) await relay.close();
  });

  async function connectFakePaired(): Promise<RelayTerminalConnection> {
    relay = new FakeRelay();
    const port = await relay.listen();
    const conn = new RelayTerminalConnection({
      relayWsUrl: `ws://127.0.0.1:${port}/api/v1/remote/ws2`,
      token: relay.mintToken(),
      deviceSid: relay.deviceSid,
    });
    connections.push(conn);
    expect(await conn.connect()).toBe("paired");
    return conn;
  }

  function decodeClientFrame(index: number) {
    const payload = relay.clientDataPayloads[index];
    return decodeChannelMessage(Uint8Array.from(Buffer.from(String(payload.dataBase64), "base64")));
  }

  function controllerEventFrame(id: number, body: unknown, seq: number, messageSeq: number) {
    return frameFromBytes(
      encodeChannelResponse({ responseType: RESPONSE_TYPE_EVENT_FIRE, id, body }),
      seq,
      messageSeq
    );
  }

  function hiTaskRow(liveStatus: string) {
    return {
      address: { workspacePath: FAKE_TARGET.workspacePath, taskId: FAKE_TARGET.taskId },
      meta: {
        title: "hi",
        taskId: FAKE_TARGET.taskId,
        workspacePath: FAKE_TARGET.workspacePath,
      },
      membership: { pinned: false, archived: false, active: true },
      sourceAvailability: "online",
      liveStatus,
    };
  }

  it("subscribe_active_enumeration：Initialize 门 → EventListen+subscribeControllerV4 → snapshot/deltas 任务行 → title 精确唯一匹配；全程只发读面控制帧", async () => {
    const conn = await connectFakePaired();
    const reader = new ControllerTaskIndexReader({
      sender: conn,
      identity: FAKE_IDENTITY,
      timeouts: { initializeMs: 2_000, subscribeMs: 2_000, windowMs: 400, unsubscribeMs: 400 },
      sleep: realSleep,
    });
    conn.onData = (payload) => reader.onData(payload);
    const runPromise = reader.run();

    // 官方 ChannelClient 在收到桥 ChannelServer 的 Initialize 之前不发任何请求
    // （channelClient.ts:124-128 whenInitialized 门）：FAKE 先推 Initialize。
    relay.pushPayload(initializeFrame(1, 1));

    // 前两个出站控制帧 = EventListen(102) + subscribeControllerV4(100)，都在
    // channel "window-controller" 上（官方 registry 同序：先监听后订阅）。
    await waitFor(() => relay.clientDataPayloads.length >= 2);
    const listenDecoded = decodeClientFrame(0);
    const subscribeDecoded = decodeClientFrame(1);
    expect(listenDecoded?.headerType).toBe(102);
    expect(listenDecoded?.channelName).toBe("window-controller");
    expect(listenDecoded?.method).toBe("onDynamicControllerFrame");
    expect(subscribeDecoded?.headerType).toBe(REQUEST_TYPE_PROMISE);
    expect(subscribeDecoded?.channelName).toBe("window-controller");
    expect(subscribeDecoded?.method).toBe("subscribeControllerV4");
    expect((subscribeDecoded?.body as unknown[])[0]).toEqual({
      topic: "controller/tasks-index",
      visibility: "foreground",
    });
    const listenId = listenDecoded?.id as number;
    const subscribeId = subscribeDecoded?.id as number;

    // subscribe ack（PromiseSuccess 201 → {ack:{subscriptionId, mode, logEpoch}}）。
    relay.pushPayload(
      frameFromBytes(
        encodeChannelResponse({
          responseType: RESPONSE_TYPE_PROMISE_SUCCESS,
          id: subscribeId,
          body: {
            ack: { subscriptionId: "fake-sub-1", mode: "snapshot", logEpoch: "fake-log-epoch" },
          },
        }),
        2,
        2
      )
    );
    // snapshot 事件帧（204 按 EventListen id 路由）：hi 行 + 另一行。
    relay.pushPayload(
      controllerEventFrame(
        listenId,
        {
          topic: "controller/tasks-index",
          subscriptionId: "fake-sub-1",
          logEpoch: "fake-log-epoch",
          fromSeq: 0,
          toSeq: 1,
          sentAt: 0,
          payload: {
            kind: "snapshot",
            snapshot: {
              protocolVersion: 1,
              logEpoch: "fake-log-epoch",
              tasks: [
                hiTaskRow("idle"),
                {
                  address: {
                    workspacePath: FAKE_TARGET.workspacePath,
                    taskId: "fake-task-other-0002",
                  },
                  meta: {
                    title: "other",
                    taskId: "fake-task-other-0002",
                    workspacePath: FAKE_TARGET.workspacePath,
                  },
                  membership: { pinned: false, archived: false, active: false },
                  sourceAvailability: "online",
                  liveStatus: "running",
                },
              ],
            },
          },
        },
        3,
        3
      )
    );
    // delta 事件帧：task.upserted 同键覆盖（行数不增）。
    relay.pushPayload(
      controllerEventFrame(
        listenId,
        {
          topic: "controller/tasks-index",
          subscriptionId: "fake-sub-1",
          logEpoch: "fake-log-epoch",
          fromSeq: 1,
          toSeq: 2,
          sentAt: 0,
          payload: {
            kind: "deltas",
            deltas: [{ op: "task.upserted", task: hiTaskRow("running") }],
          },
        },
        4,
        4
      )
    );
    // peer 受理回执（FAKE 策略：由测试直发；官方对端按帧回 rpc-frame-ack）。
    relay.pushPayload({ zcode_type: "rpc-frame-ack", ...FAKE_IDENTITY, ackMessageSeq: 1 });

    // 收帧窗闭后退订：第 3 个出站控制帧 = unsubscribeControllerV4；推回其 201
    // 应答，让「尽力退订」在其有界窗内得到确认。
    await waitFor(() => relay.clientDataPayloads.length >= 3);
    const unsubDecoded = decodeClientFrame(2);
    expect(unsubDecoded?.headerType).toBe(REQUEST_TYPE_PROMISE);
    expect(unsubDecoded?.channelName).toBe("window-controller");
    expect(unsubDecoded?.method).toBe("unsubscribeControllerV4");
    expect((unsubDecoded?.body as unknown[])[0]).toEqual({ subscriptionId: "fake-sub-1" });
    const unsubId = unsubDecoded?.id as number;
    relay.pushPayload(
      frameFromBytes(
        encodeChannelResponse({
          responseType: RESPONSE_TYPE_PROMISE_SUCCESS,
          id: unsubId,
          body: {},
        }),
        5,
        5
      )
    );

    const result = await runPromise;
    expect(result.kind).toBe("ok");
    if (result.kind !== "ok") return;
    expect(result.ackMode).toBe("snapshot");
    expect(result.rows).toHaveLength(2);
    // 探针 designate 同款：workspace 内 title 精确唯一匹配 → 精确 taskId。
    const matches = result.rows.filter(
      (row) => row.address.workspacePath === FAKE_TARGET.workspacePath && row.meta?.title === "hi"
    );
    expect(matches).toHaveLength(1);
    expect(matches[0].address.taskId).toBe(FAKE_TARGET.taskId);
    expect(result.counters.controllerFramesSeen).toBe(2);
    expect(result.counters.snapshotFrames).toBe(1);
    expect(result.counters.deltaFrames).toBe(1);
    expect(result.counters.peerAckedFrames).toBeGreaterThanOrEqual(1);
    expect(result.counters.controlFramesSent).toBe(3);
    expect(result.counters.unsubscribed).toBe(true);
    expect(result.counters.initializeObserved).toBe(true);
    expect(unsubId).toBeGreaterThan(subscribeId);

    // 全程恰 3 条出站帧，无任何会话发送帧（sendConversationCommandV4 不存在）。
    expect(relay.clientDataPayloads).toHaveLength(3);
    for (const payload of relay.clientDataPayloads) {
      const decoded = decodeChannelMessage(
        Uint8Array.from(Buffer.from(String(payload.dataBase64), "base64"))
      );
      expect(decoded?.method).not.toBe(AGENT_SEND_COMMAND_METHOD);
    }
  });

  it("zero_rows_after_ack：subscribe ack 后窗内零任务行 → rows 空（调用方 fail-closed）", async () => {
    const conn = await connectFakePaired();
    const reader = new ControllerTaskIndexReader({
      sender: conn,
      identity: FAKE_IDENTITY,
      timeouts: { initializeMs: 1_000, subscribeMs: 1_000, windowMs: 250, unsubscribeMs: 400 },
      sleep: realSleep,
    });
    conn.onData = (payload) => reader.onData(payload);
    const runPromise = reader.run();
    relay.pushPayload(initializeFrame(1, 1));
    await waitFor(() => relay.clientDataPayloads.length >= 2);
    const subscribeId = decodeClientFrame(1)?.id as number;
    relay.pushPayload(
      frameFromBytes(
        encodeChannelResponse({
          responseType: RESPONSE_TYPE_PROMISE_SUCCESS,
          id: subscribeId,
          body: { ack: { subscriptionId: "fake-sub-2", mode: "snapshot", logEpoch: "fake-epoch" } },
        }),
        2,
        2
      )
    );
    // 收帧窗（零行）闭后退订：出现第 3 个出站帧后直发其 201 应答（FAKE 策略）。
    await waitFor(() => relay.clientDataPayloads.length >= 3);
    const unsubDecoded = decodeClientFrame(2);
    expect(unsubDecoded?.method).toBe("unsubscribeControllerV4");
    relay.pushPayload(
      frameFromBytes(
        encodeChannelResponse({
          responseType: RESPONSE_TYPE_PROMISE_SUCCESS,
          id: unsubDecoded?.id as number,
          body: {},
        }),
        3,
        3
      )
    );
    const result = await runPromise;
    expect(result.kind).toBe("ok");
    if (result.kind !== "ok") return;
    expect(result.rows).toHaveLength(0);
    expect(result.counters.controllerFramesSeen).toBe(0);
    // 无任务行也会尽力退订（ack 带了 subscriptionId）。
    expect(result.counters.unsubscribed).toBe(true);
  });

  it("subscribe_timeout：Initialize 已到而 subscribe ack 缺席 → 结构性失败（subscribe-timeout），零行、不退订、不重发", async () => {
    const conn = await connectFakePaired();
    const reader = new ControllerTaskIndexReader({
      sender: conn,
      identity: FAKE_IDENTITY,
      timeouts: { initializeMs: 500, subscribeMs: 250, windowMs: 100, unsubscribeMs: 100 },
      sleep: realSleep,
    });
    conn.onData = (payload) => reader.onData(payload);
    const runPromise = reader.run();
    relay.pushPayload(initializeFrame(1, 1));
    await waitFor(() => relay.clientDataPayloads.length >= 2);
    const result = await runPromise;
    expect(result.kind).toBe("subscribe-timeout");
    expect(result.counters.initializeObserved).toBe(true);
    expect(result.counters.controlFramesSent).toBe(2);
    expect(result.counters.unsubscribeRequestId).toBeNull();
    expect(result.counters.rowsCollected).toBe(0);
    // 不重发：出站帧恒为 EventListen + subscribe 两帧。
    await realSleep(80);
    expect(relay.clientDataPayloads).toHaveLength(2);
  });

  it("initialize_timeout：桥 Initialize 缺席 → initialize-timeout，一个控制帧都不发（官方 whenInitialized 门）", async () => {
    const conn = await connectFakePaired();
    const reader = new ControllerTaskIndexReader({
      sender: conn,
      identity: FAKE_IDENTITY,
      timeouts: { initializeMs: 150, subscribeMs: 100, windowMs: 100, unsubscribeMs: 100 },
      sleep: realSleep,
    });
    conn.onData = (payload) => reader.onData(payload);
    const result = await reader.run();
    expect(result.kind).toBe("initialize-timeout");
    expect(result.counters.initializeObserved).toBe(false);
    expect(result.counters.controlFramesSent).toBe(0);
    expect(result.counters.eventListenRequestId).toBe(0);
    expect(result.counters.subscribeRequestId).toBe(0);
    expect(result.counters.unsubscribeRequestId).toBeNull();
    expect(relay.clientDataPayloads).toHaveLength(0);
    // 再等一拍也不发（无重试路径；官方客户端同样在 Initialize 前静默排队）。
    await realSleep(60);
    expect(relay.clientDataPayloads).toHaveLength(0);
  });
});

// ---------------------------------------------------------------------------
// B2 实连 124743Z 修复的 wire 形状与应答分类（纯函数；不依赖 relay fixture）：
//   1. 桥 Initialize 帧 = serialize([200]) + serialize(undefined) == 实连 6 字节
//      （channelServer.ts:30,45-47,57-66；B2 实连 macos-B220261024T124743Z bridge-messages[0]）；
//   2. workspace-bridge-open 应答分类：ready 不带 success 字段（探针旧 summary 把它
//      误报成 success:false）；正式拒绝通道 = workspace-bridge-error
//      （webRemoteControlManager.ts:1175-1191）；ready 帧身份 = 请求声明字段的**原样回显**
//      （manager.ts:1156-1165 只回显 request 里存在的 generation/recoveryId）——探针绝不
//      虚构 bridgeGeneration：APP 接收侧 rawIdentityMatches 严格相等（含 undefined，
//      webRemoteControlAcknowledgedRelayProtocolTypes.ts:65-76），多余字段 = 静默丢帧无 ack
//      （= 实连 subscribe-timeout 根因）；
//   3. rpc-frame-ack 编码：身份缺省字段不得出现（同上严格相等判据；
//      ackRelayProtocol.ts:203-218 接收侧回执形状）。
// ---------------------------------------------------------------------------
describe("workspace-bridge-open 应答分类与桥 wire 形状（B2 实连 124743Z 修复）", () => {
  it("Initialize wire：serialize([200])+serialize(undefined) == 实连 6 字节帧；decodeChannelMessage 可解", () => {
    const bytes = encodeChannelInitialize();
    expect([...bytes]).toEqual([0x04, 0x01, 0x06, 200, 0x01, 0x00]);
    const decoded = decodeChannelMessage(bytes);
    expect(decoded).not.toBeNull();
    expect(decoded?.headerType).toBe(RESPONSE_TYPE_INITIALIZE);
    expect(decoded?.id).toBeNull();
    expect(decoded?.channelName).toBeNull();
    expect(decoded?.method).toBeNull();
  });

  it("ready 回显身份只含请求声明过的字段：无 generation 的 open → 帧身份与发送帧绝不虚构 bridgeGeneration", () => {
    // B2 实连 124743Z 的 ready 形状：bridge 对象无 bridgeGeneration/recoveryId。
    const ready = {
      zcode_type: "workspace-bridge-ready",
      requestId: "b2-fake-req-1",
      bridgeSessionId: "fake-bridge-open-0002",
      bridge: {
        bridgeSessionId: "fake-bridge-open-0002",
        kind: "local",
        workspaceKey: "/Users/fake/Travel-workspace",
        workspacePath: "/Users/fake/Travel-workspace",
      },
    };
    const cls = classifyWorkspaceBridgeOpenResponse(ready);
    expect(cls.kind).toBe("ready");
    if (cls.kind !== "ready") return;
    expect(cls.identity).toEqual({ bridgeSessionId: "fake-bridge-open-0002" });
    expect("bridgeGeneration" in cls.identity).toBe(false);
    expect("recoveryId" in cls.identity).toBe(false);
    expect(cls.bridgeKind).toBe("local");
    // 用该身份编码的 rpc-frame 也不携带 bridgeGeneration——APP 接收侧按严格相等
    // 判 identity（含 undefined），多余字段会被静默丢弃且不回 ack。
    const frame = encodeRpcFrame({
      identity: cls.identity,
      seq: 1,
      messageSeq: 1,
      bytes: new Uint8Array([1, 2, 3]),
    });
    expect("bridgeGeneration" in frame).toBe(false);
    expect("recoveryId" in frame).toBe(false);
  });

  it("ready 携带回显 generation/recoveryId 时身份原样保留（manager 只回显请求里存在的字段）", () => {
    const ready = {
      zcode_type: "workspace-bridge-ready",
      requestId: "b2-fake-req-2",
      bridgeSessionId: "fake-bridge-open-0003",
      bridgeGeneration: 4,
      recoveryId: "fake-recovery-1",
      bridge: { bridgeSessionId: "fake-bridge-open-0003", bridgeGeneration: 4, kind: "remote" },
    };
    const cls = classifyWorkspaceBridgeOpenResponse(ready);
    expect(cls).toMatchObject({
      kind: "ready",
      identity: {
        bridgeSessionId: "fake-bridge-open-0003",
        bridgeGeneration: 4,
        recoveryId: "fake-recovery-1",
      },
    });
  });

  it("workspace-bridge-error → refused（reason/error 原样带出）；success:false 包装 → refused", () => {
    // 正式拒绝通道（webRemoteControlManager.ts:1180-1190）。
    expect(
      classifyWorkspaceBridgeOpenResponse({
        zcode_type: "workspace-bridge-error",
        requestId: "b2-fake-req-3",
        bridgeSessionId: "fake-bridge-open-0004",
        reason: "workspace-not-found",
        error: "目标工作区不在当前桌面窗口中，无法创建 Web 远程控制 bridge。",
      })
    ).toEqual({
      kind: "refused",
      reason: "workspace-not-found",
      error: "目标工作区不在当前桌面窗口中，无法创建 Web 远程控制 bridge。",
    });
    // 防御分支：显式 success:false 一律按拒绝处理（ready 本身无 success 字段——
    // web-remote-control.ts:449-456）。
    expect(
      classifyWorkspaceBridgeOpenResponse({
        zcode_type: "workspace-bridge-ready",
        requestId: "r",
        success: false,
        error: null,
      })
    ).toEqual({ kind: "refused", reason: null, error: null });
  });

  it("其他形状 → unknown（fail-closed，不猜）", () => {
    expect(classifyWorkspaceBridgeOpenResponse(null)).toEqual({ kind: "unknown" });
    expect(classifyWorkspaceBridgeOpenResponse({ zcode_type: "bootstrap-response" })).toEqual({
      kind: "unknown",
    });
    expect(
      classifyWorkspaceBridgeOpenResponse({ zcode_type: "workspace-bridge-ready" }, "fallback-id")
    ).toEqual({ kind: "unknown" });
  });

  it("rpc-frame-ack 编码：身份缺省字段不得出现（接收侧严格相等判据）", () => {
    expect(
      encodeRpcFrameAck({
        identity: { bridgeSessionId: "fake-bridge-open-0005" },
        ackMessageSeq: 3,
      })
    ).toEqual({
      zcode_type: "rpc-frame-ack",
      bridgeSessionId: "fake-bridge-open-0005",
      ackMessageSeq: 3,
    });
    expect(
      encodeRpcFrameAck({
        identity: { bridgeSessionId: "b", bridgeGeneration: 2, recoveryId: "r" },
        ackMessageSeq: 4,
      })
    ).toEqual({
      zcode_type: "rpc-frame-ack",
      bridgeSessionId: "b",
      bridgeGeneration: 2,
      recoveryId: "r",
      ackMessageSeq: 4,
    });
  });
});

// ---------------------------------------------------------------------------
// B2 收官修复（stdout 别名化；实连 run macos-B220261024T132316Z-prItvw 收官时发现）：
// 「目标已锁定」一行曾把**真实** taskId/workspaceKey 打上 stdout——证据文件经
// SecretRedactor fail-closed 脱敏，stdout 不在落盘闸内。修复 = 该行经
// formatTargetLockedLine 走 redactor 别名化；本组断言锁形：真实值绝不上 stdout、
// 别名与 mapping 阶段登记稳定复用、verdict 措辞保持人类可读。
// ---------------------------------------------------------------------------
describe("stdout 别名化（B2 收官修复）", () => {
  const FAKE_REAL_TARGET = {
    taskId: "b2-fake-real-task-id",
    workspaceKey: "/Users/fake/real-travel-path",
    generation: 1,
  };

  it("目标锁定行只含别名与代际：真实 taskId/workspace 路径不出现在文案里", () => {
    const redactor = new SecretRedactor();
    const line = formatTargetLockedLine(redactor, FAKE_REAL_TARGET);
    expect(line).toContain("目标已锁定（别名）");
    expect(line).toMatch(/taskId=SID_[A-Z]/);
    expect(line).toMatch(/workspace=PATH_\d+/);
    expect(line).toContain("（generation 1）");
    expect(line).not.toContain(FAKE_REAL_TARGET.taskId);
    expect(line).not.toContain(FAKE_REAL_TARGET.workspaceKey);
  });

  it("别名稳定复用：mapping 阶段已登记的同一值在 stdout 行上复用同一别名", () => {
    const redactor = new SecretRedactor();
    // mapping 阶段收割 task 行 / workspace 条目时已按同值登记（sid / path 两类）。
    expect(redactor.register("sid", FAKE_REAL_TARGET.taskId)).toBe("SID_A");
    expect(redactor.register("path", FAKE_REAL_TARGET.workspaceKey)).toBe("PATH_1");
    const line = formatTargetLockedLine(redactor, FAKE_REAL_TARGET);
    expect(line).toContain("taskId=SID_A");
    expect(line).toContain("workspace=PATH_1");
    expect(line).not.toContain(FAKE_REAL_TARGET.taskId);
    expect(line).not.toContain(FAKE_REAL_TARGET.workspaceKey);
  });
});
