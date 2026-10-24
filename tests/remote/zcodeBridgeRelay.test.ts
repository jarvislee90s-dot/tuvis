// @vitest-environment node
// tests/remote/zcodeBridgeRelay.test.ts — B1 红测：fake relay 上的终端身份门槛
// （工作包 B 计划：docs/superpowers/plans/2026-10-24-zcode-official-bridge-validation.md §B1 第 1 条）。
//
// 本文件内含一个最小 FAKE relay（in-test，仅 127.0.0.1、仅合成凭据），它建模的是
// 固定官方源码 zai-org/ZCode @ 3c5b227649371507ddc4eb46ac5133afe6db1cab 中**客户端侧**
// 实际执行的认证握手形状；官方云 relay 服务端不在该仓库，服务端内部语义不在本测试断言范围：
//   - HTTP POST /api/v1/remote/token，Authorization: Bearer <账号JWT>，body {role:"terminal"}
//     → packages/shared/src/web-remote-relay/contract.ts:96-101（路径表）、
//       packages/web/src/web-remote-control/adapters/webRemoteControlV2Token.ts:13-22
//   - 响应信封 {code:0,msg,data:{token,expires_in}}，token 为 22 字符 base64URL（末字符∈[AQgw]）
//     → contract.ts:119-133（token schema 与信封 union）
//   - WS /api/v1/remote/ws2?token=<一次性token>（链接 mid trim 非空时追加 &mid=<device_mid>，
//     官方条件性 webRemoteControlTransport.ts:107-110；desktop 设备角色同款 ：117-119），open 后发
//     auth_init{device_sid,meta,client_ts} → auth_ack{device_sid,pair_status: waiting|matched}；
//     waiting 为合法配对前态（官方 30s 窗口内等 matched，超时为独立终态、非 rejected）
//     → contract.ts:19-24 与 53-59；packages/web/src/web-remote-control/adapters/
//       webRemoteControlTransport.ts:106-143,460-473
//   - 出站 data 帧仅在 paired 态发送（客户端门禁）
//     → webRemoteControlTransport.ts:216-222（未 paired 返回 unavailable）
//   - error 帧 code ∈ AUTH_FAILED|WRONG_PARAM|KICKED|INTERNAL|DEVICE_OFFLINE
//     → contract.ts:18,71-76；KICKED 60 秒窗口内第二次即终态停止
//     → packages/shared/src/web-remote-relay/domain/retry.ts:26-30
//
// 被测客户端 = scripts/zcode-bridge-probe/relayAuth.ts（Tuvis 侧参照实现）：
// 单连接、无自动重连、token 一次性、认证失败/被踢/配对窗口超时即停且不发后续帧；
// pair_status=waiting 为非终态前态（保活等 matched，不并案 rejected）。
// 全部凭据为合成值（b1-fake-* / 随机 token），不含任何真实 SID/JWT/token/链接。
import * as crypto from "node:crypto";
import * as http from "node:http";
import type { Socket } from "node:net";
import { afterAll, afterEach, beforeAll, beforeEach, describe, expect, it } from "vitest";

// 本文件跑真实 127.0.0.1 socket：tests/setup.ts 已对全部 HTTP/WS 挂上全局 MSW 拦截
// （未登记请求 warn 放行）。这里在文件级 beforeAll 把它关掉——vitest 默认按文件隔离
// worker，只影响本文件，不影响其他用例的 mock 语义。
import { server as mswServer } from "../msw/server";

beforeAll(() => {
  mswServer.close();
});
afterAll(() => {
  // 交还拦截器（本 worker 内若被复用仍恢复干净状态；正常情况下进程随文件结束）。
  mswServer.listen({ onUnhandledRequest: "bypass" });
});

import {
  OFFICIAL_TOKEN_SHAPE,
  RelayTerminalConnection,
  requestTerminalToken,
  type RelayConnectionResult,
} from "../../scripts/zcode-bridge-probe/relayAuth";

// ---------------------------------------------------------------------------
// FAKE relay（仅测试）：行为标签——凡本文件注释标「FAKE 策略」的处理均为本测试自定，
// 不是官方服务端取证结论（服务端源码不在仓库，见决策文件 B1 节）。
// ---------------------------------------------------------------------------

