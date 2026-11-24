/**
 * T06 插件侧本机授权测试（brief 指定四类行为）：
 *
 * (a) plugin_refuses_to_deliver_before_approval —— 未批准前插件不兑换、不交付
 *     任何凭据材料；
 * (b) replayed_challenge_is_rejected_and_nonce_rotates —— 服务端拒重放
 *     （DSH_AUTH_REPLAY）后，插件下一次 hello 换新 nonce；
 * (c) stale_epoch_is_rejected_and_rendezvous_refreshed —— 旧代际被拒
 *     （DSH_AUTH_EPOCH_STALE）后，插件重读 rendezvous 用新代际；
 * (d) on_revoke_plugin_stops_delivering —— 撤权后插件停止交付新凭据。
 *
 * 全部走假 IPC 传输 / 假存储 / 假兑换（无真实套接字、无真实 /connect；真实
 * /connect 兑换发生在宿主内，属 G1 实机验证项）。假服务端持**真实 Ed25519
 * 密钥**并对挑战消息真签（插件会验 Rust 签名——与生产 wire 行为一致）。
 * 观测面 = 假传输录得的 wire 帧与假兑换的调用记录。
 */

import { sha256Hex } from "./wire-fixtures";
import { generateKeyPairSync, sign as cryptoSign } from "node:crypto";
import { describe, expect, it } from "vitest";
import {
  BindingClient,
  signingBytes,
  type BindingStorage,
  type CookieExchange,
  type HomeBinding,
  type IpcConnection,
  type IpcTransport,
  type Rendezvous,
} from "../src/auth.js";
import { BINDING_PROTOCOL } from "../src/types.js";

// ============================================================
// 假 Rust 侧（真实 Ed25519：SPKI 进 rendezvous，挑战消息真签）
// ============================================================

const rustKeys = generateKeyPairSync("ed25519");
const rustSpkiDer = rustKeys.publicKey.export({ type: "spki", format: "der" }) as Buffer;
const homeBinding: HomeBinding = {
  homeRoot: "/t06-memory-only/.dsh",
  hostPid: 1,
  hostStartedAtMs: 1_760_000_000_000,
};

function b64(bytes: Buffer): string {
  return bytes.toString("base64").replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");
}


function challengeBytes(hello: Record<string, unknown>): Buffer {
  return signingBytes({
    bindingProtocol: BINDING_PROTOCOL,
    keyDigests: [
      sha256Hex(rustSpkiDer),
      sha256Hex(Buffer.from(String(hello["publicKeySpki"]), "base64")),
    ],
    nonce: String(hello["nonce"]),
    hostId: String(hello["hostId"]),
    epoch: String(hello["epoch"]),
    expiry: Number(hello["expiry"]),
    homeBinding: hello["homeBinding"] as HomeBinding,
  });
}

/** 对完整七字段挑战及服务端决策分别做真实 Ed25519 签名。 */
function serverHello(
  hello: Record<string, unknown>,
  state: string,
  bindingId: string,
  displayCode: string | null,
): string {
  const challenge = challengeBytes(hello);
  const pendingExpiresAt = null;
  const decision = Buffer.from(JSON.stringify({
    purpose: "tuvis.dsh.binding-decision.v2",
    ...JSON.parse(challenge.toString("utf8")),
    state,
    bindingId,
    displayCode,
    pendingExpiresAt,
  }), "utf8");
  return JSON.stringify({
    type: "server_hello",
    bindingProtocol: BINDING_PROTOCOL,
    publicKeySpki: b64(rustSpkiDer),
    nonce: hello["nonce"],
    expiry: hello["expiry"],
    homeBinding: hello["homeBinding"],
    hostId: hello["hostId"],
    epoch: hello["epoch"],
    signature: b64(cryptoSign(null, challenge, rustKeys.privateKey)),
    decisionSignature: b64(cryptoSign(null, decision, rustKeys.privateKey)),
    state,
    bindingId,
    displayCode,
    pendingExpiresAt,
  });
}

