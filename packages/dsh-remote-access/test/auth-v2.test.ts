/**
 * R01 协议 v2 行为合同：两条真实握手都必须发送并签名完整 homeBinding。
 *
 * 仅替换外部 IPC、身份存储、Cookie 兑换和定时器；不 mock 握手或 crypto。
 * homeBinding 选项是待实现的本机有效 home/宿主代际输入接缝，不能从服务端回显取值。
 * 签名预期独立编码，刻意不复用待测 signingBytes，防止两侧一起漏签仍然通过。
 * 所有路径都是内存标签，不读取文件系统、宿主环境或真实 ~/.dsh。
 */

import { sha256Hex as digest } from "./wire-fixtures";
import {
  createPublicKey,
  generateKeyPairSync,
  sign,
  verify,
} from "node:crypto";
import { setImmediate as settle } from "node:timers/promises";
import { describe, expect, it } from "vitest";
import {
  BindingClient,
  BindingSession,
  type BindingScheduler,
  type BindingStorage,
  type IpcConnection,
  type Rendezvous,
} from "../src/auth.js";
import { BINDING_PROTOCOL } from "../src/types.js";

type Frame = Record<string, unknown>;
type HomeBinding = { homeRoot: string; hostPid: number; hostStartedAtMs: number };

const now = 1_760_000_000_000;
const localHome: HomeBinding = {
  homeRoot: "/r01-memory-only/自定义 home/.dsh",
  hostPid: 4202,
  hostStartedAtMs: now - 10_000,
};
const serverKeys = generateKeyPairSync("ed25519");
const serverSpki = serverKeys.publicKey.export({ type: "spki", format: "der" }) as Buffer;

/** 独立的 wire 合同编码；undefined 只用于构造旧协议的六字段签名反例。 */
function challengeBytes(hello: Frame, homeBinding: unknown): Buffer {
  return Buffer.from(JSON.stringify({
    bindingProtocol: hello["bindingProtocol"],
    keyDigests: [
      digest(serverSpki),
      digest(Buffer.from(String(hello["publicKeySpki"]), "base64url")),
    ],
    nonce: hello["nonce"],
    hostId: hello["hostId"],
    epoch: hello["epoch"],
    expiry: hello["expiry"],
    ...(homeBinding === undefined ? {} : { homeBinding }),
  }), "utf8");
}

function serverHello(hello: Frame, homeBinding: unknown = localHome): Frame {
  const reply: Frame = {
    type: "server_hello",
    bindingProtocol: hello["bindingProtocol"],
    publicKeySpki: serverSpki.toString("base64url"),
    nonce: hello["nonce"],
    expiry: hello["expiry"],
    hostId: hello["hostId"],
    epoch: hello["epoch"],
    homeBinding: { ...localHome },
    signature: sign(null, challengeBytes(hello, homeBinding), serverKeys.privateKey).toString("base64url"),
    state: "authorized",
    bindingId: "r01-binding",
    displayCode: null,
    pendingExpiresAt: null,
  };
  // 独立编码完整决策；固定顺序及显式 null 与 Rust wire 合同一致。
  const decision = Buffer.from(JSON.stringify({
    purpose: "tuvis.dsh.binding-decision.v2",
    ...JSON.parse(challengeBytes(hello, homeBinding).toString("utf8")),
    state: reply["state"],
    bindingId: reply["bindingId"],
    displayCode: reply["displayCode"],
    pendingExpiresAt: reply["pendingExpiresAt"],
  }), "utf8");
  reply["decisionSignature"] = sign(null, decision, serverKeys.privateKey).toString("base64url");
  return reply;
}

/** 旧六字段签名：即使回显完整 homeBinding，也不能让未被签名的字段取得效力。 */
function unsignedHomeReply(hello: Frame): Frame {
  const reply = serverHello(hello);
  reply["signature"] = sign(null, challengeBytes(hello, undefined), serverKeys.privateKey)
    .toString("base64url");
  return reply;
}

class MemoryConnection implements IpcConnection {
  readonly sent: Frame[] = [];
  closed = false;
  private readonly incoming: string[] = [];
  private hello: Frame | null = null;
  private decision: Frame | null = null;
  private waiter: { resolve: (line: string) => void; reject: (error: Error) => void } | null = null;

  constructor(private readonly reply: (hello: Frame) => Frame) {}

  async send(line: string): Promise<void> {
    if (this.closed) throw new Error("memory IPC closed");
    const frame = JSON.parse(line) as Frame;
    this.sent.push(frame);
    if (frame["type"] === "hello") {
      this.hello = frame;
      this.decision = this.reply(frame);
      this.push(this.decision);
    } else {
      if (!this.hello || !this.decision) throw new Error("credential before hello");
      const event = "credential_ack";
      const data = Buffer.from(JSON.stringify({
        purpose: "tuvis.dsh.binding-event.v2",
        ...JSON.parse(challengeBytes(this.hello, this.hello["homeBinding"]).toString("utf8")),
        decisionSignature: this.decision["decisionSignature"],
        event,
        bindingId: frame["bindingId"],
      }), "utf8");
      this.push({ type: "granted", bindingId: frame["bindingId"], event,
        eventSignature: sign(null, data, serverKeys.privateKey).toString("base64url") });
    }
  }