const WS_GUID = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";
// 与官方 token schema 同形（contract.ts:121）：21 个 base64URL 字符 + 末字符∈[AQgw]。
const TOKEN_LAST_CHARS = "AQgw";

type StoredToken = { expiresAt: number; consumed: boolean };

type ClientSession = {
  socket: Socket;
  authenticated: boolean;
  alive: boolean;
};

class FakeRelay {
  readonly accountJwt = "b1-fake-account-jwt-synthetic";
  readonly deviceSid = "b1-fake-device-sid-0001";
  readonly stats = {
    wsUpgrades: 0,
    tokenMinted: 0,
    tokenHttp401: 0,
    wsAuthFailed: 0,
    dataAccepted: 0,
    preAuthDataRejected: 0,
    kickedSent: 0,
  };
  /** 每次 ws 升级 URL 的查询参数快照（升级序）；mid 条件性断言用。 */
  readonly wsUpgradeQueries: URLSearchParams[] = [];
  private readonly server: http.Server;
  private readonly tokens = new Map<string, StoredToken>();
  private readonly sessions = new Set<ClientSession>();
  /** auth_ack 的 pair_status 剧本：默认 ["matched"]；waiting 用例注入 ["waiting"] 等。 */
  private pairScript: Array<"waiting" | "matched"> = ["matched"];

