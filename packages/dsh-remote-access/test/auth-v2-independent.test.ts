/**
 * v2 独立复现：待批批准的信任锚、Rust 推送字段、验签异常恢复、挑战回显一致性。
 *
 * 只替换进程外边界（IPC、存储、兑换、时钟）；握手、签名与验签均为真实实现。
 * 挑战字节按 Rust wire 合同独立编码，不调用生产 signingBytes，不 mock crypto。
 * 全部状态留在内存；标记路径不访问真实 home、身份文件或凭据。
 *
 * Rust auth.rs 的 ServerFrame::{Granted, Revoked} 使用 binding_id；ServerHelloFrame
 * 则使用 camelCase。只有专门的 wire 用例使用 snake_case 推送；其余批准用例先以
 * camelCase 隔离信任锚问题，避免前一个协议缺陷遮住后一个生命周期缺陷。
 */
import { sha256Hex as digest } from "./wire-fixtures";
import {
  createPublicKey,
  generateKeyPairSync,
  sign,
  verify,
  type KeyObject,
} from "node:crypto";
import { setImmediate as settle } from "node:timers/promises";
import { describe, expect, it } from "vitest";
import {
  BindingClient,
  BindingSession,
  type BindingScheduler,
  type BindingStorage,
  type HomeBinding,
  type IpcConnection,
  type Rendezvous,
} from "../src/auth.js";

type Frame = Record<string, unknown>;
type PeerKind = "BindingClient" | "BindingSession";
type Keys = { publicKey: KeyObject; privateKey: KeyObject };

const NOW = 1_760_000_000_000;
const BINDING_ID = "independent-v2-binding";
const COOKIE = "memory-only-owner-cookie";
const HOME: HomeBinding = {
  homeRoot: "/independent-v2-memory/自定义 home/.dsh",
  hostPid: 8302,
  hostStartedAtMs: NOW - 20_000,
};
const originalKeys = generateKeyPairSync("ed25519");
const replacementKeys = generateKeyPairSync("ed25519");
const spki = (keys: Keys): Buffer => keys.publicKey.export({ type: "spki", format: "der" });

/** 固定 7 字段及嵌套字段顺序；协议号独立写为 2，避免常量漂移双方一起通过。 */
function challengeBytes(hello: Frame, keys: Keys): Buffer {
  const home = hello["homeBinding"] as HomeBinding;
  return Buffer.from(JSON.stringify({
    bindingProtocol: 2,
    keyDigests: [digest(spki(keys)), digest(Buffer.from(String(hello["publicKeySpki"]), "base64url"))],
    nonce: hello["nonce"],
    hostId: hello["hostId"],
    epoch: hello["epoch"],
    expiry: hello["expiry"],
    homeBinding: {
      homeRoot: home.homeRoot,
      hostPid: home.hostPid,
      hostStartedAtMs: home.hostStartedAtMs,
    },
  }), "utf8");
}

function signedReply(hello: Frame, keys: Keys, state: "pending_approval" | "authorized"): Frame {
  const reply: Frame = {
    type: "server_hello",
    bindingProtocol: 2,
    publicKeySpki: spki(keys).toString("base64url"),
    nonce: hello["nonce"],
    expiry: hello["expiry"],
    hostId: hello["hostId"],
    epoch: hello["epoch"],
    homeBinding: { ...(hello["homeBinding"] as HomeBinding) },
    signature: sign(null, challengeBytes(hello, keys), keys.privateKey).toString("base64url"),
    state,
    bindingId: BINDING_ID,
    displayCode: state === "pending_approval" ? "624108" : null,
    pendingExpiresAt: state === "pending_approval" ? hello["expiry"] : null,
  };
  // 决策签名独立编码，签入用途、完整挑战与全部决定字段；null 不能省略。
  const decision = Buffer.from(JSON.stringify({
    purpose: "tuvis.dsh.binding-decision.v2",
    ...JSON.parse(challengeBytes(hello, keys).toString("utf8")),
    state: reply["state"],
    bindingId: reply["bindingId"],
    displayCode: reply["displayCode"],
    pendingExpiresAt: reply["pendingExpiresAt"],
  }), "utf8");
  reply["decisionSignature"] = sign(null, decision, keys.privateKey).toString("base64url");
  return reply;
}