  async recv(): Promise<string> {
    const line = this.incoming.shift();
    if (line !== undefined) return line;
    if (this.closed) throw new Error("memory IPC closed");
    return new Promise<string>((resolve, reject) => { this.waiter = { resolve, reject }; });
  }

  private push(frame: Frame): void {
    const line = JSON.stringify(frame);
    const waiter = this.waiter;
    this.waiter = null;
    if (waiter) waiter.resolve(line);
    else this.incoming.push(line);
  }

  close(): void {
    this.closed = true;
    const waiter = this.waiter;
    this.waiter = null;
    waiter?.reject(new Error("memory IPC closed"));
  }
}

/** 重连和待批计时器只记录、不运行，确保拒绝断言不会被下一轮握手干扰。 */
class ManualScheduler implements BindingScheduler {
  private readonly timers = new Set<unknown>();
  setTimeout(run: () => void, _ms: number): unknown {
    this.timers.add(run);
    return run;
  }
  clearTimeout(handle: unknown): void { this.timers.delete(handle); }
}

function harness(
  kind: "BindingClient" | "BindingSession",
  config: {
    protocol?: number;
    homeBinding?: unknown;
    reply?: (hello: Frame) => Frame;
  } = {},
) {
  const { protocol = BINDING_PROTOCOL as number, reply = serverHello } = config;
  const homeBinding = Object.hasOwn(config, "homeBinding") ? config.homeBinding : { ...localHome };
  const connections: MemoryConnection[] = [];
  const exchanges: string[] = [];
  let identity: string | null = null;
  const storage: BindingStorage = {
    async read() { return identity; },
    async write(data) { identity = data; },
  };
  const rv: Rendezvous = {
    bindingProtocol: protocol,
    address: "memory://r01-v2",
    hostId: "r01-web-instance",
    epoch: "18446744073709551614",
    processIdentity: {
      platform: "test",
      executable: "dsh-web-memory-only",
      pid: localHome.hostPid,
      startedAtMs: localHome.hostStartedAtMs,
    },
    publicKeySpki: serverSpki.toString("base64url"),
  };
  // 结构化变量允许当前实现忽略新增输入，获得行为 RED，而不是构造器类型错误。
  const options = {
    homeBinding,
    transport: {
      async connect() {
        const conn = new MemoryConnection(reply);
        connections.push(conn);
        return conn;
      },
    },
    rendezvous: { async read() { return rv; } },
    storage,
    exchange: {
      async exchange() { exchanges.push("exchange"); return "memory-owner-cookie"; },
    },
    clock: () => now,
    scheduler: new ManualScheduler(),
  };
  const peer = kind === "BindingClient" ? new BindingClient(options) : new BindingSession(options);
  return {
    peer,
    connections,
    exchanges,
    frames: () => connections.flatMap((conn) => conn.sent),
    async run() {
      if (peer instanceof BindingClient) return await peer.runOnce();
      peer.start();
      // 一次事件循环边界排空内存握手的 Promise 链；无真实睡眠、无轮询次数假设。
      await settle();
      return undefined;
    },
    async dispose() {
      if (peer instanceof BindingSession) peer.dispose();
      for (const conn of connections) conn.close();
      await settle();
    },
  };
}

const missingLocalFields: Array<{ field: string; homeBinding: unknown }> = [
  { field: "homeBinding", homeBinding: undefined },
  { field: "non-null homeBinding", homeBinding: null },
  { field: "homeRoot", homeBinding: { hostPid: localHome.hostPid, hostStartedAtMs: localHome.hostStartedAtMs } },
  { field: "hostPid", homeBinding: { homeRoot: localHome.homeRoot, hostStartedAtMs: localHome.hostStartedAtMs } },
  { field: "hostStartedAtMs", homeBinding: { homeRoot: localHome.homeRoot, hostPid: localHome.hostPid } },
];

const changedHomes: Array<{ field: string; homeBinding: HomeBinding }> = [
  { field: "homeRoot", homeBinding: { ...localHome, homeRoot: "/r01-memory-only/other/.dsh" } },
  { field: "hostPid", homeBinding: { ...localHome, hostPid: 4203 } },
  { field: "hostStartedAtMs", homeBinding: { ...localHome, hostStartedAtMs: localHome.hostStartedAtMs + 1 } },
];

const invalidReplies: Array<{ defect: string; mutate: (reply: Frame) => void }> = [
  { defect: "v1 protocol", mutate: (reply) => { reply["bindingProtocol"] = 1; } },
  { defect: "missing homeBinding", mutate: (reply) => { delete reply["homeBinding"]; } },
  ...(["homeRoot", "hostPid", "hostStartedAtMs"] as const).map((field) => ({
    defect: `missing ${field}`,
    mutate: (reply: Frame) => { delete (reply["homeBinding"] as Frame)[field]; },
  })),
  ...changedHomes.map(({ field, homeBinding }) => ({
    defect: `tampered ${field}`,
    mutate: (reply: Frame) => { reply["homeBinding"] = homeBinding; },
  })),
];

