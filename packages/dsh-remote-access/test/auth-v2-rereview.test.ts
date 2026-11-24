/**
 * v2 复审独立行为测试：跨域 ACK 竞态、验签前墓碑、未知帧、ACK 对位与异常清理。
 * 不复用其他测试的假件，不调用生产 signingBytes，不 mock crypto 或握手。
 * 域切换使用真实 instanceFileStorage / instanceRendezvousReader 和独立临时目录；
 * 其他用例仅替换外部 IPC、存储、兑换和调度器。所有 cookie 都是内存标签。
 */
import { sha256Hex as digest } from "./wire-fixtures";
import {
  createPublicKey,
  generateKeyPairSync,
  sign,
  verify,
  type KeyObject,
} from "node:crypto";
import { mkdtempSync, mkdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
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
  type RendezvousReader,
} from "../src/auth.js";
import { instanceFileStorage, instanceRendezvousReader } from "../src/index.js";

type Frame = Record<string, unknown>;
type BindingEvent = "approval" | "revocation" | "credential_ack";
type Keys = { publicKey: KeyObject; privateKey: KeyObject };
type Kind = "BindingClient" | "BindingSession";
const NOW = 1_760_000_000_000;
const ID = "rereview-v2-approved-binding";
const OTHER_ID = "rereview-v2-other-binding";
const COOKIE = "rereview-memory-cookie";
const keys = generateKeyPairSync("ed25519");
const instanceKeys = generateKeyPairSync("ed25519");
const publicSpki = (keyPair: Keys): string => keyPair.publicKey
  .export({ type: "spki", format: "der" }).toString("base64url");

function rendezvous(keyPair: Keys = keys, hostId = "rereview-legacy-host"): Rendezvous {
  return {
    bindingProtocol: 2,
    address: `memory://${hostId}`,
    hostId,
    epoch: "18446744073709551614",
    processIdentity: {
      platform: "memory-test",
      pid: process.pid,
      startedAtMs: NOW - 20_000,
      executable: "memory-only-dsh-web",
    },
    publicKeySpki: publicSpki(keyPair),
  };
}