  constructor() {
    this.server = http.createServer((req, res) => this.handleHttp(req, res));
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

  /** 测试直发：铸造一枚一次性 token（绕过 HTTP，用于过期/重复消费负例）。 */
  mintToken(ttlMs = 30_000): string {
    const body = crypto.randomBytes(16).toString("base64url"); // 22 字符
    const token = body.slice(0, 21) + TOKEN_LAST_CHARS[crypto.randomInt(TOKEN_LAST_CHARS.length)];
    this.tokens.set(token, { expiresAt: Date.now() + ttlMs, consumed: false });
    this.stats.tokenMinted += 1;
    return token;
  }

  /** 测试直发：对已认证连接注入 KICKED（单 owner 冲突的 fake 模拟）。 */
  kick(): void {
    for (const session of this.sessions) {
      if (session.authenticated && session.alive) {
        this.stats.kickedSent += 1;
        this.sendJson(session, { type: "error", code: "KICKED", message: "fake-relay: kicked" });
        this.closeSession(session);
      }
    }
  }

  /** 测试直发：改写 auth_ack 的 pair_status 剧本（默认 ["matched"]）。 */
  setPairScript(script: Array<"waiting" | "matched">): void {
    this.pairScript = [...script];
  }

  /**
   * 测试直发：对已认证会话补发 auth_ack（官方 pair_status_query 心跳的回应形态，
   * contract.ts:53-59；被测客户端不发心跳，waiting 语义由该注入驱动）。
   */
  sendPairAck(pairStatus: "waiting" | "matched"): void {
    for (const session of this.sessions) {
      if (session.authenticated && session.alive) {
        this.sendJson(session, {
          type: "auth_ack",
          device_sid: this.deviceSid,
          terminal_sid: "b1-fake-terminal-sid-0001",
          pair_status: pairStatus,
          server_ts: Math.floor(Date.now() / 1000),
        });
      }
    }
  }

  private handleHttp(req: http.IncomingMessage, res: http.ServerResponse): void {
    if (req.method === "POST" && (req.url ?? "").split("?")[0] === "/api/v1/remote/token") {
      const header = req.headers.authorization ?? "";
      const bearer = header.startsWith("Bearer ") ? header.slice("Bearer ".length) : "";
      let body = "";
      req.on("data", (chunk: Buffer) => {
        body += chunk.toString("utf8");
      });
      req.on("end", () => {
        if (bearer !== this.accountJwt) {
          this.stats.tokenHttp401 += 1;
          res.writeHead(401).end();
          return;
        }
        let role: unknown;
        try {
          role = (JSON.parse(body) as { role?: unknown }).role;
        } catch {
          role = undefined;
        }
        if (role !== "terminal") {
          // FAKE 策略：本 fake 只服务 terminal 角色；device 角色路径不在本测试断言范围。
          res.writeHead(200, { "Content-Type": "application/json" });
          res.end(JSON.stringify({ code: 1, msg: "fake-relay: role not served", data: null }));
          return;
        }
        const token = this.mintToken();
        res.writeHead(200, { "Content-Type": "application/json" });
        res.end(JSON.stringify({ code: 0, msg: "ok", data: { token, expires_in: 30 } }));
      });
      return;
    }
    res.writeHead(404).end();
  }

  private handleUpgrade(req: http.IncomingMessage, socket: Socket): void {
    const url = new URL(req.url ?? "/", "http://fake-relay.invalid");
    this.stats.wsUpgrades += 1;
    this.wsUpgradeQueries.push(url.searchParams);
    if (url.pathname !== "/api/v1/remote/ws2") {
      socket.destroy();
      return;
    }
    // FAKE 策略：升级一律完成握手，token 校验放到 auth_init 之后以 error 帧回——
    // 官方客户端的 AUTH_FAILED/KICKED 处理都在消息层（handleRelayError），fake 沿用该层。
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
    let msg: { type?: unknown; device_sid?: unknown };
    try {
      msg = JSON.parse(text);
    } catch {
      return;
    }
    if (msg.type === "auth_init") {
      const entry = token ? this.tokens.get(token) : undefined;
      const valid =
        !!entry &&
        !entry.consumed &&
        entry.expiresAt > Date.now() &&
        OFFICIAL_TOKEN_SHAPE.test(token ?? "") &&
        msg.device_sid === this.deviceSid;
      if (!valid) {
        this.stats.wsAuthFailed += 1;
        this.sendJson(session, {
          type: "error",
          code: "AUTH_FAILED",
          message: "fake-relay: token rejected",
        });
        this.closeSession(session);
        return;
      }
      entry.consumed = true; // 一次性：auth_ack 送达即消费
      session.authenticated = true;
      this.sendJson(session, {
        type: "auth_ack",
        device_sid: this.deviceSid,
        terminal_sid: "b1-fake-terminal-sid-0001",
        pair_status: this.pairScript.shift() ?? "matched",
        server_ts: Math.floor(Date.now() / 1000),
      });
      return;
    }
    if (msg.type === "data") {
      if (!session.authenticated) {
        // FAKE 策略：未认证 data 一律丢弃并断开；官方服务端对未认证 data 的处置未取证。
        this.stats.preAuthDataRejected += 1;
        this.sendJson(session, {
          type: "error",
          code: "AUTH_FAILED",
          message: "fake-relay: data before auth",
        });
        this.closeSession(session);
        return;
      }
      this.stats.dataAccepted += 1;
      return;
    }
    // pair_status_query 等其余消息本 fake 不需要。
  }

  private sendJson(session: ClientSession, value: unknown): void {
    if (!session.alive) return;
    try {
      session.socket.write(encodeServerFrame(JSON.stringify(value)));
    } catch {
      session.alive = false;
    }
  }

  private closeSession(session: ClientSession): void {
    if (!session.alive) return;
    session.alive = false;
    // RFC6455 close 帧（1000 正常关闭），随后半关。
    try {
      session.socket.write(Buffer.from([0x88, 0x02, 0x03, 0xe8]));
      session.socket.end();
    } catch {
      session.socket.destroy();
    }
  }
}

/** 解析客户端→服务端的 masked 文本帧；测试只用到 text/close。 */
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
// 测试脚手架
// ---------------------------------------------------------------------------

async function waitFor(check: () => boolean, timeoutMs = 2_000, stepMs = 20): Promise<void> {
  const deadline = Date.now() + timeoutMs;
  while (!check()) {
    if (Date.now() > deadline) {
      throw new Error("waitFor timeout");
    }
    await new Promise((resolve) => setTimeout(resolve, stepMs));
  }
}

/** 裸 WS 客户端（不经过被测模块），用于服务端门禁的直测。 */
function bareSocket(url: string): Promise<WebSocket> {
  return new Promise((resolve, reject) => {
    const ws = new WebSocket(url);
    ws.addEventListener("open", () => resolve(ws), { once: true });
    ws.addEventListener("error", () => reject(new Error("bare socket error")), { once: true });
  });
}

function nextEvent(ws: WebSocket): Promise<{ type: string; code?: string }> {
  return new Promise((resolve) => {
    const onMessage = (ev: WebSocketEventMap["message"]) => {
      ws.removeEventListener("message", onMessage);
      resolve(JSON.parse(String(ev.data)));
    };
    ws.addEventListener("message", onMessage);
  });
}

describe("zcode 官方桥接：fake relay 上的终端身份门槛（B1）", () => {
  let relay: FakeRelay;
  let port: number;
  let relayOrigin: string;
  let relayWsUrl: string;
  const connections: RelayTerminalConnection[] = [];
  const rawSockets: WebSocket[] = [];

  beforeEach(async () => {
    relay = new FakeRelay();
    port = await relay.listen();
    relayOrigin = `http://127.0.0.1:${port}`;
    relayWsUrl = `ws://127.0.0.1:${port}/api/v1/remote/ws2`;
  });

  afterEach(async () => {
    for (const conn of connections.splice(0)) {
      conn.close();
    }
    for (const ws of rawSockets.splice(0)) {
      try {
        ws.close();
      } catch {
        // 已关闭则忽略
      }
    }
    await relay.close();
  });

  function track(conn: RelayTerminalConnection): RelayTerminalConnection {
    connections.push(conn);
    return conn;
  }

  it("未认证客户端不能申请或消费 token，也不能发送 data 帧", async () => {
    // 无凭据 / 错凭据申请 token：均被 401 拒绝（webRemoteControlV2Token.ts:23 → auth-required）
    await expect(
      requestTerminalToken({ relayOrigin, accountJwt: relay.accountJwt + "-wrong" })
    ).resolves.toMatchObject({ kind: "auth-required" });
    await expect(requestTerminalToken({ relayOrigin, accountJwt: "" })).resolves.toMatchObject({
      kind: "auth-required",
    });
    expect(relay.stats.tokenHttp401).toBe(2);
    expect(relay.stats.tokenMinted).toBe(0); // 没有任何可消费的 token 被产出

    // 裸 WS：无 token 连接，auth_init 即被拒，其后 data 帧被丢弃不计数
    const ws = await bareSocket(relayWsUrl);
    rawSockets.push(ws);
    const failure = nextEvent(ws);
    ws.send(JSON.stringify({ type: "auth_init", device_sid: relay.deviceSid }));
    ws.send(JSON.stringify({ type: "data", payload: { probe: "must-not-pass" } }));
    await expect(failure).resolves.toMatchObject({ type: "error", code: "AUTH_FAILED" });
    await waitFor(() => relay.stats.preAuthDataRejected >= 1);
    expect(relay.stats.dataAccepted).toBe(0);
  });

  it("合法（fake）凭据可申请 role=terminal 一次性 token 并完成 auth_init→auth_ack 握手", async () => {
    const first = await requestTerminalToken({ relayOrigin, accountJwt: relay.accountJwt });
    expect(first).toMatchObject({ kind: "ok" });
    if (first.kind !== "ok") return;
    expect(first.expiresInSeconds).toBeGreaterThan(0);
    expect(OFFICIAL_TOKEN_SHAPE.test(first.token)).toBe(true); // 官方客户端侧 schema（contract.ts:121）

    // token 一次性、不缓存：每次申请铸造新值（webRemoteControlV2Token.ts:8）
    // 先断言第二次申请确已成功（kind/token 形状），再做不等式——避免失败态混过比较。
    const second = await requestTerminalToken({ relayOrigin, accountJwt: relay.accountJwt });
    expect(second).toMatchObject({ kind: "ok" });
    if (second.kind !== "ok") return;
    expect(second.token).not.toBe(first.token);
    expect(OFFICIAL_TOKEN_SHAPE.test(second.token)).toBe(true);

    const conn = track(
      new RelayTerminalConnection({ relayWsUrl, token: first.token, deviceSid: relay.deviceSid })
    );
    await expect(conn.connect()).resolves.toBe("paired");
    expect(conn.currentState).toBe("paired");

    // paired 后 data 帧被 relay 接受
    expect(conn.sendData({ probe: "b1-read-only-marker" })).toBe(true);
    await waitFor(() => relay.stats.dataAccepted === 1);
  });

  it("ws 升级 URL 条件性携带 mid：trim 非空才 set，空白/空串/缺省一律省略（官方条件）", async () => {
    // 官方条件 deviceMid?.trim() 非空才 searchParams.set("mid", …)：
    // web/.../webRemoteControlTransport.ts:107-110；desktop 设备角色同款 :117-119。
    // relay 靠 mid 路由到目标设备：缺席 → pair_status 恒 waiting → pair-timeout
    // （B1 实连两轮 macos-B120261024T104607Z-iykCdg / macos-B120261024T104720Z-OMksKw 的根因）。
    const tokenA = relay.mintToken();
    const connWithMid = track(
      new RelayTerminalConnection({
        relayWsUrl,
        token: tokenA,
        deviceSid: relay.deviceSid,
        deviceMid: "b1-fake-device-mid-0001",
      })
    );
    await expect(connWithMid.connect()).resolves.toBe("paired");
    expect(relay.wsUpgradeQueries[0].get("mid")).toBe("b1-fake-device-mid-0001");
    expect(relay.wsUpgradeQueries[0].get("token")).toBe(tokenA);
    connWithMid.close();

    // 空白 mid：trim 后为空 → 条件不成立 → 不携带 mid 参数
    const tokenB = relay.mintToken();
    const connBlankMid = track(
      new RelayTerminalConnection({
        relayWsUrl,
        token: tokenB,
        deviceSid: relay.deviceSid,
        deviceMid: "   ",
      })
    );
    await expect(connBlankMid.connect()).resolves.toBe("paired");
    expect(relay.wsUpgradeQueries[1].has("mid")).toBe(false);
    expect(relay.wsUpgradeQueries[1].get("token")).toBe(tokenB);
    connBlankMid.close();

    // 空串：同上省略
    const tokenC = relay.mintToken();
    const connEmptyMid = track(
      new RelayTerminalConnection({
        relayWsUrl,
        token: tokenC,
        deviceSid: relay.deviceSid,
        deviceMid: "",
      })
    );
    await expect(connEmptyMid.connect()).resolves.toBe("paired");
    expect(relay.wsUpgradeQueries[2].has("mid")).toBe(false);
    connEmptyMid.close();

    // 缺省（options 未传）：同样省略——既有用例的连接形状不变
    const tokenD = relay.mintToken();
    const connNoMid = track(
      new RelayTerminalConnection({ relayWsUrl, token: tokenD, deviceSid: relay.deviceSid })
    );
    await expect(connNoMid.connect()).resolves.toBe("paired");
    expect(relay.wsUpgradeQueries[3].has("mid")).toBe(false);
    expect(relay.wsUpgradeQueries[3].get("token")).toBe(tokenD);
  });

  it("pair_status=waiting 是合法前态：保活等待，收到 matched 才 paired", async () => {
    relay.setPairScript(["waiting"]);
    const conn = track(
      new RelayTerminalConnection({
        relayWsUrl,
        token: relay.mintToken(),
        deviceSid: relay.deviceSid,
        pairWindowMs: 5_000, // 官方 30s 窗口的测试短模（OFFICIAL_PAIR_WINDOW_MS 同因终态）
      })
    );
    let outcome: RelayConnectionResult | undefined;
    void conn.connect().then((result) => {
      outcome = result;
    });
    await waitFor(() => conn.currentState === "waiting");
    // 窗口内：连接保活、未终态、未 paired 不得发送 data（客户端门禁照常生效）
    await new Promise((resolve) => setTimeout(resolve, 60));
    expect(outcome).toBeUndefined();
    expect(conn.currentState).toBe("waiting");
    expect(conn.sendData({ probe: "while-waiting" })).toBe(false);
    expect(relay.stats.wsUpgrades).toBe(1);

    // 后续 auth_ack(matched)（官方 pair_status_query 心跳的回应形态）→ 才落 paired
    relay.sendPairAck("matched");
    await waitFor(() => outcome === "paired");
    expect(conn.currentState).toBe("paired");
    expect(conn.sendData({ probe: "after-matched" })).toBe(true);
    await waitFor(() => relay.stats.dataAccepted === 1);
  });

  it("waiting 配对窗口超时：结果为 pair-timeout（非 rejected），不发后续帧也不重连", async () => {
    relay.setPairScript(["waiting"]);
    const conn = track(
      new RelayTerminalConnection({
        relayWsUrl,
        token: relay.mintToken(),
        deviceSid: relay.deviceSid,
        pairWindowMs: 60, // 有界短窗口（官方 30s 的测试模，webRemoteControlTransport.ts:460-473）
      })
    );
    await expect(conn.connect()).resolves.toBe("pair-timeout");
    expect(conn.currentState).toBe("pair-timeout");
    // 超时 ≠ 显式拒绝：不得静默并案为 rejected（决策文件 §6.2-9 口径）
    expect(conn.currentState).not.toBe("rejected");
    expect(conn.sendData({ probe: "after-timeout" })).toBe(false);
    expect(relay.stats.dataAccepted).toBe(0);
    // 客户端无重连路径：升级计数不增长
    await new Promise((resolve) => setTimeout(resolve, 100));
    expect(relay.stats.wsUpgrades).toBe(1);
  });

  it("过期 token 与已消费 token 均被拒；认证失败后客户端不发后续帧", async () => {
    // 过期：60ms TTL 的 fake 直发 token，等其过期后连接
    const expired = relay.mintToken(60);
    await new Promise((resolve) => setTimeout(resolve, 200));
    const connExpired = track(
      new RelayTerminalConnection({ relayWsUrl, token: expired, deviceSid: relay.deviceSid })
    );
    await expect(connExpired.connect()).resolves.toBe("rejected");
    expect(connExpired.sendData({ probe: "after-expired" })).toBe(false); // 不发后续帧
    expect(relay.stats.wsAuthFailed).toBe(1);
    expect(relay.stats.dataAccepted).toBe(0);

    // 已消费：首次连接成功消费 token，重连同一 token 必须被拒（重连必须取新 token）
    const consumed = relay.mintToken();
    const connFirst = track(
      new RelayTerminalConnection({ relayWsUrl, token: consumed, deviceSid: relay.deviceSid })
    );
    await expect(connFirst.connect()).resolves.toBe("paired");
    connFirst.close();
    const connReuse = track(
      new RelayTerminalConnection({ relayWsUrl, token: consumed, deviceSid: relay.deviceSid })
    );
    await expect(connReuse.connect()).resolves.toBe("rejected");
    expect(connReuse.sendData({ probe: "after-reuse" })).toBe(false);
    expect(relay.stats.wsAuthFailed).toBe(2);
  });

  it("SID 形态字符串绝不作为 token 被接受", async () => {
    // SID 冒充账号凭据申请 token：401
    await expect(
      requestTerminalToken({ relayOrigin, accountJwt: relay.deviceSid })
    ).resolves.toMatchObject({ kind: "auth-required" });
    expect(relay.stats.tokenMinted).toBe(0);

    // SID 冒充 relay token 连 WS：形状即不符，auth_init 被拒
    const conn = track(
      new RelayTerminalConnection({
        relayWsUrl,
        token: relay.deviceSid,
        deviceSid: relay.deviceSid,
      })
    );
    await expect(conn.connect()).resolves.toBe("rejected");
    expect(conn.sendData({ probe: "sid-as-token" })).toBe(false);
    expect(relay.stats.dataAccepted).toBe(0);
  });

  it("KICKED 即停：进入终态且不发起任何新连接（无自动重连）", async () => {
    const token = await requestTerminalToken({ relayOrigin, accountJwt: relay.accountJwt });
    expect(token.kind).toBe("ok");
    if (token.kind !== "ok") return;
    const conn = track(
      new RelayTerminalConnection({ relayWsUrl, token: token.token, deviceSid: relay.deviceSid })
    );
    await expect(conn.connect()).resolves.toBe("paired");
    expect(relay.stats.wsUpgrades).toBe(1);

    relay.kick();
    await waitFor(() => conn.currentState === "kicked");
    expect(conn.sendData({ probe: "after-kicked" })).toBe(false);

    // 客户端没有重连逻辑：被踢后 upgrades 计数不增长（无循环重连的结构性保证）
    await new Promise((resolve) => setTimeout(resolve, 100));
    expect(relay.stats.wsUpgrades).toBe(1);
  });
});
