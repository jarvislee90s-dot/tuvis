/**
 * 独立行为复现：真实 Node IPC 的中文 UTF-8 分块，以及健康绑定的 legacy → 实例域交接。
 *
 * v2 挑战和决策字节独立编码，双方 Ed25519 签名真实；不调用生产签名编码器。
 * socket/身份文件只在独立临时目录，兑换凭据只在内存，不改 HOME 或任何真实数据。
 */
import { sha256Hex as hash } from "./wire-fixtures";
import {
  createPublicKey, generateKeyPairSync, randomUUID, sign, verify, type KeyObject,
} from "node:crypto";
import { channel } from "node:diagnostics_channel";
import { once } from "node:events";
import { existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { createServer, type Socket } from "node:net";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { StringDecoder } from "node:string_decoder";
import { setImmediate as settle } from "node:timers/promises";
import { describe, expect, it } from "vitest";
import {
  BindingClient, BindingSession, type BindingScheduler, type BindingStorage,
  type HomeBinding, type IpcConnection, type Rendezvous,
} from "../src/auth.js";
import { instanceFileStorage, instanceRendezvousReader } from "../src/index.js";
import { NodeIpcTransport } from "../src/ipc.js";

type Frame = Record<string, unknown>;
type Keys = { publicKey: KeyObject; privateKey: KeyObject };
const NOW = 1_760_000_000_000;
const OLD_ID = "live-legacy-binding";
const NEW_ID = "live-instance-binding";
const INSTANCE_ID = "e".repeat(32);
const COOKIE = "independent-memory-only-cookie";
const spki = (keys: Keys): string => keys.publicKey.export({ type: "spki", format: "der" }).toString("base64url");

function record(address: string, hostId: string, keys: Keys): Rendezvous {
  return {
    bindingProtocol: 2, address, hostId, epoch: "18446744073709551614",
    processIdentity: {
      platform: process.platform, pid: process.pid, startedAtMs: NOW - 20_000,
      executable: "independent-test-only-dsh-host",
    },
    publicKeySpki: spki(keys),
  };
}

/** v2 固定七字段及嵌套顺序；期望字节不由生产 signingBytes/decisionBytes 计算。 */
function challengeBytes(hello: Frame, serverKey: string): Buffer {
  const home = hello["homeBinding"] as HomeBinding;
  return Buffer.from(JSON.stringify({
    bindingProtocol: 2,
    keyDigests: [hash(Buffer.from(serverKey, "base64url")), hash(Buffer.from(String(hello["publicKeySpki"]), "base64url"))],
    nonce: hello["nonce"], hostId: hello["hostId"], epoch: hello["epoch"], expiry: hello["expiry"],
    homeBinding: { homeRoot: home.homeRoot, hostPid: home.hostPid, hostStartedAtMs: home.hostStartedAtMs },
  }), "utf8");
}

function signedReply(hello: Frame, rv: Rendezvous, keys: Keys, bindingId: string, state: "authorized" | "pending_approval"): Frame {
  const challenge = challengeBytes(hello, rv.publicKeySpki);
  const clientKey = createPublicKey({
    key: Buffer.from(String(hello["publicKeySpki"]), "base64url"), type: "spki", format: "der",
  });
  if (!verify(null, challenge, clientKey, Buffer.from(String(hello["signature"]), "base64url"))) {
    throw new Error("independent peer rejected client challenge signature");
  }
  if (hello["hostId"] !== rv.hostId || hello["epoch"] !== rv.epoch) {
    throw new Error("client hello targeted the wrong authorization domain");
  }
  const reply: Frame = {
    type: "server_hello", bindingProtocol: 2, publicKeySpki: rv.publicKeySpki,
    nonce: hello["nonce"], hostId: hello["hostId"], epoch: hello["epoch"], expiry: hello["expiry"],
    homeBinding: { ...(hello["homeBinding"] as HomeBinding) },
    signature: sign(null, challenge, keys.privateKey).toString("base64url"),
    state, bindingId, displayCode: state === "pending_approval" ? "581624" : null,
    pendingExpiresAt: state === "pending_approval" ? hello["expiry"] : null,
  };
  const decision = Buffer.from(JSON.stringify({
    purpose: "tuvis.dsh.binding-decision.v2",
    ...JSON.parse(challenge.toString("utf8")) as Frame,
    state, bindingId, displayCode: reply["displayCode"], pendingExpiresAt: reply["pendingExpiresAt"],
  }), "utf8");
  reply["decisionSignature"] = sign(null, decision, keys.privateKey).toString("base64url");
  return reply;
}

/** 与 pending-events 独立合同一致：purpose → 完整挑战 → 首帧决策签名 → 事件 → bindingId。 */
function signedEvent(
  hello: Frame, decision: Frame, rv: Rendezvous, keys: Keys,
  event: "approval" | "revocation" | "credential_ack",
): Frame {
  const bindingId = decision["bindingId"];
  const message = Buffer.from(JSON.stringify({
    purpose: "tuvis.dsh.binding-event.v2",
    ...JSON.parse(challengeBytes(hello, rv.publicKeySpki).toString("utf8")) as Frame,
    decisionSignature: decision["decisionSignature"],
    event,
    bindingId,
  }), "utf8");
  return {
    type: event === "revocation" ? "revoked" : "granted",
    binding_id: bindingId,
    event,
    eventSignature: sign(null, message, keys.privateKey).toString("base64url"),
  };
}

class MemoryStorage implements BindingStorage {
  data: string | null = null;
  async read(): Promise<string | null> { return this.data; }
  async write(data: string): Promise<void> { this.data = data; }
}

function deferred<T>() {
  let resolve!: (value: T) => void;
  const promise = new Promise<T>((done) => { resolve = done; });
  return { promise, resolve };
}

/** 真正的数据队列；close 才模拟 EOF，测试不会在交接时替生产代码关旧线。 */
class MemoryConnection implements IpcConnection {
  closed = false;
  hello!: Frame;
  decision!: Frame;
  readonly sent: Frame[] = [];
  private readonly incoming: string[] = [];
  private waiter: { resolve: (line: string) => void; reject: (error: Error) => void } | null = null;
  constructor(readonly rv: Rendezvous, private readonly respond: (frame: Frame, conn: MemoryConnection) => void) {}
  async send(line: string): Promise<void> {
    if (this.closed) throw new Error("memory IPC closed");
    const frame = JSON.parse(line) as Frame;
    this.sent.push(frame);
    this.respond(frame, this);
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

/** 注入生产调度接口，逐秒推进所有到期工作，不依赖真实睡眠或轮询私有状态。 */
class ManualScheduler implements BindingScheduler {
  elapsed = 0;
  private id = 0;
  private readonly jobs = new Map<number, { at: number; run: () => void }>();
  setTimeout(run: () => void, ms: number): unknown {
    const id = ++this.id;
    this.jobs.set(id, { at: this.elapsed + ms, run });
    return id;
  }
  clearTimeout(id: unknown): void { this.jobs.delete(id as number); }
  advance(ms: number): void {
    this.elapsed += ms;
    const due = [...this.jobs.entries()].filter(([, job]) => job.at <= this.elapsed)
      .sort((a, b) => a[1].at - b[1].at);
    for (const [id, job] of due) if (this.jobs.delete(id)) job.run();
  }
}

function writeJson(file: string, value: unknown): void {
  mkdirSync(dirname(file), { recursive: true });
  writeFileSync(file, JSON.stringify(value), { mode: 0o600 });
}
function readJson(file: string): Frame | null {
  return existsSync(file) ? JSON.parse(readFileSync(file, "utf8")) as Frame : null;
}

describe("v2 independent: real Node IPC preserves signed Chinese UTF-8", () => {
  // 0 是有效签名/真实 socket 的阳性对照；1/2 分别切在三字节汉字内部两个边界。
  it.each([0, 1, 2])("completes signed v2 authorization when the reply is split after %i bytes of 中", async (splitBytes) => {
    const root = mkdtempSync(join(tmpdir(), "dsh-v2-ipc-"));
    const address = process.platform === "win32" ? `\\\\.\\pipe\\dsh-v2-${randomUUID()}` : join(root, "peer.sock");
    const keys = generateKeyPairSync("ed25519");
    const rv = record(address, "utf8-test-host", keys);
    const home: HomeBinding = { homeRoot: join(root, "中文 home", ".dsh"), hostPid: process.pid, hostStartedAtMs: NOW - 20_000 };
    const storage = new MemoryStorage();
    const sockets = new Set<Socket>();
    const clientChunks: Buffer[] = [];
    const received: Frame[] = [];
    const credentials: Frame[] = [];
    const exchanges: string[] = [];
    const errors: unknown[] = [];
    const firstChunk = deferred<void>();
    let expectedReply: Frame | null = null;
    let splitAt = 0;
    const observedClients = new Set<Socket>();
    const onClientData = (chunk: Buffer): void => {
      clientChunks.push(Buffer.from(chunk));
      firstChunk.resolve();
    };
    // Node 的只读诊断通道暴露真实客户端 socket；不 mock net，不替换或重发任何字节。
    // 首段确实到达 data 事件后才发尾段，避免依赖 write 边界、sleep 或系统调度运气。
    const socketChannel = channel("net.client.socket");
    const observeSocket = (message: unknown): void => {
      const socket = (message as { socket: Socket }).socket;
      observedClients.add(socket);
      socket.on("data", onClientData);
    };
    const server = createServer((socket) => {
      sockets.add(socket);
      socket.on("close", () => sockets.delete(socket));
      socket.on("error", (error) => errors.push(error));
      const decoder = new StringDecoder("utf8");
      let socketHello: Frame | null = null;
      let socketDecision: Frame | null = null;
      let buffer = "";
      socket.on("data", (chunk: Buffer) => {
        buffer += decoder.write(chunk);
        for (;;) {
          const newline = buffer.indexOf("\n");
          if (newline < 0) break;
          const frame = JSON.parse(buffer.slice(0, newline)) as Frame;
          buffer = buffer.slice(newline + 1);
          void (async () => {
            if (frame["type"] === "hello") {
              socketHello = frame;
              socketDecision = signedReply(frame, rv, keys, "utf8-binding", "authorized");
              expectedReply = socketDecision;
              const wire = Buffer.from(`${JSON.stringify(expectedReply)}\n`, "utf8");
              if (splitBytes === 0) { socket.write(wire); return; }
              const chineseOffset = wire.indexOf(Buffer.from("中", "utf8"));
              if (chineseOffset < 0) throw new Error("fixture must contain a literal Chinese signed field");
              splitAt = chineseOffset + splitBytes;
              socket.write(wire.subarray(0, splitAt));
              await firstChunk.promise;
              socket.write(wire.subarray(splitAt));
            } else if (frame["type"] === "credential") {
              credentials.push(frame);
              if (!socketHello || !socketDecision) throw new Error("credential arrived before the signed decision");
              socket.write(`${JSON.stringify(signedEvent(socketHello, socketDecision, rv, keys, "credential_ack"))}\n`);
            } else throw new Error("unexpected real IPC client frame");
          })().catch((error: unknown) => { errors.push(error); socket.destroy(); });
        }
      });
    });
    const connections: IpcConnection[] = [];
    const transport = new NodeIpcTransport();
    const client = new BindingClient({
      homeBinding: home, rendezvous: { async read() { return rv; } }, storage, clock: () => NOW,
      exchange: { async exchange() { exchanges.push(COOKIE); return COOKIE; } },
      transport: {
        async connect(target) {
          const conn = await transport.connect(target);
          connections.push(conn);
          return {
            send: (line) => conn.send(line), close: () => conn.close(),
            get closed() { return conn.closed; },
            async recv() {
              const line = await conn.recv();
              received.push(JSON.parse(line) as Frame);
              return line; // 只观察，不修复生产传输返回的乱码。
            },
          };
        },
      },
    });
    try {
      server.listen(address);
      await once(server, "listening");
      socketChannel.subscribe(observeSocket);
      const outcome = await client.runOnce();
      expect(errors).toEqual([]);
      if (splitBytes !== 0) {
        expect(clientChunks[0]?.length, "first fragment must be consumed before the remainder is written").toBe(splitAt);
        expect(clientChunks.length).toBeGreaterThanOrEqual(2);
      }
      expect(received[0]?.["homeBinding"], "signed homeBinding must survive a UTF-8 code point split").toEqual(home);
      expect(received[0], "all v2 signature and decision fields must arrive unchanged").toEqual(expectedReply);
      expect(outcome).toEqual({ state: "authorized", bindingId: "utf8-binding", cookieDelivered: true });
      expect(client.snapshot()).toEqual({ state: "bound", bindingId: "utf8-binding", displayCode: null });
      expect(exchanges).toEqual([COOKIE]);
      expect(credentials).toEqual([{ type: "credential", bindingId: "utf8-binding", cookie: COOKIE }]);
    } finally {
      socketChannel.unsubscribe(observeSocket);
      observedClients.forEach((socket) => socket.off("data", onClientData));
      firstChunk.resolve();
      connections.forEach((conn) => conn.close());
      sockets.forEach((socket) => socket.destroy());
      await new Promise<void>((resolve, reject) => server.close((error) => error ? reject(error) : resolve()));
      rmSync(root, { recursive: true, force: true });
    }
  });
});

describe("v2 independent: healthy binding follows legacy retirement", () => {
  it("automatically closes a healthy legacy connection and binds the new instance on the same BindingSession", async () => {
    const root = mkdtempSync(join(tmpdir(), "dsh-v2-live-domain-"));
    const home = join(root, "中文 home", ".dsh");
    const stateDir = join(root, "tuvis-state");
    const oldKeys = generateKeyPairSync("ed25519");
    const newKeys = generateKeyPairSync("ed25519");
    const oldRv = record("memory://healthy-legacy", "legacy-live-host", oldKeys);
    const newRv = record("memory://new-instance", INSTANCE_ID, newKeys);
    const legacyRvFile = join(stateDir, "rendezvous.json");
    const legacyIdentityFile = join(home, "profiles", "desktop", "tuvis-remote", "identity.json");
    const instanceIdentityFile = join(home, "tuvis-remote", INSTANCE_ID, "identity.json");
    writeJson(legacyRvFile, oldRv);
    const reader = instanceRendezvousReader(stateDir);
    const storage = instanceFileStorage(home, stateDir);
    const scheduler = new ManualScheduler();
    const connections: MemoryConnection[] = [];
    const exchanges: string[] = [];
    const overlapAtConnect: number[] = [];
    const peer = new BindingSession({
      storage, rendezvous: reader, scheduler, clock: () => NOW + scheduler.elapsed,
      homeBinding: (rv) => ({ homeRoot: home, hostPid: rv.processIdentity.pid, hostStartedAtMs: rv.processIdentity.startedAtMs }),
      exchange: { async exchange() { exchanges.push(COOKIE); return COOKIE; } },
      transport: {
        async connect(address) {
          const rv = address === oldRv.address ? oldRv : address === newRv.address ? newRv : null;
          if (!rv) throw new Error("unexpected memory endpoint");
          overlapAtConnect.push(connections.filter((conn) => !conn.closed).length);
          const isLegacy = rv === oldRv;
          const bindingId = isLegacy ? OLD_ID : NEW_ID;
          const conn = new MemoryConnection(rv, (frame, socket) => {
            if (frame["type"] === "hello") {
              socket.hello = frame;
              socket.decision = signedReply(frame, rv, isLegacy ? oldKeys : newKeys, bindingId, isLegacy ? "authorized" : "pending_approval");
              socket.push(socket.decision);
            } else if (frame["type"] === "credential") {
              if (frame["bindingId"] !== bindingId) throw new Error("credential crossed authorization domains");
              socket.push(signedEvent(socket.hello, socket.decision, rv, isLegacy ? oldKeys : newKeys, "credential_ack"));
            } else throw new Error("unexpected memory client frame");
          });
          connections.push(conn);
          return conn;
        },
      },
    });
    try {
      peer.start();
      await settle();
      expect(peer.snapshot()).toEqual({ state: "bound", bindingId: OLD_ID, displayCode: null });
      expect(connections).toHaveLength(1);
      const old = connections[0]!;
      const legacyBytes = readFileSync(legacyIdentityFile);
      const legacyIdentity = readJson(legacyIdentityFile)!;
      expect(legacyIdentity["trustAnchorSpki"]).toBe(oldRv.publicKeySpki);
      // 无交接时连接健康且稳定；不靠 EOF、撤权、待批到期或重新 start 触发后续切换。
      scheduler.advance(3_000);
      old.push(signedEvent(old.hello, old.decision, old.rv, oldKeys, "credential_ack"));
      await settle();
      expect(old.closed).toBe(false);
      expect(connections).toHaveLength(1);
      expect(exchanges).toEqual([COOKIE]);

      writeJson(legacyRvFile, { ...oldRv, retired: true });
      writeJson(join(stateDir, "instances", INSTANCE_ID, "rendezvous.json"), newRv);
      expect(await reader.read()).toEqual(newRv); // 真实文件选择已完成，不伪造 reader 值。
      expect(old.closed).toBe(false); // 文件发布本身没有替客户端关闭内存连接。
      // 给自动发现/退避留出 30 秒虚拟时间；此处没有 close、start、dispose 或服务端推送。
      for (let second = 0; second < 30; second++) {
        scheduler.advance(1_000);
        await settle();
        if (peer.state === "pending" && peer.snapshot().bindingId === NEW_ID) break;
      }
      expect(old.closed, "BindingSession must retire the still-healthy old connection itself").toBe(true);
      expect(connections.map((conn) => conn.rv.hostId)).toEqual([oldRv.hostId, newRv.hostId]);
      expect(peer.snapshot(), "the new domain requires its own signed handshake and approval").toEqual({
        state: "pending", bindingId: NEW_ID, displayCode: "581624",
      });
      expect(exchanges, "old bound state must not bypass approval in the new domain").toEqual([COOKIE]);
      expect(readJson(instanceIdentityFile)).toMatchObject({
        publicKeySpki: legacyIdentity["publicKeySpki"], trustAnchorSpki: null, revoked: false, revokedBindingId: null,
      });
      const next = connections.find((conn) => conn.rv.hostId === INSTANCE_ID);
      expect(next?.sent.find((frame) => frame["type"] === "hello")).toMatchObject({
        bindingProtocol: 2, hostId: INSTANCE_ID, publicKeySpki: legacyIdentity["publicKeySpki"],
        homeBinding: { homeRoot: home, hostPid: process.pid, hostStartedAtMs: NOW - 20_000 },
      });
      if (next) next.push(signedEvent(next.hello, next.decision, next.rv, newKeys, "approval"));
      await settle();
      expect(peer.snapshot()).toEqual({ state: "bound", bindingId: NEW_ID, displayCode: null });
      expect(next?.sent.filter((frame) => frame["type"] === "credential")).toEqual([
        { type: "credential", bindingId: NEW_ID, cookie: COOKIE },
      ]);
      expect(exchanges).toEqual([COOKIE, COOKIE]);
      expect(readJson(instanceIdentityFile)).toMatchObject({
        publicKeySpki: legacyIdentity["publicKeySpki"], trustAnchorSpki: newRv.publicKeySpki,
        revoked: false, revokedBindingId: null,
      });
      expect(overlapAtConnect, "close old IPC before connecting the new domain").toEqual([0, 0]);
      expect(readFileSync(legacyIdentityFile)).toEqual(legacyBytes);
    } finally {
      peer.dispose();
      connections.forEach((conn) => conn.close());
      await settle();
      rmSync(root, { recursive: true, force: true });
    }
  });
});