/** 保留原 camelCase/snake_case wire，独立签入本连接挑战、首帧决定与事件用途。 */
function signedEvent(
  hello: Frame, decision: Frame, frame: Frame,
  event: "approval" | "revocation" | "credential_ack", keys: Keys,
): Frame {
  const data = Buffer.from(JSON.stringify({
    purpose: "tuvis.dsh.binding-event.v2",
    ...JSON.parse(challengeBytes(hello, keys).toString("utf8")),
    decisionSignature: decision["decisionSignature"],
    event,
    bindingId: frame["bindingId"] ?? frame["binding_id"],
  }), "utf8");
  return { ...frame, event, eventSignature: sign(null, data, keys.privateKey).toString("base64url") };
}

/** 异步读是真正的队列等待；close 会解除 recv，防止失败用例留下悬挂循环。 */
class MemoryConnection implements IpcConnection {
  readonly sent: Frame[] = [];
  hello!: Frame;
  decision!: Frame;
  closed = false;
  private readonly incoming: string[] = [];
  private waiting: { resolve: (line: string) => void; reject: (error: Error) => void } | null = null;

  constructor(private readonly onSend: (frame: Frame, conn: MemoryConnection) => void) {}

  async send(line: string): Promise<void> {
    if (this.closed) throw new Error("memory IPC closed");
    const frame = JSON.parse(line) as Frame;
    this.sent.push(frame);
    this.onSend(frame, this);
  }

  async recv(): Promise<string> {
    const line = this.incoming.shift();
    if (line !== undefined) return line;
    if (this.closed) throw new Error("memory IPC closed");
    if (this.waiting) throw new Error("concurrent recv on memory IPC");
    return new Promise((resolve, reject) => { this.waiting = { resolve, reject }; });
  }

  push(frame: Frame): void {
    const line = JSON.stringify(frame);
    const waiting = this.waiting;
    this.waiting = null;
    if (waiting) waiting.resolve(line);
    else this.incoming.push(line);
  }

  close(): void {
    this.closed = true;
    const waiting = this.waiting;
    this.waiting = null;
    waiting?.reject(new Error("memory IPC closed"));
  }
}

/** 只在测试明确推进时运行定时器，不依赖真实睡眠、系统日期或固定微任务次数。 */
class ManualScheduler implements BindingScheduler {
  elapsedMs = 0;
  private nextId = 0;
  private readonly jobs = new Map<number, { atMs: number; run: () => void }>();

  setTimeout(run: () => void, ms: number): unknown {
    const id = ++this.nextId;
    this.jobs.set(id, { atMs: this.elapsedMs + ms, run });
    return id;
  }

  clearTimeout(handle: unknown): void { this.jobs.delete(handle as number); }

  delays(): number[] {
    return [...this.jobs.values()].map((job) => job.atMs - this.elapsedMs).sort((a, b) => a - b);
  }

  advance(ms: number): void {
    this.elapsedMs += ms;
    const due = [...this.jobs.entries()]
      .filter(([, job]) => job.atMs <= this.elapsedMs)
      .sort((a, b) => a[1].atMs - b[1].atMs);
    for (const [id, job] of due) {
      if (this.jobs.delete(id)) job.run();
    }
  }
}

class MemoryStorage implements BindingStorage {
  data: string | null = null;
  async read(): Promise<string | null> { return this.data; }
  async write(data: string): Promise<void> { this.data = data; }
  saved(): Frame | null { return this.data === null ? null : JSON.parse(this.data) as Frame; }
}