/** 独立编码 v2 挑战合同；advertisedSpki 可注入非法 DER 以真实触发 createPublicKey。 */
function challengeBytes(hello: Frame, advertisedSpki: string): Buffer {
  const home = hello["homeBinding"] as HomeBinding;
  return Buffer.from(JSON.stringify({
    bindingProtocol: 2,
    keyDigests: [
      digest(Buffer.from(advertisedSpki, "base64url")),
      digest(Buffer.from(String(hello["publicKeySpki"]), "base64url")),
    ],
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

function signedReply(hello: Frame, rv: Rendezvous, keyPair: Keys, state: string): Frame {
  const challenge = challengeBytes(hello, rv.publicKeySpki);
  const reply: Frame = {
    type: "server_hello",
    bindingProtocol: 2,
    publicKeySpki: rv.publicKeySpki,
    nonce: hello["nonce"],
    hostId: hello["hostId"],
    epoch: hello["epoch"],
    expiry: hello["expiry"],
    homeBinding: { ...(hello["homeBinding"] as HomeBinding) },
    signature: sign(null, challenge, keyPair.privateKey).toString("base64url"),
    state,
    bindingId: ID,
    displayCode: state === "pending_approval" ? "531246" : null,
    pendingExpiresAt: state === "pending_approval" ? hello["expiry"] : null,
  };
  // 独立固定合同；负例仍在生成真实双签名后改帧，不能靠缺少 decisionSignature 误过。
  const decision = Buffer.from(JSON.stringify({
    purpose: "tuvis.dsh.binding-decision.v2",
    ...JSON.parse(challenge.toString("utf8")) as Frame,
    state, bindingId: ID, displayCode: reply["displayCode"], pendingExpiresAt: reply["pendingExpiresAt"],
  }), "utf8");
  reply["decisionSignature"] = sign(null, decision, keyPair.privateKey).toString("base64url");
  return reply;
}

class MemoryStorage implements BindingStorage {
  data: string | null = null;
  async read(): Promise<string | null> { return this.data; }
  async write(data: string): Promise<void> { this.data = data; }
  saved(): Frame | null { return this.data === null ? null : JSON.parse(this.data) as Frame; }
}

class ManualScheduler implements BindingScheduler {
  elapsed = 0;
  private nextId = 0;
  private readonly jobs = new Map<number, { at: number; run: () => void }>();
  setTimeout(run: () => void, ms: number): unknown {
    const id = ++this.nextId;
    this.jobs.set(id, { at: this.elapsed + ms, run });
    return id;
  }
  clearTimeout(id: unknown): void { this.jobs.delete(id as number); }
  delays(): number[] { return [...this.jobs.values()].map((job) => job.at - this.elapsed).sort((a, b) => a - b); }
  advance(ms: number): void {
    this.elapsed += ms;
    const due = [...this.jobs.entries()].filter(([, job]) => job.at <= this.elapsed)
      .sort((a, b) => a[1].at - b[1].at);
    for (const [id, job] of due) if (this.jobs.delete(id)) job.run();
  }
}

class MemoryConnection implements IpcConnection {
  closed = false;
  hello: Frame | null = null;
  decision: Frame | null = null;
  readonly sent: Frame[] = [];
  private readonly incoming: string[] = [];
  private waiter: { resolve: (line: string) => void; reject: (error: Error) => void } | null = null;
  constructor(
    private readonly onSend: (frame: Frame, conn: MemoryConnection) => void,
    private readonly keyPair: Keys,
  ) {}
  event(event: BindingEvent, bindingId = ID): Frame {
    if (!this.hello || !this.decision) throw new Error("memory peer has not sent its signed decision");
    // 固定独立合同；必须使用本连接实际发出的首帧，迟到事件不能换绑到后继连接。
    const message = Buffer.from(JSON.stringify({
      purpose: "tuvis.dsh.binding-event.v2",
      ...JSON.parse(challengeBytes(this.hello, String(this.decision["publicKeySpki"])).toString("utf8")) as Frame,
      decisionSignature: this.decision["decisionSignature"],
      event,
      bindingId,
    }), "utf8");
    return {
      type: event === "revocation" ? "revoked" : "granted",
      binding_id: bindingId,
      event,
      eventSignature: sign(null, message, this.keyPair.privateKey).toString("base64url"),
    };
  }
  get waitingForFrame(): boolean { return this.waiter !== null; }
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
    if (this.waiter) throw new Error("concurrent memory recv");
    return new Promise((resolve, reject) => { this.waiter = { resolve, reject }; });
  }
  push(frame: Frame): void {
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

function harness(kind: Kind = "BindingSession", config: {
  state?: "pending_approval" | "authorized";
  storage?: BindingStorage;
  rv?: Rendezvous;
  reader?: RendezvousReader;
  homeRoot?: string;
  keyPair?: Keys;
  firstFrame?: Frame;
  mutateHello?: (reply: Frame) => void;
  ackId?: string;
  holdAck?: boolean;
} = {}) {
  const rv = config.rv ?? rendezvous(config.keyPair);
  const storage = config.storage ?? new MemoryStorage();
  const scheduler = new ManualScheduler();
  const connections: MemoryConnection[] = [];
  const exchanged: string[] = [];
  const options = {
    storage,
    scheduler,
    rendezvous: config.reader ?? { async read() { return rv; } },
    homeBinding: (selected: Rendezvous): HomeBinding => ({
      homeRoot: config.homeRoot ?? "/rereview-memory-only/.dsh",
      hostPid: selected.processIdentity.pid,
      hostStartedAtMs: selected.processIdentity.startedAtMs,
    }),
    clock: () => NOW + scheduler.elapsed,
    exchange: { async exchange() { exchanged.push(COOKIE); return COOKIE; } },
    transport: {
      async connect() {
        const attempt = connections.length;
        const conn = new MemoryConnection((frame, socket) => {
          if (frame["type"] === "hello") {
            const clientKey = createPublicKey({
              key: Buffer.from(String(frame["publicKeySpki"]), "base64url"),
              type: "spki", format: "der",
            });
            if (!verify(null, challengeBytes(frame, rv.publicKeySpki), clientKey,
              Buffer.from(String(frame["signature"]), "base64url"))) {
              throw new Error("independent peer rejected client signature");
            }
            if (attempt === 0 && config.firstFrame) {
              socket.push(config.firstFrame); // 首帧反例不伪造签名，也不先发 server_hello。
              return;
            }
            const reply = signedReply(frame, rv, config.keyPair ?? keys, config.state ?? "authorized");
            config.mutateHello?.(reply);
            socket.hello = frame;
            socket.decision = reply;
            socket.push(reply);
          } else if (frame["type"] === "credential") {
            if (frame["bindingId"] !== ID) {
              socket.push({ type: "error", error: "DSH_AUTH_UNKNOWN_BINDING", detail: "memory binding mismatch" });
            } else if (!config.holdAck) {
              // 错 binding_id 反例也真实签名，不能靠缺少证明提前拒绝来误过。
              socket.push(socket.event("credential_ack", config.ackId ?? ID));
            }
          } else throw new Error("unexpected memory client frame");
        }, config.keyPair ?? keys);
        connections.push(conn);
        return conn;
      },
    },
  };
  const peer = kind === "BindingClient" ? new BindingClient(options) : new BindingSession(options);
  return {
    peer, rv, connections, scheduler, exchanged,
    credentials: () => connections.flatMap((conn) => conn.sent).filter((frame) => frame["type"] === "credential"),
    async run() {
      if (peer instanceof BindingClient) return await peer.runOnce();
      peer.start();
      await settle();
      return undefined;
    },
    async push(frame: Frame) { connections.at(-1)!.push(frame); await settle(); },
    async approve() {
      const conn = connections.at(-1)!;
      conn.push(conn.event("approval"));
      await settle();
    },
    async acknowledge() {
      const conn = connections.at(-1)!;
      conn.push(conn.event("credential_ack"));
      await settle();
    },
    async dispose() {
      if (peer instanceof BindingSession) peer.dispose();
      for (const conn of connections) conn.close();
      await settle();
    },
  };
}

function writeJson(file: string, value: unknown): void {
  mkdirSync(dirname(file), { recursive: true });
  writeFileSync(file, JSON.stringify(value), { mode: 0o600 });
}
function readJson(file: string): Frame { return JSON.parse(readFileSync(file, "utf8")) as Frame; }
function freshIdentity(anchor: string | null): string {
  const clientKeys = generateKeyPairSync("ed25519");
  return JSON.stringify({
    version: 1,
    privateKeyPem: clientKeys.privateKey.export({ type: "pkcs8", format: "pem" }),
    publicKeySpki: publicSpki(clientKeys),
    trustAnchorSpki: anchor,
    revoked: false,
    revokedBindingId: null,
  });
}

// 两例集成测试保留生产域选择与文件存储，精确暂停在旧连接的 credential ACK 等待点。
describe("v2 rereview: pending ACK cannot cross legacy/instance storage domains", () => {
  it.each(["migrated empty slot", "existing instance identity"] as const)(
    "preserves %s when legacy selection changes before the credential ACK",
    async (slot) => {
      const root = mkdtempSync(join(tmpdir(), "tuvis-v2-domain-ack-"));
      const home = join(root, "dsh-home");
      const state = join(root, "tuvis-state");
      const instanceId = "b".repeat(32);
      const legacyFile = join(state, "rendezvous.json");
      const legacyIdentity = join(home, "profiles", "desktop", "tuvis-remote", "identity.json");
      const instanceIdentity = join(home, "tuvis-remote", instanceId, "identity.json");
      const legacyRv = rendezvous();
      const nextRv = rendezvous(instanceKeys, instanceId);
      writeJson(legacyFile, legacyRv);
      const storage = instanceFileStorage(home, state);
      const reader = instanceRendezvousReader(state);
      const old = harness("BindingSession", {
        rv: legacyRv, reader, storage, homeRoot: home, state: "pending_approval", holdAck: true,
      });
      let next: ReturnType<typeof harness> | undefined;
      try {
        await old.run();
        expect(old.peer.state).toBe("pending");
        expect(old.exchanged).toEqual([]);
        expect(readJson(legacyIdentity)["trustAnchorSpki"]).toBeNull();
        await old.approve();
        expect(old.credentials()).toEqual([{ type: "credential", bindingId: ID, cookie: COOKIE }]);
        expect(old.peer.state).toBe("pending");
        expect(old.connections[0]!.waitingForFrame).toBe(true); // 已发 credential，尚未回 ACK。

        writeJson(legacyFile, { ...legacyRv, retired: true });
        writeJson(join(state, "instances", instanceId, "rendezvous.json"), nextRv);
        expect(await reader.read()).toEqual(nextRv);
        if (slot === "existing instance identity") {
          await storage.write(freshIdentity(nextRv.publicKeySpki));
        } else {
          await storage.read(); // 执行真实显式迁移：同客户端钥、清空域关系。
        }
        const newDomainBeforeAck = readFileSync(instanceIdentity, "utf8");
        expect(readJson(instanceIdentity)["trustAnchorSpki"]).toBe(
          slot === "existing instance identity" ? nextRv.publicKeySpki : null,
        );

        await old.acknowledge(); // 保留旧连接挑战与 decision，旧域已签 ACK 此刻才到达。
        expect.soft(readJson(instanceIdentity)["trustAnchorSpki"], "old server key must not enter instance domain")
          .not.toBe(legacyRv.publicKeySpki);
        expect.soft(digest(readFileSync(instanceIdentity)), "old ACK must not overwrite the selected instance identity")
          .toBe(digest(Buffer.from(newDomainBeforeAck)));
        await old.dispose();

        next = harness("BindingSession", { rv: nextRv, keyPair: instanceKeys, reader, storage, homeRoot: home });
        await next.run();
        expect.soft(next.connections, "the genuine instance key must remain usable").toHaveLength(1);
        expect(next.peer.snapshot()).toEqual({ state: "bound", bindingId: ID, displayCode: null });
      } finally {
        await old.dispose();
        await next?.dispose();
        rmSync(root, { recursive: true, force: true });
      }
    },
  );
});

const unsignedRevocations = [
  { label: "revoked", frame: { type: "revoked", binding_id: ID } },
  { label: "PEER_REVOKED", frame: { type: "error", error: "DSH_PEER_REVOKED", detail: "unsigned first frame" } },
];
for (const kind of ["BindingClient", "BindingSession"] as const) {
  describe(`v2 rereview: ${kind} pinned identity requires an authenticated peer before tombstones`, () => {
    it.each(unsignedRevocations)("does not persist a permanent tombstone from unsigned first-frame $label", async ({ frame }) => {
      const storage = new MemoryStorage();
      const initial = harness(kind, { storage });
      try {
        await initial.run();
        expect(initial.peer.state).toBe("bound");
        expect(storage.saved()?.["trustAnchorSpki"]).toBe(publicSpki(keys));
      } finally { await initial.dispose(); }
      const before = storage.data;
      const attempt = harness(kind, { storage, firstFrame: frame });
      try {
        await attempt.run();
        expect(attempt.connections).toHaveLength(1);
        expect(attempt.connections[0]!.sent.map((f) => f["type"])).toEqual(["hello"]);
        expect(attempt.exchanged).toEqual([]);
        expect.soft(attempt.peer.state).not.toBe("revoked");
        expect.soft(storage.saved()).toMatchObject({ revoked: false, revokedBindingId: null });
        expect.soft(digest(Buffer.from(storage.data!)), "an unsigned first frame cannot mutate pinned identity")
          .toBe(digest(Buffer.from(before!)));
        expect(attempt.connections[0]!.closed).toBe(true);
        if (kind === "BindingSession") {
          expect.soft(attempt.scheduler.delays()).toEqual([1_000]);
          attempt.scheduler.advance(1_000);
          await settle();
          expect.soft(attempt.connections).toHaveLength(2);
          expect(attempt.peer.state).toBe("bound");
        }
      } finally { await attempt.dispose(); }
    });
  });
}

for (const kind of ["BindingClient", "BindingSession"] as const) {
  describe(`v2 rereview: ${kind} unknown server frame types`, () => {
    it.each(["server_hell0", "future_server_hello"])("rejects %s even with a valid signed server_hello payload", async (type) => {
      const storage = new MemoryStorage();
      const h = harness(kind, { storage, mutateHello: (reply) => { reply["type"] = type; } });
      try {
        const outcome = await h.run();
        expect(h.connections).toHaveLength(1);
        expect.soft(h.exchanged).toEqual([]);
        expect.soft(h.credentials()).toEqual([]);
        expect.soft(storage.saved()?.["trustAnchorSpki"]).toBeNull();
        expect.soft(h.peer.state).toBe("idle");
        expect.soft(h.connections[0]!.closed).toBe(true);
        if (kind === "BindingClient") expect(outcome).toMatchObject({ state: "refused" });
        else expect(h.scheduler.delays()).toEqual([1_000]);
      } finally { await h.dispose(); }
    });

    it("rejects a bare unknown frame without attempting credential exchange as a control", async () => {
      const h = harness(kind, { firstFrame: { type: "unknown_future_frame" } });
      try {
        await h.run();
        expect(h.exchanged).toEqual([]);
        expect(h.credentials()).toEqual([]);
        expect(h.peer.state).toBe("idle");
        expect(h.connections[0]!.closed).toBe(true);
      } finally { await h.dispose(); }
    });
  });
}

const deliveryPaths = [
  { kind: "BindingClient", state: "authorized" },
  { kind: "BindingSession", state: "authorized" },
  { kind: "BindingSession", state: "pending_approval" },
] as const;
for (const { kind, state } of deliveryPaths) {
  describe(`v2 rereview: ${kind} ${state} credential ACK`, () => {
    it("does not become bound when Rust ACK binding_id identifies a different binding", async () => {
      const storage = new MemoryStorage();
      const h = harness(kind, { storage, state, ackId: OTHER_ID });
      try {
        const outcome = await h.run();
        if (state === "pending_approval") {
          expect(h.peer.state).toBe("pending");
          await h.approve();
        }
        expect(h.credentials()).toEqual([{ type: "credential", bindingId: ID, cookie: COOKIE }]);
        expect.soft(h.peer.state).not.toBe("bound");
        expect.soft(storage.saved()?.["trustAnchorSpki"]).toBeNull();
        expect.soft(h.connections[0]!.closed).toBe(true);
        if (kind === "BindingSession") expect(h.scheduler.delays()).toEqual([1_000]);
        else expect(outcome).toMatchObject({ state: "refused" });
      } finally { await h.dispose(); }
    });

    it("becomes bound when Rust ACK binding_id matches the delivered credential as a control", async () => {
      const storage = new MemoryStorage();
      const h = harness(kind, { storage, state });
      try {
        await h.run();
        if (state === "pending_approval") await h.approve();
        expect(h.peer.snapshot()).toEqual({ state: "bound", bindingId: ID, displayCode: null });
        expect(h.credentials()).toEqual([{ type: "credential", bindingId: ID, cookie: COOKIE }]);
        expect(storage.saved()?.["trustAnchorSpki"]).toBe(publicSpki(keys));
        expect(h.scheduler.delays()).toEqual([]);
      } finally { await h.dispose(); }
    });
  });
}

function isOpenSslError(error: unknown): boolean {
  if (!(error instanceof Error)) return false;
  const details = error as Error & { code?: unknown; opensslErrorStack?: unknown };
  return (typeof details.code === "string" && /^ERR_OSSL_/.test(details.code))
    || (Array.isArray(details.opensslErrorStack) && details.opensslErrorStack.some(
      (entry: unknown) => typeof entry === "string" && /^error:[\da-f]+:[^:]+:/i.test(entry),
    ));
}

describe("v2 rereview: OpenSSL error identification", () => {
  it.each([
    [Object.assign(new Error("decoder"), { code: "ERR_OSSL_UNSUPPORTED" }), true],
    [Object.assign(new Error("key"), { opensslErrorStack: ["error:06800066:asn1 encoding routines::bad object header"] }), true],
    [new Error("unrelated"), false],
    [Object.assign(new Error("unrelated"), { opensslErrorStack: ["ASN.1 error"] }), false],
    [{ code: "ERR_OSSL_UNSUPPORTED" }, false],
  ])("identifies an OpenSSL error only from error evidence %#", (error, expected) => {
    expect(isOpenSslError(error)).toBe(expected);
  });
});

describe("v2 rereview: real exceptions reaching lifecycle cleanup", () => {
  const invalidDerSpki = Buffer.from("invalid DER for actual node createPublicKey exception").toString("base64url");

  it("proves invalid rendezvous DER actually throws inside the single-shot crypto path", async () => {
    const h = harness("BindingClient", { rv: { ...rendezvous(), publicKeySpki: invalidDerSpki } });
    try {
      // reply 的 signature 是字符串且所有回显都匹配，不能被 matchesServerHello 提前挡下。
      await expect(h.run()).rejects.toSatisfy(isOpenSslError);
      expect(h.connections[0]!.sent.map((frame) => frame["type"])).toEqual(["hello"]);
      expect(h.connections[0]!.closed).toBe(true);
      expect(h.exchanged).toEqual([]);
    } finally { await h.dispose(); }
  });

  it("closes and retries after actual createPublicKey throws beyond reply validation", async () => {
    const rv = { ...rendezvous(), publicKeySpki: invalidDerSpki };
    const h = harness("BindingSession", { rv });
    try {
      await h.run();
      expect(h.connections).toHaveLength(1);
      expect(h.connections[0]!.closed).toBe(true);
      expect(h.exchanged).toEqual([]);
      expect(h.scheduler.delays()).toEqual([1_000]);
      rv.publicKeySpki = publicSpki(keys);
      h.scheduler.advance(1_000);
      await settle();
      expect(h.connections).toHaveLength(2);
      expect(h.peer.state).toBe("bound");
      expect(h.credentials()).toEqual([{ type: "credential", bindingId: ID, cookie: COOKIE }]);
      expect(h.scheduler.delays()).toEqual([]);
    } finally { await h.dispose(); }
  });

  it.each(["authorized", "pending_approval"] as const)(
    "clears every old connection timer when %s anchor persistence throws into cycle catch",
    async (state) => {
      const backing = new MemoryStorage();
      const thrownWrites: Error[] = [];
      const fault = new Error("test boundary: first anchor persistence failed");
      const storage: BindingStorage = {
        read: () => backing.read(),
        async write(data) {
          if ((JSON.parse(data) as Frame)["trustAnchorSpki"] && thrownWrites.length === 0) {
            thrownWrites.push(fault);
            throw fault; // 在生产 await storage.write 处实际 reject，外层 cycle catch 必须收场。
          }
          await backing.write(data);
        },
      };
      const h = harness("BindingSession", { state, storage });
      try {
        await h.run();
        if (state === "pending_approval") {
          expect(h.scheduler.delays()).toEqual([60_000]);
          await h.approve();
        }
        expect(thrownWrites).toEqual([fault]); // 阶段哨兵：确实越过验签与 ACK，触发异常。
        expect(h.credentials()).toEqual([{ type: "credential", bindingId: ID, cookie: COOKIE }]);
        expect(h.connections[0]!.closed).toBe(true);
        expect(h.peer.state).not.toBe("bound");
        expect(backing.saved()?.["trustAnchorSpki"]).toBeNull();
        expect.soft(h.scheduler.delays(), "catch must clear pending expiry as well as release the socket")
          .toEqual([1_000]);
        h.scheduler.advance(1_000);
        await settle();
        expect(h.connections).toHaveLength(2);
        if (state === "pending_approval") await h.approve();
        expect(h.peer.state).toBe("bound");
        expect(h.scheduler.delays()).toEqual([]);
        h.peer instanceof BindingSession && h.peer.dispose();
        expect(h.connections.every((conn) => conn.closed)).toBe(true);
        expect(h.scheduler.delays()).toEqual([]);
      } finally { await h.dispose(); }
    },
  );
});