for (const kind of ["BindingClient", "BindingSession"] as const) {
  describe(`R01 v2 ${kind}`, () => {
    it("sends complete local homeBinding in the actual hello", async () => {
      // 使用当前发布代际进入真实 send；协议仍为 v1 时也能单独暴露漏字段。
      const h = harness(kind);
      try {
        await h.run();
        const hello = h.frames().find((frame) => frame["type"] === "hello");
        expect(hello).toBeDefined();
        expect(hello?.["homeBinding"]).toEqual(localHome);
      } finally { await h.dispose(); }
    });

    it("signs complete homeBinding in the actual hello", async () => {
      const h = harness(kind);
      try {
        await h.run();
        const hello = h.frames().find((frame) => frame["type"] === "hello")!;
        expect(hello).toBeDefined();
        const key = createPublicKey({
          key: Buffer.from(String(hello["publicKeySpki"]), "base64url"),
          type: "spki",
          format: "der",
        });
        const signature = Buffer.from(String(hello["signature"]), "base64url");
        expect(verify(null, challengeBytes(hello, localHome), key, signature)).toBe(true);
        for (const { homeBinding } of changedHomes) {
          expect(verify(null, challengeBytes(hello, homeBinding), key, signature)).toBe(false);
        }
        expect(verify(null, challengeBytes(hello, undefined), key, signature)).toBe(false);
      } finally { await h.dispose(); }
    });

    it("accepts a valid v2 peer and delivers the credential after the signed handshake", async () => {
      const h = harness(kind, { protocol: 2 });
      try {
        await h.run();
        const hello = h.frames().find((frame) => frame["type"] === "hello");
        expect.soft(hello?.["bindingProtocol"]).toBe(2);
        expect.soft(h.peer.state).toBe("bound");
        expect.soft(h.exchanges).toEqual(["exchange"]);
        expect(h.frames().filter((frame) => frame["type"] === "credential")).toEqual([
          { type: "credential", bindingId: "r01-binding", cookie: "memory-owner-cookie" },
        ]);
      } finally { await h.dispose(); }
    });

    it("refuses a v1 rendezvous before opening IPC", async () => {
      const h = harness(kind, { protocol: 1, reply: unsignedHomeReply });
      try {
        const outcome = await h.run();
        expect.soft(h.connections).toHaveLength(0);
        expect.soft(h.exchanges).toEqual([]);
        expect.soft(h.frames().filter((frame) => frame["type"] === "credential")).toEqual([]);
        expect.soft(h.peer.state).toBe("idle");
        if (h.peer instanceof BindingClient) {
          expect(outcome).toEqual({ state: "refused", reason: "DSH_AUTH_PROTOCOL" });
        }
      } finally { await h.dispose(); }
    });

    it.each(missingLocalFields)("refuses local input missing $field before opening IPC", async ({ homeBinding }) => {
      const h = harness(kind, { homeBinding, reply: unsignedHomeReply });
      try {
        const outcome = await h.run();
        expect.soft(h.connections).toHaveLength(0);
        expect.soft(h.exchanges).toEqual([]);
        expect.soft(h.frames().filter((frame) => frame["type"] === "credential")).toEqual([]);
        expect.soft(h.peer.state).toBe("idle");
        if (h.peer instanceof BindingClient) {
          expect(outcome).toMatchObject({ state: "refused" });
        }
      } finally { await h.dispose(); }
    });

    it("rejects a six-field signature even when the server echoes complete homeBinding", async () => {
      const h = harness(kind, { reply: unsignedHomeReply });
      try {
        const outcome = await h.run();
        expect.soft(h.connections).toHaveLength(1);
        expect.soft(h.exchanges).toEqual([]);
        expect.soft(h.frames().filter((frame) => frame["type"] === "credential")).toEqual([]);
        expect.soft(h.connections[0]?.closed).toBe(true);
        expect.soft(h.peer.state).toBe("idle");
        if (h.peer instanceof BindingClient) {
          expect(outcome).toEqual({ state: "refused", reason: "DSH_AUTH_SIGNATURE" });
        }
      } finally { await h.dispose(); }
    });

    it.each(invalidReplies)("rejects server_hello with $defect before exchanging a credential", async ({ mutate }) => {
      const h = harness(kind, {
        reply: (hello) => {
          const reply = serverHello(hello);
          mutate(reply); // 签名之后篡改 wire；仅验本地消息但忽略回显不一致也必须失败。
          return reply;
        },
      });
      try {
        const outcome = await h.run();
        expect.soft(h.connections).toHaveLength(1);
        expect.soft(h.exchanges).toEqual([]);
        expect.soft(h.frames().filter((frame) => frame["type"] === "credential")).toEqual([]);
        expect.soft(h.connections[0]?.closed).toBe(true);
        expect.soft(h.peer.state).toBe("idle");
        if (h.peer instanceof BindingClient) {
          expect(outcome).toMatchObject({ state: "refused" });
        }
      } finally { await h.dispose(); }
    });
  });
}
