/**
 * scope / rendezvous 交接的确定性交错测试。
 *
 * 身份文件、scope、迁移和 rendezvous 选择全部使用生产实现；包装器只提供暂停点，
 * 不改写任何读值、写值、目标路径或异常。仅 IPC、兑换和时间为外部边界假件。
 * Ed25519 与双方挑战验签均真实；签名字节按 v2 合同独立编码。
 * 每例使用独立临时目录，不修改 HOME、真实身份文件或其他测试。
 */
import { sha256Hex as hash } from "./wire-fixtures";
import {
  createPublicKey, generateKeyPairSync, sign, verify, type KeyObject,
} from "node:crypto";
import { existsSync, mkdtempSync, mkdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { setImmediate as settle } from "node:timers/promises";
import { describe, expect, it } from "vitest";
import {
  BindingSession, type BindingScheduler, type BindingStorage, type HomeBinding,
  type IpcConnection, type Rendezvous, type RendezvousReader,
} from "../src/auth.js";
import { instanceFileStorage, instanceRendezvousReader } from "../src/index.js";

type Frame = Record<string, unknown>;
type BindingEvent = "approval" | "revocation" | "credential_ack";
type Keys = { publicKey: KeyObject; privateKey: KeyObject };
type OldPolicy = "revoked" | "pinned" | "unpinned";
type InstanceSlot = "empty" | "independent identity";
const NOW = 1_760_000_000_000;
const OLD_ID = "scope-old-binding";
const NEW_ID = "scope-new-binding";
const COOKIE = "scope-memory-only-cookie";
const INSTANCE_ID = "c".repeat(32);
const oldServerKeys = generateKeyPairSync("ed25519");
const newServerKeys = generateKeyPairSync("ed25519");
const spki = (pair: Keys): string => pair.publicKey.export({ type: "spki", format: "der" }).toString("base64url");

/** 只运行一次的显式栅栏；阶段等待不依赖睡眠或固定微任务轮数。 */
class Barrier {
  private enter!: () => void;
  private leave!: () => void;
  private used = false;
  readonly reached = new Promise<void>((resolve) => { this.enter = resolve; });
  private readonly released = new Promise<void>((resolve) => { this.leave = resolve; });
  async pause(): Promise<void> {
    if (this.used) return;
    this.used = true;
    this.enter();
    await this.released;
  }
  release(): void { this.leave(); }
}

class Scheduler implements BindingScheduler {
  elapsed = 0;
  private id = 0;
  private readonly jobs = new Map<number, { at: number; run: () => void }>();
  setTimeout(run: () => void, ms: number): unknown {
    const id = ++this.id;
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

class Connection implements IpcConnection {
  closed = false;
  hello: Frame | null = null;
  decision: Frame | null = null;
  readonly sent: Frame[] = [];
  private readonly incoming: string[] = [];
  private waiter: { resolve: (line: string) => void; reject: (e: Error) => void } | null = null;
  constructor(
    readonly record: Rendezvous,
    readonly bindingId: string,
    private readonly keyPair: Keys,
    private readonly onSend: (frame: Frame, socket: Connection) => void,
  ) {}
  event(event: BindingEvent): Frame {
    if (!this.hello || !this.decision) throw new Error("memory peer has not sent its signed decision");
    // 独立编码完整事件合同；按旧连接签迟到撤权/ACK，不从当前域重新取挑战。
    const message = Buffer.from(JSON.stringify({
      purpose: "tuvis.dsh.binding-event.v2",
      ...JSON.parse(challengeBytes(this.hello, String(this.decision["publicKeySpki"])).toString("utf8")) as Frame,
      decisionSignature: this.decision["decisionSignature"],
      event,
      bindingId: this.bindingId,
    }), "utf8");
    return {
      type: event === "revocation" ? "revoked" : "granted",
      binding_id: this.bindingId,
      event,
      eventSignature: sign(null, message, this.keyPair.privateKey).toString("base64url"),
    };
  }
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
    const waiter = this.waiter;
    this.waiter = null;
    const line = JSON.stringify(frame);
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

function challengeBytes(hello: Frame, serverSpki: string): Buffer {
  const home = hello["homeBinding"] as HomeBinding;
  return Buffer.from(JSON.stringify({
    bindingProtocol: 2,
    keyDigests: [hash(Buffer.from(serverSpki, "base64url")), hash(Buffer.from(String(hello["publicKeySpki"]), "base64url"))],
    nonce: hello["nonce"], hostId: hello["hostId"], epoch: hello["epoch"], expiry: hello["expiry"],
    homeBinding: { homeRoot: home.homeRoot, hostPid: home.hostPid, hostStartedAtMs: home.hostStartedAtMs },
  }), "utf8");
}
function signedHello(hello: Frame, record: Rendezvous, pair: Keys, state: string, bindingId: string): Frame {
  const challenge = challengeBytes(hello, record.publicKeySpki);
  const reply: Frame = {
    type: "server_hello", bindingProtocol: 2, publicKeySpki: record.publicKeySpki,
    nonce: hello["nonce"], hostId: hello["hostId"], epoch: hello["epoch"], expiry: hello["expiry"],
    homeBinding: { ...(hello["homeBinding"] as HomeBinding) },
    signature: sign(null, challenge, pair.privateKey).toString("base64url"),
    state, bindingId, displayCode: state === "pending_approval" ? "816234" : null,
    pendingExpiresAt: state === "pending_approval" ? hello["expiry"] : null,
  };
  // 独立固定合同：purpose → 七字段挑战 → state/bindingId/displayCode/pendingExpiresAt。
  const decision = Buffer.from(JSON.stringify({
    purpose: "tuvis.dsh.binding-decision.v2",
    ...JSON.parse(challenge.toString("utf8")) as Frame,
    state, bindingId, displayCode: reply["displayCode"], pendingExpiresAt: reply["pendingExpiresAt"],
  }), "utf8");
  reply["decisionSignature"] = sign(null, decision, pair.privateKey).toString("base64url");
  return reply;
}
function writeJson(file: string, value: unknown): void {
  mkdirSync(dirname(file), { recursive: true });
  writeFileSync(file, JSON.stringify(value), { mode: 0o600 });
}
function readIdentity(file: string): Frame | null {
  return existsSync(file) ? JSON.parse(readFileSync(file, "utf8")) as Frame : null;
}
function fileHash(file: string): string | null { return existsSync(file) ? hash(readFileSync(file)) : null; }
function identity(policy: OldPolicy, serverSpki: string, bindingId = OLD_ID): Frame {
  const client = generateKeyPairSync("ed25519");
  return {
    version: 1,
    privateKeyPem: client.privateKey.export({ type: "pkcs8", format: "pem" }),
    publicKeySpki: spki(client),
    trustAnchorSpki: policy === "unpinned" ? null : serverSpki,
    revoked: policy === "revoked",
    revokedBindingId: policy === "revoked" ? bindingId : null,
  };
}

async function topology(policy: OldPolicy, instancePolicy: OldPolicy = "pinned") {
  const root = mkdtempSync(join(tmpdir(), "tuvis-v2-scope-order-"));
  const home = join(root, "dsh-home");
  const state = join(root, "tuvis-state");
  const oldRecord: Rendezvous = {
    bindingProtocol: 2, address: "memory://legacy-scope", hostId: "scope-legacy-host",
    epoch: "18446744073709551614",
    processIdentity: { platform: process.platform, pid: process.pid, startedAtMs: NOW - 10_000, executable: "memory-dsh-host" },
    publicKeySpki: spki(oldServerKeys),
  };
  const nextRecord: Rendezvous = {
    ...oldRecord, address: "memory://instance-scope", hostId: INSTANCE_ID,
    publicKeySpki: spki(newServerKeys),
  };
  const legacyRvFile = join(state, "rendezvous.json");
  const legacyIdentityFile = join(home, "profiles", "desktop", "tuvis-remote", "identity.json");
  const instanceIdentityFile = join(home, "tuvis-remote", INSTANCE_ID, "identity.json");
  writeJson(legacyRvFile, oldRecord);
  const storage = instanceFileStorage(home, state);
  const reader = instanceRendezvousReader(state);
  const legacyIdentity = identity(policy, oldRecord.publicKeySpki);
  const instanceIdentity = identity(instancePolicy, nextRecord.publicKeySpki, NEW_ID);
  await storage.write(JSON.stringify(legacyIdentity));
  return {
    root, home, storage, reader, oldRecord, nextRecord, legacyIdentity, instanceIdentity,
    legacyIdentityFile, instanceIdentityFile,
    async handover(slot: InstanceSlot = "empty") {
      writeJson(legacyRvFile, { ...oldRecord, retired: true });
      writeJson(join(state, "instances", INSTANCE_ID, "rendezvous.json"), nextRecord);
      if (slot === "independent identity") await storage.write(JSON.stringify(instanceIdentity));
      // 直接调用同一生产 reader，证明文件交接完成且选择唯一，不给会话伪造返回值。
      expect(await reader.read()).toEqual(nextRecord);
    },
    expectedInstanceClient(slot: InstanceSlot): unknown {
      return slot === "empty" ? legacyIdentity["publicKeySpki"] : instanceIdentity["publicKeySpki"];
    },
    destroy() { rmSync(root, { recursive: true, force: true }); },
  };
}

type Topology = Awaited<ReturnType<typeof topology>>;
function sessionHarness(t: Topology, config: {
  beforeReader?: Barrier;
  afterReader?: Barrier;
  beforeRevocationWrite?: Barrier;
  legacyReply?: "authorized" | "pending_approval" | "retry";
} = {}) {
  const scheduler = new Scheduler();
  const connections: Connection[] = [];
  const exchanges: string[] = [];
  const observedHosts: Array<string | null> = [];
  const rejectedWrites: string[] = [];
  const trace: string[] = [];
  const consistencyChecks: Array<{ hostId: string; matches: boolean }> = [];

  // 每次 scope 都先调用真实生产实现。返回对象只转发并可暂停 write，绝不换路由。
  function instrument(actual: BindingStorage): BindingStorage {
    const matchesRendezvous = actual.matchesRendezvous?.bind(actual);
    return {
      ...(matchesRendezvous ? {
        matchesRendezvous(record: Rendezvous) {
          const matches = matchesRendezvous(record);
          consistencyChecks.push({ hostId: record.hostId, matches });
          return matches;
        },
      } : {}),
      read: () => actual.read(),
      async write(data) {
        if ((JSON.parse(data) as Frame)["revoked"] === true && config.beforeRevocationWrite) {
          trace.push("before-revocation-write");
          await config.beforeRevocationWrite.pause();
        }
        try { await actual.write(data); }
        catch (error) {
          rejectedWrites.push(String(error));
          throw error; // 保留生产异常，让真实 cycle catch 决定收场和重试。
        }
      },
      scope() {
        const scoped = actual.scope?.() ?? actual;
        trace.push("scope-captured");
        return instrument(scoped);
      },
    };
  }
  const reader: RendezvousReader = {
    async read() {
      if (config.beforeReader) await config.beforeReader.pause();
      trace.push("read-rendezvous");
      const record = await t.reader.read();
      observedHosts.push(record?.hostId ?? null);
      if (config.afterReader) await config.afterReader.pause();
      return record;
    },
  };
  const peer = new BindingSession({
    storage: instrument(t.storage), rendezvous: reader, scheduler,
    homeBinding: (record) => ({ homeRoot: t.home, hostPid: record.processIdentity.pid, hostStartedAtMs: record.processIdentity.startedAtMs }),
    clock: () => NOW + scheduler.elapsed,
    exchange: { async exchange() { exchanges.push(COOKIE); return COOKIE; } },
    transport: {
      async connect(address) {
        const record = address === t.oldRecord.address ? t.oldRecord : address === t.nextRecord.address ? t.nextRecord : null;
        if (!record) throw new Error("unexpected memory address");
        const isLegacy = record.hostId === t.oldRecord.hostId;
        const pair = isLegacy ? oldServerKeys : newServerKeys;
        const bindingId = isLegacy ? OLD_ID : NEW_ID;
        const conn = new Connection(record, bindingId, pair, (frame, socket) => {
          if (frame["type"] === "hello") {
            const publicKey = createPublicKey({ key: Buffer.from(String(frame["publicKeySpki"]), "base64url"), type: "spki", format: "der" });
            if (!verify(null, challengeBytes(frame, record.publicKeySpki), publicKey, Buffer.from(String(frame["signature"]), "base64url"))) {
              throw new Error("independent peer rejected client challenge signature");
            }
            if (frame["hostId"] !== record.hostId) throw new Error("hello host does not match connected domain");
            if (isLegacy && config.legacyReply === "retry") {
              socket.push({ type: "error", error: "DSH_AUTH_NOT_APPROVED", detail: "memory retry boundary" });
            } else {
              socket.hello = frame;
              socket.decision = signedHello(frame, record, pair, isLegacy ? config.legacyReply ?? "pending_approval" : "pending_approval", bindingId);
              socket.push(socket.decision);
            }
          } else if (frame["type"] === "credential") {
            socket.push(frame["bindingId"] === bindingId
              ? socket.event("credential_ack")
              : { type: "error", error: "DSH_AUTH_UNKNOWN_BINDING", detail: "wrong memory domain binding" });
          } else throw new Error("unexpected memory client frame");
        });
        connections.push(conn);
        return conn;
      },
    },
  });
  return {
    peer, scheduler, connections, exchanges, observedHosts, rejectedWrites, trace, consistencyChecks,
    latest: () => connections.at(-1),
    credentials: () => connections.flatMap((conn) => conn.sent).filter((frame) => frame["type"] === "credential"),
    async start() { peer.start(); await settle(); },
    async approveLatest() {
      const conn = connections.at(-1);
      if (conn && !conn.closed) conn.push(conn.event("approval"));
      await settle();
    },
    async revokeLatest(notification: "revoked" | "PEER_REVOKED") {
      let conn = connections.at(-1)!;
      if (notification === "PEER_REVOKED") {
        // 保留未签 error 的拒绝反例：它不能再被当成永久撤权写入。
        // 真实会话重试认证后，再由有效 revocation 证明进入原有撤权写入交错。
        const before = fileHash(t.legacyIdentityFile);
        const exchanged = exchanges.length;
        const attempts = connections.length;
        conn.push({ type: "error", error: "DSH_PEER_REVOKED", detail: "unproved memory peer revocation" });
        await settle();
        expect(peer.state).not.toBe("revoked");
        expect(fileHash(t.legacyIdentityFile)).toBe(before);
        expect(exchanges).toHaveLength(exchanged);
        expect(conn.closed).toBe(true);
        expect(scheduler.delays()).toEqual([1_000]);
        scheduler.advance(1_000);
        await settle();
        expect(connections).toHaveLength(attempts + 1);
        conn = connections.at(-1)!;
      }
      conn.push(conn.event("revocation"));
      await settle();
      return conn;
    },
    async dispose() {
      config.beforeReader?.release();
      config.afterReader?.release();
      config.beforeRevocationWrite?.release();
      peer.dispose();
      connections.forEach((conn) => conn.close());
      await settle();
    },
  };
}

const startInterleavings = (["revoked", "pinned", "unpinned"] as const).flatMap((policy) =>
  (["empty", "independent identity"] as const).map((slot) => ({ policy, slot })),
);
describe("v2 scope interleaving: handover before actual rendezvous read", () => {
  it.each(startInterleavings)("recovers the same session from old $policy policy into an instance with $slot", async ({ policy, slot }) => {
    const t = await topology(policy);
    const barrier = new Barrier();
    const h = sessionHarness(t, { beforeReader: barrier });
    const oldHash = fileHash(t.legacyIdentityFile);
    // A01/A02 迁移守卫：已撤钥（revoked）+ 空实例槽 → 拒绝迁移、走全新生成路径
    // ——预期客户端钥不再是 legacy 钥，而是运行期写入实例槽的全新钥。
    const expectedClientKey = (): unknown =>
      policy === "revoked" && slot === "empty"
        ? readIdentity(t.instanceIdentityFile)?.["publicKeySpki"]
        : t.expectedInstanceClient(slot);
    try {
      h.peer.start();
      await barrier.reached;
      // 当前实现此时已固定 legacy scope；只观察调用，不把此顺序写成必须遵循的合同。
      // 将来若改为先读 rendezvous，再固定对应 scope，本例仍检验相同的用户可见行为。
      await t.handover(slot);
      const newHash = fileHash(t.instanceIdentityFile);
      barrier.release();
      await settle();
      expect(h.observedHosts[0]).toBe(t.nextRecord.hostId);
      expect.soft(h.peer.state, "old-domain tombstone must not permanently revoke the selected instance")
        .not.toBe("revoked");
      expect(h.exchanges).toEqual([]);
      const first = h.latest();
      if (first && !first.closed) {
        expect.soft(first.sent.find((frame) => frame["type"] === "hello")?.["publicKeySpki"],
          "the selected instance must use its own identity, not another scoped file")
          .toBe(expectedClientKey());
      }
      expect(fileHash(t.legacyIdentityFile)).toBe(oldHash);
      if (slot === "independent identity") {
        expect(fileHash(t.instanceIdentityFile)).toBe(newHash);
      } else if (policy === "revoked") {
        // 已撤钥不得迁入：实例槽 = 全新生成的钥（≠ 旧域已撤钥），域关系干净。
        const fresh = readIdentity(t.instanceIdentityFile);
        expect(fresh?.["publicKeySpki"]).toBeTruthy();
        expect(fresh?.["publicKeySpki"]).not.toBe(t.legacyIdentity["publicKeySpki"]);
        expect(fresh).toMatchObject({ trustAnchorSpki: null, revoked: false, revokedBindingId: null });
      } else {
        // 若实现已安全切到新 scope，允许真实迁移创建新槽，但不得带入旧域关系。
        const migrated = readIdentity(t.instanceIdentityFile);
        if (migrated) expect(migrated).toMatchObject({
          publicKeySpki: t.legacyIdentity["publicKeySpki"], trustAnchorSpki: null,
          revoked: false, revokedBindingId: null,
        });
      }

      // 若本轮选择不一致，允许安全中止再重试；若已连上，则模拟掉线让同一对象重取 scope。
      first?.close();
      await settle();
      expect.soft(h.scheduler.delays(), "a cross-domain attempt must leave a retry rather than stop forever")
        .toEqual([1_000]);
      h.scheduler.advance(1_000);
      await settle();
      expect.soft(h.peer.snapshot()).toEqual({ state: "pending", bindingId: NEW_ID, displayCode: "816234" });
      const latest = h.latest();
      expect.soft(latest?.record.hostId).toBe(t.nextRecord.hostId);
      expect.soft(latest?.sent.find((frame) => frame["type"] === "hello")?.["publicKeySpki"])
        .toBe(expectedClientKey());
      await h.approveLatest();
      expect.soft(h.peer.snapshot()).toEqual({ state: "bound", bindingId: NEW_ID, displayCode: null });
      expect.soft(readIdentity(t.instanceIdentityFile)).toMatchObject({
        publicKeySpki: expectedClientKey(), trustAnchorSpki: t.nextRecord.publicKeySpki,
        revoked: false, revokedBindingId: null,
      });
      expect(fileHash(t.legacyIdentityFile)).toBe(oldHash);
    } finally { await h.dispose(); t.destroy(); }
  });
});

const reverseReadScenarios = (["revoked", "pinned", "unpinned"] as const).flatMap((oldPolicy) =>
  (["revoked", "pinned", "unpinned"] as const).map((newPolicy) => ({ oldPolicy, newPolicy })),
);
describe("v2 scope interleaving: handover after actual rendezvous read before scope capture", () => {
  it.each(reverseReadScenarios)("rejects stale old $oldPolicy rendezvous before using new $newPolicy identity", async ({ oldPolicy, newPolicy }) => {
    const t = await topology(oldPolicy, newPolicy);
    const barrier = new Barrier();
    const h = sessionHarness(t, { afterReader: barrier });
    const oldHash = fileHash(t.legacyIdentityFile);
    try {
      h.peer.start();
      await barrier.reached;
      expect(h.observedHosts).toEqual([t.oldRecord.hostId]);
      expect(h.trace).not.toContain("scope-captured"); // 栅栏确实位于实际读取后、scope 获取前。
      await t.handover("independent identity");
      const newHash = fileHash(t.instanceIdentityFile);
      barrier.release();
      await settle();

      // rv 仍是刚才实际读出的旧对象，scope 已来自新域。必须调用真实一致性检查，
      // 不能把新域的墓碑或未钉身份套给旧端点，也不能兑换凭据后再以写失败补救。
      expect(h.consistencyChecks[0]).toEqual({ hostId: t.oldRecord.hostId, matches: false });
      expect(h.peer.snapshot()).toEqual({ state: "idle", bindingId: null, displayCode: null });
      expect(h.connections).toEqual([]);
      expect(h.exchanges).toEqual([]);
      expect(h.credentials()).toEqual([]);
      expect(h.scheduler.delays()).toEqual([1_000]);
      expect(fileHash(t.legacyIdentityFile)).toBe(oldHash);
      expect(fileHash(t.instanceIdentityFile)).toBe(newHash);

      h.scheduler.advance(1_000);
      await settle();
      expect(h.observedHosts).toEqual([t.oldRecord.hostId, t.nextRecord.hostId]);
      expect(h.consistencyChecks.at(-1)).toEqual({ hostId: t.nextRecord.hostId, matches: true });
      if (newPolicy === "revoked") {
        // 只有本轮真正选定新域后，新域自身的撤权才有权停止同一会话。
        expect(h.peer.snapshot()).toEqual({ state: "revoked", bindingId: NEW_ID, displayCode: null });
        expect(h.connections).toEqual([]);
        expect(h.exchanges).toEqual([]);
        expect(h.scheduler.delays()).toEqual([]);
        expect(fileHash(t.instanceIdentityFile)).toBe(newHash);
      } else {
        expect(h.connections.map((conn) => conn.record.hostId)).toEqual([t.nextRecord.hostId]);
        expect(h.latest()?.sent.find((frame) => frame["type"] === "hello")?.["publicKeySpki"])
          .toBe(t.instanceIdentity["publicKeySpki"]);
        expect(h.peer.snapshot()).toEqual({ state: "pending", bindingId: NEW_ID, displayCode: "816234" });
        expect(h.exchanges).toEqual([]);
        await h.approveLatest();
        expect(h.peer.snapshot()).toEqual({ state: "bound", bindingId: NEW_ID, displayCode: null });
        expect(h.credentials()).toEqual([{ type: "credential", bindingId: NEW_ID, cookie: COOKIE }]);
        expect(readIdentity(t.instanceIdentityFile)).toMatchObject({
          publicKeySpki: t.instanceIdentity["publicKeySpki"], trustAnchorSpki: t.nextRecord.publicKeySpki,
          revoked: false, revokedBindingId: null,
        });
      }
      expect(fileHash(t.legacyIdentityFile)).toBe(oldHash);
    } finally { await h.dispose(); t.destroy(); }
  });

  it.each(["revoked", "pinned", "unpinned"] as const)("does not apply old %s policy when the reverse handover migrates an empty instance slot", async (policy) => {
    const t = await topology(policy);
    const barrier = new Barrier();
    const h = sessionHarness(t, { afterReader: barrier });
    const oldHash = fileHash(t.legacyIdentityFile);
    try {
      h.peer.start();
      await barrier.reached;
      expect(h.observedHosts).toEqual([t.oldRecord.hostId]);
      expect(h.trace).not.toContain("scope-captured");
      await t.handover();
      barrier.release();
      await settle();
      expect(h.consistencyChecks[0]).toEqual({ hostId: t.oldRecord.hostId, matches: false });
      expect(h.peer.state).toBe("idle");
      expect(h.connections).toEqual([]);
      expect(h.exchanges).toEqual([]);
      expect(h.scheduler.delays()).toEqual([1_000]);
      if (policy === "revoked") {
        // A01/A02 迁移守卫：已撤钥不得迁入空实例槽——拒绝迁移（read = null），
        // 实例槽不落地任何身份；全新钥只在真正选中新域的重试中生成。
        expect(readIdentity(t.instanceIdentityFile), "refused migration must not write the instance slot").toBeNull();
      } else {
        expect(readIdentity(t.instanceIdentityFile)).toMatchObject({
          publicKeySpki: t.legacyIdentity["publicKeySpki"], trustAnchorSpki: null,
          revoked: false, revokedBindingId: null,
        });
      }
      h.scheduler.advance(1_000);
      await settle();
      expect(h.consistencyChecks.at(-1)).toEqual({ hostId: t.nextRecord.hostId, matches: true });
      expect(h.connections.map((conn) => conn.record.hostId)).toEqual([t.nextRecord.hostId]);
      expect(h.peer.snapshot()).toEqual({ state: "pending", bindingId: NEW_ID, displayCode: "816234" });
      expect(h.exchanges).toEqual([]);
      await h.approveLatest();
      expect(h.peer.snapshot()).toEqual({ state: "bound", bindingId: NEW_ID, displayCode: null });
      const boundIdentity = readIdentity(t.instanceIdentityFile);
      expect(boundIdentity).toMatchObject({
        trustAnchorSpki: t.nextRecord.publicKeySpki,
        revoked: false, revokedBindingId: null,
      });
      if (policy === "revoked") {
        // 全新生成路径：绑定用全新钥（≠ 旧域已撤钥）。
        expect(boundIdentity?.["publicKeySpki"]).toBeTruthy();
        expect(boundIdentity?.["publicKeySpki"]).not.toBe(t.legacyIdentity["publicKeySpki"]);
      } else {
        expect(boundIdentity?.["publicKeySpki"]).toBe(t.legacyIdentity["publicKeySpki"]);
      }
      expect(fileHash(t.legacyIdentityFile)).toBe(oldHash);
    } finally { await h.dispose(); t.destroy(); }
  });
});

describe("v2 scope interleaving: retry reacquires the selected domain on the same session", () => {
  it.each(["pinned", "unpinned"] as const)("retries old %s into a newly selected instance without reconstructing the session", async (policy) => {
    const t = await topology(policy);
    const h = sessionHarness(t, { legacyReply: "retry" });
    const oldHash = fileHash(t.legacyIdentityFile);
    try {
      await h.start();
      expect(h.connections.map((conn) => conn.record.hostId)).toEqual([t.oldRecord.hostId]);
      expect(h.scheduler.delays()).toEqual([1_000]);
      expect(h.exchanges).toEqual([]);
      await t.handover("independent identity");
      h.scheduler.advance(1_000);
      await settle();
      expect(h.connections.map((conn) => conn.record.hostId)).toEqual([t.oldRecord.hostId, t.nextRecord.hostId]);
      expect(h.latest()?.sent.find((frame) => frame["type"] === "hello")?.["publicKeySpki"])
        .toBe(t.instanceIdentity["publicKeySpki"]);
      expect(h.peer.snapshot()).toEqual({ state: "pending", bindingId: NEW_ID, displayCode: "816234" });
      await h.approveLatest();
      expect(h.peer.snapshot()).toEqual({ state: "bound", bindingId: NEW_ID, displayCode: null });
      expect(readIdentity(t.instanceIdentityFile)).toMatchObject({ revoked: false, revokedBindingId: null, trustAnchorSpki: t.nextRecord.publicKeySpki });
      expect(fileHash(t.legacyIdentityFile)).toBe(oldHash);
    } finally { await h.dispose(); t.destroy(); }
  });
});

const writeInterleavings = (["pending_approval", "authorized"] as const).flatMap((state) =>
  (["revoked", "PEER_REVOKED"] as const).map((notification) => ({ state, notification })),
);
describe("v2 scope interleaving: handover between revocation read and actual write", () => {
  it.each(writeInterleavings)("does not tombstone the new domain when old $state receives $notification during handover", async ({ state, notification }) => {
    const t = await topology("pinned");
    const barrier = new Barrier();
    const h = sessionHarness(t, { legacyReply: state, beforeRevocationWrite: barrier });
    try {
      await h.start();
      expect(h.peer.state).toBe(state === "authorized" ? "bound" : "pending");
      // 使用当前连接的真实事件证明；PEER_REVOKED 原始未签 error 仍在假服务中验证拒绝。
      const old = await h.revokeLatest(notification);
      await barrier.reached; // markRevoked 已完成实际旧身份读取，暂停在生产 write 之前。
      await t.handover("independent identity");
      const newHash = fileHash(t.instanceIdentityFile);
      barrier.release();
      await settle();
      expect(fileHash(t.instanceIdentityFile)).toBe(newHash);
      expect(readIdentity(t.instanceIdentityFile)).toMatchObject({ revoked: false, revokedBindingId: null });
      expect(old.closed).toBe(true);
      expect(h.peer.state).not.toBe("revoked");
      expect(h.scheduler.delays()).toEqual([1_000]); // 也断言旧待批计时器被清掉。
      h.scheduler.advance(1_000);
      await settle();
      expect(h.peer.snapshot()).toEqual({ state: "pending", bindingId: NEW_ID, displayCode: "816234" });
      expect(h.latest()?.record.hostId).toBe(t.nextRecord.hostId);
      await h.approveLatest();
      expect(h.peer.snapshot()).toEqual({ state: "bound", bindingId: NEW_ID, displayCode: null });
      expect(readIdentity(t.instanceIdentityFile)).toMatchObject({ revoked: false, revokedBindingId: null, trustAnchorSpki: t.nextRecord.publicKeySpki });
    } finally { await h.dispose(); t.destroy(); }
  });
});

describe("v2 scope interleaving: coherent-domain controls", () => {
  it.each(["revoked", "pinned", "unpinned"] as const)("migrates old %s without its domain policy when handover precedes session start", async (policy) => {
    const t = await topology(policy);
    const oldHash = fileHash(t.legacyIdentityFile);
    await t.handover();
    const h = sessionHarness(t);
    try {
      await h.start();
      expect(h.peer.snapshot()).toEqual({ state: "pending", bindingId: NEW_ID, displayCode: "816234" });
      expect(h.exchanges).toEqual([]);
      const slotIdentity = readIdentity(t.instanceIdentityFile);
      expect(slotIdentity).toMatchObject({ trustAnchorSpki: null, revoked: false, revokedBindingId: null });
      if (policy === "revoked") {
        // A01/A02 迁移守卫：已撤钥拒绝迁入 → 全新生成路径（新钥 ≠ 旧域已撤钥）。
        expect(slotIdentity?.["publicKeySpki"]).toBeTruthy();
        expect(slotIdentity?.["publicKeySpki"]).not.toBe(t.legacyIdentity["publicKeySpki"]);
      } else {
        expect(slotIdentity?.["publicKeySpki"]).toBe(t.legacyIdentity["publicKeySpki"]);
      }
      await h.approveLatest();
      expect(h.peer.snapshot()).toEqual({ state: "bound", bindingId: NEW_ID, displayCode: null });
      expect(readIdentity(t.instanceIdentityFile)?.["trustAnchorSpki"]).toBe(t.nextRecord.publicKeySpki);
      expect(fileHash(t.legacyIdentityFile)).toBe(oldHash);
    } finally { await h.dispose(); t.destroy(); }
  });

  it("honors an actual legacy tombstone when no domain handover occurred", async () => {
    const t = await topology("revoked");
    const h = sessionHarness(t);
    const oldHash = fileHash(t.legacyIdentityFile);
    try {
      await h.start();
      expect(h.peer.snapshot()).toEqual({ state: "revoked", bindingId: OLD_ID, displayCode: null });
      expect(h.connections).toEqual([]);
      expect(h.exchanges).toEqual([]);
      expect(h.scheduler.delays()).toEqual([]);
      expect(fileHash(t.legacyIdentityFile)).toBe(oldHash);
      expect(fileHash(t.instanceIdentityFile)).toBeNull();
    } finally { await h.dispose(); t.destroy(); }
  });
});