// ============================================================
// 假件：脚本化服务端 + 记录型传输/存储/兑换
// ============================================================

/** 服务端脚本的一步：收到一帧后回一帧（push 帧用 emit）。 */
class FakeConnection implements IpcConnection {
  sent: string[] = [];
  closed = false;
  private pending: string[] = [];
  private waiting: ((line: string) => void) | null = null;
  private decisionSignature: string | null = null;

  constructor(private readonly server: (frame: string, conn: FakeConnection) => void) {}

  async send(frame: string): Promise<void> {
    if (this.closed) throw new Error("fake ipc: connection closed");
    this.sent.push(frame);
    this.server(frame, this);
  }

  async recv(): Promise<string> {
    const line = this.pending.shift();
    if (line !== undefined) return line;
    return await new Promise<string>((resolve) => {
      this.waiting = resolve;
    });
  }

  /** 服务端主动推帧（granted / revoked）。 */
  emit(line: string): void {
    const frame = JSON.parse(line) as Record<string, unknown>;
    if (frame["type"] === "server_hello") {
      this.decisionSignature = frame["decisionSignature"] as string;
    }
    const waiter = this.waiting;
    this.waiting = null;
    if (waiter) waiter(line);
    else this.pending.push(line);
  }

  /** 显式签发事件；错误帧等负例仍直接经 emit 原样发送。 */
  emitEvent(event: "approval" | "revocation" | "credential_ack", bindingId: string): void {
    const hello = this.helloFrames().at(-1);
    if (!hello || !this.decisionSignature) {
      throw new Error("fixture: signed event requires this connection's server hello");
    }
    const bytes = Buffer.from(JSON.stringify({
      purpose: "tuvis.dsh.binding-event.v2",
      ...JSON.parse(challengeBytes(hello).toString("utf8")),
      decisionSignature: this.decisionSignature,
      event,
      bindingId,
    }), "utf8");
    this.emit(JSON.stringify({
      type: event === "revocation" ? "revoked" : "granted",
      bindingId,
      event,
      eventSignature: b64(cryptoSign(null, bytes, rustKeys.privateKey)),
    }));
  }

  close(): void {
    this.closed = true;
  }

  /** 客户端发出的 hello 帧序列（观测 nonce/epoch 用）。 */
  helloFrames(): Array<Record<string, unknown>> {
    return this.sent
      .map((line) => JSON.parse(line) as Record<string, unknown>)
      .filter((frame) => frame["type"] === "hello");
  }

  credentialFrames(): string[] {
    return this.sent.filter((line) => line.includes('"type":"credential"'));
  }
}

function fakeTransport(
  server: (frame: string, conn: FakeConnection) => void,
): IpcTransport & { connections: FakeConnection[] } {
  // 每次握手尝试一条新连接（与生产一致）；脚本闭包共享服务端状态。
  const connections: FakeConnection[] = [];
  return {
    connections,
    async connect(): Promise<IpcConnection> {
      const conn = new FakeConnection(server);
      connections.push(conn);
      return conn;
    },
  };
}

function fakeStorage(initial: string | null = null): BindingStorage & { data: string | null } {
  const box: { data: string | null } = { data: initial };
  return {
    get data() {
      return box.data;
    },
    set data(v: string | null) {
      box.data = v;
    },
    async read() {
      return box.data;
    },
    async write(data: string) {
      box.data = data;
    },
  };
}

function fakeExchange(calls: string[]): CookieExchange {
  return {
    async exchange() {
      calls.push("exchange");
      return "owner-cookie-from-connect";
    },
  };
}

function rendezvousOf(epoch: string): Rendezvous {
  return {
    bindingProtocol: BINDING_PROTOCOL,
    address: "mem://tuvis-dsh-auth",
    hostId: "local",
    epoch,
    processIdentity: {
      platform: "test",
      pid: 1,
      startedAtMs: 1_760_000_000_000,
      executable: "DeepSeek Harness",
    },
    publicKeySpki: b64(rustSpkiDer),
  };
}

// ============================================================
// 签名消息稳定性（两侧逐字节对齐是握手成立的前提）
// ============================================================

