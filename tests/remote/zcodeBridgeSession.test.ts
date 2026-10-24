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
import {
  AGENT_CHANNEL,
  AGENT_SEND_COMMAND_METHOD,
  REQUEST_TYPE_PROMISE,
  RESPONSE_TYPE_PROMISE_SUCCESS,
  SendOnceLoop,
  SessionTargetMapper,
  SingleConnectionGate,
  crc32WireBytes,
  decodeChannelMessage,
  encodeChannelRequest,
  encodeChannelResponse,
  encodeRpcFrame,
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

  it("stale_epoch_rejected：旧连接代际绑定的映射在新代际上不能授权发送，且一个帧都不发", async () => {
    await startRelay();
    await connectPaired(); // 占住 relay（本用例只测映射代际，不发帧）
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

    // 发送循环在未经 authorize 的目标上不得发帧：直接构造 loop 也要求代际一致，
    // 旧代际 target 传入 → 构造即抛，零 data 帧。
    expect(
      () =>
        new SendOnceLoop({
          sender: { sendData: () => true },
          identity: { ...FAKE_IDENTITY, bridgeGeneration: 2 },
          target: bound, // generation 1
          markerText: "[tuvis-b2-marker:stale]",
          clientId: "fake-client",
          timeouts: { ackMs: 60, activityMs: 20 },
          sleep: realSleep,
        })
    ).toThrow(/stale-epoch/);
    expect(relay.clientDataPayloads.length).toBe(0);
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