function harness(kind: PeerKind = "BindingSession", config: {
  initialState?: "pending_approval" | "authorized";
  keys?: Keys;
  storage?: MemoryStorage;
  ack?: "bindingId" | "binding_id";
  mutateReply?: (reply: Frame, attempt: number) => void;
} = {}) {
  const keys = config.keys ?? originalKeys;
  const storage = config.storage ?? new MemoryStorage();
  const scheduler = new ManualScheduler();
  const connections: MemoryConnection[] = [];
  const exchanged: string[] = [];
  const rv: Rendezvous = {
    bindingProtocol: 2,
    address: "memory://independent-v2",
    hostId: "independent-v2-host",
    epoch: "18446744073709551614",
    processIdentity: {
      platform: "memory-only",
      pid: HOME.hostPid,
      startedAtMs: HOME.hostStartedAtMs,
      executable: "memory-only-dsh-web",
    },
    publicKeySpki: spki(keys).toString("base64url"),
  };
  const options = {
    homeBinding: { ...HOME },
    rendezvous: { async read() { return rv; } },
    storage,
    scheduler,
    clock: () => NOW + scheduler.elapsedMs,
    exchange: { async exchange() { exchanged.push(COOKIE); return COOKIE; } },
    transport: {
      async connect() {
        const attempt = connections.length;
        const conn = new MemoryConnection((frame, socket) => {
          if (frame["type"] === "hello") {
            // 假服务端也验证真实客户端签名，不能凭一个任意 hello 就发授权帧。
            const clientKey = createPublicKey({
              key: Buffer.from(String(frame["publicKeySpki"]), "base64url"),
              format: "der",
              type: "spki",
            });
            if (!verify(null, challengeBytes(frame, keys), clientKey,
              Buffer.from(String(frame["signature"]), "base64url"))) {
              throw new Error("independent peer rejected client signature");
            }
            const reply = signedReply(frame, keys, config.initialState ?? "authorized");
            config.mutateReply?.(reply, attempt);
            socket.hello = frame;
            socket.decision = reply;
            socket.push(reply);
          } else if (frame["type"] === "credential") {
            // 不给缺少/错误 bindingId 的 credential 假成功 ACK。
            socket.push(frame["bindingId"] === BINDING_ID
              ? signedEvent(socket.hello, socket.decision,
                { type: "granted", [config.ack ?? "bindingId"]: BINDING_ID }, "credential_ack", keys)
              : { type: "error", error: "DSH_AUTH_UNKNOWN_BINDING", detail: "unknown memory binding" });
          } else {
            throw new Error("unexpected client frame");
          }
        });
        connections.push(conn);
        return conn;
      },
    },
  };
  const peer = kind === "BindingClient" ? new BindingClient(options) : new BindingSession(options);
  return {
    peer, storage, scheduler, connections, exchanged,
    credentials: () => connections.flatMap((conn) => conn.sent).filter((frame) => frame["type"] === "credential"),
    async run() {
      if (peer instanceof BindingClient) return await peer.runOnce();
      peer.start();
      // 内存 I/O 仅由 Promise 链完成，事件循环边界将它推进到稳定的 recv/定时器等待点。
      await settle();
      return undefined;
    },
    async push(frame: Frame) {
      const conn = connections[0]!;
      conn.push(frame["type"] === "granted" || frame["type"] === "revoked"
        ? signedEvent(conn.hello, conn.decision, frame,
          frame["type"] === "revoked" ? "revocation" : "approval", keys)
        : frame);
      await settle();
    },
    async dispose() {
      if (peer instanceof BindingSession) peer.dispose();
      for (const conn of connections) conn.close();
      await settle();
    },
  };
}

async function approveWithCamelCase(h: ReturnType<typeof harness>): Promise<void> {
  // 隔离信任锚/撤权问题：此处故意沿用 TS 已支持形状，Rust 原始 wire 另有独立用例。
  expect(h.peer.state).toBe("pending");
  expect(h.exchanged).toEqual([]);
  await h.push({ type: "granted", bindingId: BINDING_ID });
  expect(h.peer.snapshot()).toEqual({ state: "bound", bindingId: BINDING_ID, displayCode: null });
  expect(h.credentials()).toEqual([{ type: "credential", bindingId: BINDING_ID, cookie: COOKIE }]);
}

describe("v2 independent: trust anchor after pending approval", () => {
  it("persists the verified server key when approval on the pending connection succeeds", async () => {
    const h = harness("BindingSession", { initialState: "pending_approval" });
    try {
      await h.run();
      await approveWithCamelCase(h);
      expect(h.storage.saved()?.["trustAnchorSpki"]).toBe(spki(originalKeys).toString("base64url"));
    } finally { await h.dispose(); }
  });

  it("rejects a replacement rendezvous key after restarting an approved pending session", async () => {
    const storage = new MemoryStorage();
    const first = harness("BindingSession", { storage, initialState: "pending_approval" });
    try {
      await first.run();
      await approveWithCamelCase(first);
    } finally { await first.dispose(); }

    // 第二次构造会话，只从同一存储恢复；新服务端持有另一把真实有效的 Ed25519 钥。
    const restarted = harness("BindingSession", { storage, keys: replacementKeys });
    try {
      await restarted.run();
      expect.soft(restarted.connections, "pinned key mismatch must be rejected before IPC").toHaveLength(0);
      expect.soft(restarted.exchanged).toEqual([]);
      expect.soft(restarted.credentials()).toEqual([]);
      expect(restarted.peer.state).toBe("idle");
    } finally { await restarted.dispose(); }
  });

  it("rejects a replacement key after a directly authorized session as a control", async () => {
    const storage = new MemoryStorage();
    const first = harness("BindingSession", { storage });
    try {
      await first.run();
      expect(first.peer.state).toBe("bound");
      expect(storage.saved()?.["trustAnchorSpki"]).toBe(spki(originalKeys).toString("base64url"));
    } finally { await first.dispose(); }
    const restarted = harness("BindingSession", { storage, keys: replacementKeys });
    try {
      await restarted.run();
      expect(restarted.connections).toHaveLength(0);
      expect(restarted.exchanged).toEqual([]);
    } finally { await restarted.dispose(); }
  });
});