describe("签名消息（固定字段数组）", () => {
  it("signing bytes bind the web home and host generation", () => {
    const message = {
      bindingProtocol: 2,
      keyDigests: ["rust", "client"] as [string, string],
      nonce: "abc",
      hostId: "instance",
      epoch: "123",
      expiry: 1_760_000_060_000,
      homeBinding: { homeRoot: "/home/a/.dsh", hostPid: 42, hostStartedAtMs: 1000 },
    };
    expect(JSON.parse(signingBytes(message).toString("utf8"))).toMatchObject({
      homeBinding: message.homeBinding,
    });
    expect(signingBytes(message).equals(signingBytes({
      ...message,
      homeBinding: { ...message.homeBinding, homeRoot: "/home/b/.dsh" },
    }))).toBe(false);
  });

  it("signing_bytes_use_fixed_field_order", () => {
    const bytes = signingBytes({
      bindingProtocol: BINDING_PROTOCOL,
      keyDigests: ["rust", "client"],
      nonce: "abc",
      hostId: "local",
      epoch: "123",
      expiry: 1_760_000_060_000,
      homeBinding,
    });
    expect(bytes.toString("utf8")).toBe(
      '{"bindingProtocol":2,"keyDigests":["rust","client"],"nonce":"abc","hostId":"local","epoch":"123","expiry":1760000060000,"homeBinding":{"homeRoot":"/t06-memory-only/.dsh","hostPid":1,"hostStartedAtMs":1760000000000}}',
    );
  });
});

// ============================================================
// brief 四类行为
// ============================================================

describe("T06 插件授权行为", () => {
  it("plugin_refuses_to_deliver_before_approval", async () => {
    const exchanges: string[] = [];
    const rv = rendezvousOf("111");
    const transport = fakeTransport((frame, c) => {
      const hello = JSON.parse(frame) as Record<string, unknown>;
      if (hello["type"] !== "hello") return;
      // 服务端：新公钥 → pending_approval（带两端核对码 + 真签），不授权。
      c.emit(serverHello(hello, "pending_approval", "binding-1", "AB23CD"));
    });
    const client = new BindingClient({
      homeBinding,
      transport,
      rendezvous: { read: async () => rv },
      storage: fakeStorage(),
      exchange: fakeExchange(exchanges),
      clock: () => 1_760_000_000_000,
    });

    const outcome = await client.runOnce();

    // brief 最低断言：未批准 → 不兑换、不交付任何凭据材料。
    expect(outcome).toMatchObject({ state: "pending_approval" });
    expect(exchanges).toEqual([]);
    for (const conn of transport.connections) {
      expect(conn.credentialFrames()).toEqual([]);
    }
    expect(client.state).toBe("pending");
    // 快照面（本地 UI 数据源）：核对码可见、无任何秘密。
    expect(client.snapshot()).toEqual({
      state: "pending",
      bindingId: "binding-1",
      displayCode: "AB23CD",
    });
  });

  it("replayed_challenge_is_rejected_and_nonce_rotates", async () => {
    const rv = rendezvousOf("222");
    const transport = fakeTransport((_frame, c) => {
      // 服务端纪律演示：nonce 一次性（每次尝试都回重放拒绝，逼插件换 nonce）。
      c.emit(
        JSON.stringify({
          type: "error",
          error: "DSH_AUTH_REPLAY",
          detail: "nonce already used",
        }),
      );
    });
    const client = new BindingClient({
      homeBinding,
      transport,
      rendezvous: { read: async () => rv },
      storage: fakeStorage(),
      exchange: fakeExchange([]),
      clock: () => 1_760_000_000_000,
    });

    const first = await client.runOnce();
    expect(first).toMatchObject({ state: "refused", reason: "DSH_AUTH_REPLAY" });

    const second = await client.runOnce();
    expect(second).toMatchObject({ state: "refused", reason: "DSH_AUTH_REPLAY" });

    // 重放被拒后，下一次 hello 必须换新 nonce（一次性纪律的客户端面）。
    const hellos = transport.connections.flatMap((c) => c.helloFrames());
    expect(hellos.length).toBe(2);
    const nonces = new Set(hellos.map((h) => h["nonce"]));
    expect(nonces.size).toBe(2);
  });

  it("stale_epoch_is_rejected_and_rendezvous_refreshed", async () => {
    // 宿主换代：rendezvous 读出的代际从 333 → 444（插件重读后跟随）。
    let epoch = "333";
    const rv = (): Rendezvous => rendezvousOf(epoch);
    const transport = fakeTransport((frame, c) => {
      const hello = JSON.parse(frame) as Record<string, unknown>;
      if (hello["type"] !== "hello") return;
      if (hello["epoch"] === "333") {
        c.emit(
          JSON.stringify({
            type: "error",
            error: "DSH_AUTH_EPOCH_STALE",
            detail: "host regenerated",
          }),
        );
      } else {
        c.emit(serverHello(hello, "pending_approval", "binding-2", "XY45ZV"));
      }
    });
    const client = new BindingClient({
      homeBinding,
      transport,
      rendezvous: { read: async () => rv() },
      storage: fakeStorage(),
      exchange: fakeExchange([]),
      clock: () => 1_760_000_000_000,
    });

    const first = await client.runOnce();
    expect(first).toMatchObject({ state: "refused", reason: "DSH_AUTH_EPOCH_STALE" });

    // 宿主换代后（rendezvous 更新代际），插件下一次握手携带新代际。
    epoch = "444";
    const second = await client.runOnce();
    const lastHello = transport.connections.at(-1)!.helloFrames().at(-1)!;
    expect(lastHello["epoch"]).toBe("444");
    expect(second).toMatchObject({ state: "pending_approval" });
  });

  it("on_revoke_plugin_stops_delivering", async () => {
    const exchanges: string[] = [];
    const rv = rendezvousOf("555");
    let approved = false;
    const transport = fakeTransport((frame, c) => {
      const parsed = JSON.parse(frame) as Record<string, unknown>;
      if (parsed["type"] === "credential") {
        c.emitEvent("credential_ack", String(parsed["bindingId"]));
        return;
      }
      if (parsed["type"] !== "hello") return;
      if (!approved) {
        approved = true;
        c.emit(serverHello(parsed, "authorized", "binding-3", null));
      } else {
        // 重连首帧必须用签名决策确认撤权，才能落盘永久墓碑。
        c.emit(serverHello(parsed, "revoked", "binding-3", null));
      }
    });
    const storage = fakeStorage();
    const client = new BindingClient({
      homeBinding,
      transport,
      rendezvous: { read: async () => rv },
      storage,
      exchange: fakeExchange(exchanges),
      clock: () => 1_760_000_000_000,
    });

    // 授权代：兑换 + 交付一次 cookie（受信期间正常行为）。
    const authorized = await client.runOnce();
    expect(authorized).toMatchObject({ state: "authorized", cookieDelivered: true });
    expect(exchanges).toEqual(["exchange"]);
    expect(transport.connections[0]!.credentialFrames()).toHaveLength(1);

    // 撤权代：插件收到 revoked → 停止交付新凭据（不再兑换、不再发 credential 帧）。
    const revoked = await client.runOnce();
    expect(revoked).toMatchObject({ state: "revoked", bindingId: "binding-3" });
    expect(client.state).toBe("revoked");
    const totalCredentialFrames = transport.connections.flatMap((c) => c.credentialFrames());
    expect(totalCredentialFrames).toHaveLength(1);
    expect(exchanges).toEqual(["exchange"]);

    // 撤权态持久化（重启后仍不交付）；resetIdentity 是重新绑定的唯一路径。
    const persisted = JSON.parse(storage.data ?? "{}") as Record<string, unknown>;
    expect(persisted["revoked"]).toBe(true);
    await client.resetIdentity();
    const afterReset = JSON.parse(storage.data ?? "{}") as Record<string, unknown>;
    expect(afterReset["revoked"]).toBe(false);
  });
});