describe("v2 independent: Rust Granted/Revoked wire", () => {
  it("uses binding_id from a Rust approval push when delivering the pending credential", async () => {
    const h = harness("BindingSession", { initialState: "pending_approval", ack: "binding_id" });
    try {
      await h.run();
      expect(h.peer.state).toBe("pending");
      expect(h.exchanged).toEqual([]);
      // 与 Rust enum struct variant 的 serde 输出一致，不做 camelCase 转换。
      await h.push({ type: "granted", binding_id: BINDING_ID });
      expect.soft(h.credentials()).toEqual([{ type: "credential", bindingId: BINDING_ID, cookie: COOKIE }]);
      expect(h.peer.snapshot()).toEqual({ state: "bound", bindingId: BINDING_ID, displayCode: null });
    } finally { await h.dispose(); }
  });

  it("retains binding_id in the persisted tombstone when Rust revokes a pending connection", async () => {
    const h = harness("BindingSession", { initialState: "pending_approval" });
    try {
      await h.run();
      expect(h.peer.state).toBe("pending");
      await h.push({ type: "revoked", binding_id: BINDING_ID });
      expect.soft(h.peer.snapshot()).toEqual({ state: "revoked", bindingId: BINDING_ID, displayCode: null });
      expect.soft(h.storage.saved()).toMatchObject({ revoked: true, revokedBindingId: BINDING_ID });
      expect(h.exchanged).toEqual([]);
      expect(h.connections[0]!.closed).toBe(true);
      expect(h.scheduler.delays()).toEqual([]);
    } finally { await h.dispose(); }
  });

  it("retains binding_id when Rust revokes a connection approved during its read loop", async () => {
    const h = harness("BindingSession", { initialState: "pending_approval" });
    try {
      await h.run();
      await approveWithCamelCase(h);
      await h.push({ type: "revoked", binding_id: BINDING_ID });
      expect.soft(h.peer.snapshot()).toEqual({ state: "revoked", bindingId: BINDING_ID, displayCode: null });
      expect(h.storage.saved()).toMatchObject({ revoked: true, revokedBindingId: BINDING_ID });
    } finally { await h.dispose(); }
  });

  it("accepts a Rust snake_case credential ACK on the directly authorized path as a control", async () => {
    const h = harness("BindingSession", { ack: "binding_id" });
    try {
      await h.run();
      expect(h.peer.snapshot()).toEqual({ state: "bound", bindingId: BINDING_ID, displayCode: null });
      expect(h.credentials()).toEqual([{ type: "credential", bindingId: BINDING_ID, cookie: COOKIE }]);
    } finally { await h.dispose(); }
  });
});

const malformedSignatures: Array<{ defect: string; mutate: (reply: Frame) => void }> = [
  { defect: "missing signature", mutate: (reply) => { delete reply["signature"]; } },
  { defect: "null signature", mutate: (reply) => { reply["signature"] = null; } },
  { defect: "object signature", mutate: (reply) => { reply["signature"] = { invalid: true }; } },
];

describe("v2 independent: malformed server_hello recovery", () => {
  it.each(malformedSignatures)("closes and retries after $defect reaches signature validation", async ({ mutate }) => {
    const h = harness("BindingSession", {
      mutateReply: (reply, attempt) => { if (attempt === 0) mutate(reply); },
    });
    try {
      await h.run();
      expect(h.connections).toHaveLength(1);
      expect(h.peer.state).toBe("idle");
      expect(h.exchanged).toEqual([]);
      expect(h.credentials()).toEqual([]);
      expect.soft(h.connections[0]!.closed, "failed verification must release IPC").toBe(true);
      expect.soft(h.scheduler.delays(), "verification exceptions must schedule the first retry").toEqual([1_000]);
      h.scheduler.advance(1_000);
      await settle();
      expect.soft(h.connections, "retry must establish a fresh connection").toHaveLength(2);
      expect.soft(h.peer.state).toBe("bound");
      expect(h.credentials()).toEqual([{ type: "credential", bindingId: BINDING_ID, cookie: COOKIE }]);
    } finally { await h.dispose(); }
  });

  it("closes and retries a string signature with one corrupted byte as a control", async () => {
    const h = harness("BindingSession", {
      mutateReply: (reply, attempt) => {
        if (attempt !== 0) return;
        const signature = Buffer.from(String(reply["signature"]), "base64url");
        signature[0] = signature[0]! ^ 1;
        reply["signature"] = signature.toString("base64url");
      },
    });
    try {
      await h.run();
      expect(h.exchanged).toEqual([]);
      expect(h.connections[0]!.closed).toBe(true);
      expect(h.scheduler.delays()).toEqual([1_000]);
      h.scheduler.advance(1_000);
      await settle();
      expect(h.connections).toHaveLength(2);
      expect(h.peer.state).toBe("bound");
      expect(h.credentials()).toEqual([{ type: "credential", bindingId: BINDING_ID, cookie: COOKIE }]);
    } finally { await h.dispose(); }
  });
});

/** 每个挑战回显逐项变更/删除；签名保留原本有效的签名，抓“只验本地消息不核对回显”。 */
const echoFields: Array<{ field: string; changed: unknown }> = [
  { field: "bindingProtocol", changed: 1 },
  { field: "publicKeySpki", changed: spki(replacementKeys).toString("base64url") },
  { field: "nonce", changed: Buffer.alloc(32, 0x42).toString("base64url") },
  { field: "expiry", changed: NOW + 59_999 },
  { field: "hostId", changed: "different-memory-host" },
  { field: "epoch", changed: "18446744073709551613" },
  { field: "homeBinding.homeRoot", changed: "/independent-v2-memory/different/.dsh" },
  { field: "homeBinding.hostPid", changed: HOME.hostPid + 1 },
  { field: "homeBinding.hostStartedAtMs", changed: HOME.hostStartedAtMs + 1 },
];
const echoMutations = echoFields.flatMap(({ field, changed }) => (
  ["changed", "missing"] as const
).map((defect) => ({
  field, defect,
  mutate(reply: Frame) {
    const parts = field.split(".");
    const target = parts.length === 1 ? reply : reply[parts[0]!] as Frame;
    const key = parts.at(-1)!;
    if (defect === "missing") delete target[key];
    else target[key] = changed;
  },
})));

for (const kind of ["BindingClient", "BindingSession"] as const) {
  describe(`v2 independent: ${kind} server_hello challenge echoes`, () => {
    it("delivers exactly one credential for an intact independently signed v2 reply", async () => {
      const h = harness(kind);
      try {
        await h.run();
        expect(h.peer.snapshot()).toEqual({ state: "bound", bindingId: BINDING_ID, displayCode: null });
        expect(h.exchanged).toEqual([COOKIE]);
        expect(h.credentials()).toEqual([{ type: "credential", bindingId: BINDING_ID, cookie: COOKIE }]);
        expect(h.storage.saved()?.["trustAnchorSpki"]).toBe(spki(originalKeys).toString("base64url"));
      } finally { await h.dispose(); }
    });

    it.each(echoMutations)("rejects $defect $field before credential exchange", async ({ mutate }) => {
      const h = harness(kind, { mutateReply: mutate });
      try {
        const outcome = await h.run();
        expect(h.connections).toHaveLength(1);
        expect.soft(h.exchanged, "a mismatched echo must not authorize exchange").toEqual([]);
        expect.soft(h.credentials()).toEqual([]);
        expect.soft(h.connections[0]!.closed).toBe(true);
        expect.soft(h.peer.state).toBe("idle");
        expect.soft(h.storage.saved()?.["trustAnchorSpki"]).toBeNull();
        if (kind === "BindingClient") {
          expect(outcome).toMatchObject({ state: "refused" });
        } else {
          expect(h.scheduler.delays()).toEqual([1_000]);
        }
      } finally { await h.dispose(); }
    });
  });
}
