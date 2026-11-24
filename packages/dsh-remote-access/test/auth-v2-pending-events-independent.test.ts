/**
 * 已签 pending 之后的连接内事件：独立行为 RED，不改生产、不声称 Rust 联调。
 * 首帧使用当前实际 v2 challenge + decisionSignature，所有 crypto 均真实。
 * 仅替换 IPC、身份存储、兑换和调度；发送的 credential 来自真实 BindingSession。
 *
 * 待实现的后续事件合同：保留 Rust 的 type/binding_id，附 event/eventSignature。
 * 独立被签字节固定序 = purpose、完整 challenge、首帧 decisionSignature、event、bindingId。
 * event 分 approval/revocation/credential_ack，防止有效批准签名被当作 ACK，或改作撤权。
 * 此签名封装仅为本文件的测试合同；当前 Rust 生产者尚不发送、TS 解码器尚不验证。
 * 凭据是否被插件发送与是否被假服务批准接收分开；不能把前者写成真实 Rust 越权。
 */
import { sha256Hex as hash } from "./wire-fixtures";
import { createPublicKey, generateKeyPairSync, sign, verify } from "node:crypto";
import { setImmediate as settle } from "node:timers/promises";
import { describe, expect, it } from "vitest";
import {
  BindingClient,
  BindingSession,
  type BindingScheduler,
  type BindingStorage,
  type HomeBinding,
  type IpcConnection,
} from "../src/auth.js";

type Frame = Record<string, unknown>;
type Event = "approval" | "revocation" | "credential_ack";
const NOW = 1_760_000_000_000;
const ID = "pending-events-binding";
const COOKIE = "pending-events-memory-only-cookie";
const HOME: HomeBinding = {
  homeRoot: "/pending-events-memory/中文 home/.dsh",
  hostPid: 9201,
  hostStartedAtMs: NOW - 10_000,
};
const keys = generateKeyPairSync("ed25519");
const otherKeys = generateKeyPairSync("ed25519");
const SPKI = keys.publicKey.export({ type: "spki", format: "der" }).toString("base64url");
const bytes = (value: Frame): Buffer => Buffer.from(JSON.stringify(value), "utf8");

/** 固定七字段与嵌套顺序，禁止调用生产 signingBytes/decisionBytes。 */
function challenge(hello: Frame): Frame {
  const home = hello["homeBinding"] as HomeBinding;
  return {
    bindingProtocol: 2,
    keyDigests: [
      hash(Buffer.from(SPKI, "base64url")),
      hash(Buffer.from(String(hello["publicKeySpki"]), "base64url")),
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
  };
}
function serverHello(hello: Frame, state: "pending_approval" | "authorized"): Frame {
  const frame: Frame = {
    type: "server_hello",
    bindingProtocol: 2,
    publicKeySpki: SPKI,
    nonce: hello["nonce"],
    hostId: hello["hostId"],
    epoch: hello["epoch"],
    expiry: hello["expiry"],
    homeBinding: { ...(hello["homeBinding"] as HomeBinding) },
    signature: sign(null, bytes(challenge(hello)), keys.privateKey).toString("base64url"),
    state,
    bindingId: ID,
    displayCode: state === "pending_approval" ? "581246" : null,
    pendingExpiresAt: state === "pending_approval" ? hello["expiry"] : null,
  };
  frame["decisionSignature"] = sign(
    null,
    bytes({
      purpose: "tuvis.dsh.binding-decision.v2",
      ...challenge(hello),
      state,
      bindingId: ID,
      displayCode: frame["displayCode"],
      pendingExpiresAt: frame["pendingExpiresAt"],
    }),
    keys.privateKey
  ).toString("base64url");
  return frame;
}
function eventBytes(hello: Frame, decision: Frame, event: Event, bindingId = ID): Buffer {
  return bytes({
    purpose: "tuvis.dsh.binding-event.v2",
    ...challenge(hello),
    decisionSignature: decision["decisionSignature"],
    event,
    bindingId,
  });
}
function signedEvent(hello: Frame, decision: Frame, event: Event): Frame {
  const message = eventBytes(hello, decision, event);
  const signature = sign(null, message, keys.privateKey);
  // 独立对端确实签出有效事件；正例仍以真实客户端的状态/credential 为断言对象。
  expect(verify(null, message, keys.publicKey, signature)).toBe(true);
  return {
    type: event === "revocation" ? "revoked" : "granted",
    binding_id: ID,
    event,
    eventSignature: signature.toString("base64url"),
  };
}
class Storage implements BindingStorage {
  data: string | null = null;
  async read(): Promise<string | null> {
    return this.data;
  }
  async write(data: string): Promise<void> {
    this.data = data;
  }
  saved(): Frame {
    return JSON.parse(this.data!) as Frame;
  }
}
class Scheduler implements BindingScheduler {
  private next = 0;
  private readonly jobs = new Map<number, { run: () => void; delay: number }>();
  setTimeout(run: () => void, delay: number): unknown {
    const id = ++this.next;
    this.jobs.set(id, { run, delay });
    return id;
  }
  clearTimeout(handle: unknown): void {
    this.jobs.delete(handle as number);
  }
  delays(): number[] {
    return [...this.jobs.values()].map((job) => job.delay).sort((a, b) => a - b);
  }
  reconnect(): void {
    const jobs = [...this.jobs.entries()].filter(([, job]) => job.delay === 1_000);
    expect(jobs).toHaveLength(1);
    const [id, job] = jobs[0]!;
    this.jobs.delete(id);
    job.run();
  }
}
class Connection implements IpcConnection {
  closed = false;
  approved = false;
  hello!: Frame;
  decision!: Frame;
  readonly sent: Frame[] = [];
  private readonly queue: string[] = [];
  private waiter: { resolve: (line: string) => void; reject: (e: Error) => void } | null = null;
  constructor(private readonly respond: (frame: Frame, conn: Connection) => void) {}
  async send(line: string): Promise<void> {
    if (this.closed) throw new Error("memory IPC closed");
    const frame = JSON.parse(line) as Frame;
    this.sent.push(frame);
    this.respond(frame, this);
  }
  async recv(): Promise<string> {
    const line = this.queue.shift();
    if (line !== undefined) return line;
    if (this.closed) throw new Error("memory IPC closed");
    if (this.waiter) throw new Error("concurrent memory recv");
    return new Promise((resolve, reject) => {
      this.waiter = { resolve, reject };
    });
  }
  push(frame: Frame): void {
    const line = JSON.stringify(frame);
    const waiter = this.waiter;
    this.waiter = null;
    if (waiter) waiter.resolve(line);
    else this.queue.push(line);
  }
  close(): void {
    this.closed = true;
    const waiter = this.waiter;
    this.waiter = null;
    waiter?.reject(new Error("memory IPC closed"));
  }
}
function harness(
  options: {
    state?: "pending_approval" | "authorized";
    kind?: "BindingClient" | "BindingSession";
    holdAck?: boolean;
    replaceCredentialReply?: (reply: Frame) => Frame;
  } = {}
) {
  const storage = new Storage();
  const scheduler = new Scheduler();
  const connections: Connection[] = [];
  const exchanges: string[] = [];
  const rejectedCredentials: Frame[] = [];
  const acceptedCredentials: Frame[] = [];
  const config = {
    homeBinding: HOME,
    storage,
    scheduler,
    clock: () => NOW,
    rendezvous: {
      async read() {
        return {
          bindingProtocol: 2,
          address: "memory://pending-events",
          hostId: "pending-events-domain",
          epoch: "18446744073709551614",
          publicKeySpki: SPKI,
          processIdentity: {
            platform: "test",
            pid: HOME.hostPid,
            startedAtMs: HOME.hostStartedAtMs,
            executable: "memory-only-dsh-host",
          },
        };
      },
    },
    exchange: {
      async exchange() {
        exchanges.push(COOKIE);
        return COOKIE;
      },
    },
    transport: {
      async connect() {
        const conn = new Connection((frame, socket) => {
          if (frame["type"] === "hello") {
            const key = createPublicKey({
              key: Buffer.from(String(frame["publicKeySpki"]), "base64url"),
              type: "spki",
              format: "der",
            });
            expect(
              verify(
                null,
                bytes(challenge(frame)),
                key,
                Buffer.from(String(frame["signature"]), "base64url")
              )
            ).toBe(true);
            socket.hello = frame;
            socket.approved = options.state === "authorized";
            socket.decision = serverHello(frame, options.state ?? "pending_approval");
            socket.push(socket.decision);
          } else if (frame["type"] === "credential") {
            if (!socket.approved || frame["bindingId"] !== ID) {
              rejectedCredentials.push(frame);
              const refusal: Frame = {
                type: "error",
                error: "DSH_AUTH_NOT_APPROVED",
                detail: "independent peer has not approved this binding",
              };
              socket.push(options.replaceCredentialReply?.(refusal) ?? refusal);
            } else {
              acceptedCredentials.push(frame);
              if (!options.holdAck)
                socket.push(signedEvent(socket.hello, socket.decision, "credential_ack"));
            }
          } else throw new Error("unexpected client frame");
        });
        connections.push(conn);
        return conn;
      },
    },
  };
  const peer =
    options.kind === "BindingClient" ? new BindingClient(config) : new BindingSession(config);
  let attempt: ReturnType<BindingClient["runOnce"]> | undefined;
  return {
    peer,
    storage,
    scheduler,
    connections,
    exchanges,
    rejectedCredentials,
    acceptedCredentials,
    credentials: () =>
      connections.flatMap((conn) => conn.sent).filter((frame) => frame["type"] === "credential"),
    current: () => connections.at(-1)!,
    event: (event: Event) => {
      const conn = connections.at(-1)!;
      return signedEvent(conn.hello, conn.decision, event);
    },
    async start() {
      if (peer instanceof BindingSession) peer.start();
      else attempt = peer.runOnce();
      await settle();
    },
    async push(frame: Frame) {
      connections.at(-1)!.push(frame);
      await settle();
    },
    async approve() {
      const conn = connections.at(-1)!;
      conn.approved = true;
      conn.push(signedEvent(conn.hello, conn.decision, "approval"));
      await settle();
    },
    async dispose() {
      if (peer instanceof BindingSession) peer.dispose();
      connections.forEach((conn) => conn.close());
      await attempt?.catch(() => undefined);
      await settle();
    },
  };
}
function assertPending(h: ReturnType<typeof harness>): void {
  expect(h.peer.snapshot()).toEqual({ state: "pending", bindingId: ID, displayCode: "581246" });
  expect(h.exchanges).toEqual([]);
  expect(h.credentials()).toEqual([]);
  expect(h.storage.saved()).toMatchObject({ revoked: false, trustAnchorSpki: null });
}

describe("signed pending: approval event must authenticate the current challenge and decision", () => {
  it("keeps a fully signed pending connection credential-free until approval", async () => {
    const h = harness();
    try {
      await h.start();
      assertPending(h);
      expect(h.connections).toHaveLength(1);
    } finally {
      await h.dispose();
    }
  });
  it("delivers exactly once after valid current approval and credential ACK proofs", async () => {
    const h = harness();
    try {
      await h.start();
      assertPending(h);
      await h.approve();
      expect(h.peer.snapshot()).toEqual({ state: "bound", bindingId: ID, displayCode: null });
      expect(h.exchanges).toEqual([COOKIE]);
      expect(h.credentials()).toEqual([{ type: "credential", bindingId: ID, cookie: COOKIE }]);
      expect(h.storage.saved()).toMatchObject({ trustAnchorSpki: SPKI, revoked: false });
      expect(h.scheduler.delays()).toEqual([]);
      await h.push(h.event("credential_ack"));
      expect(h.exchanges).toEqual([COOKIE]); // 重复 ACK 不是第二次批准。
      expect(h.connections).toHaveLength(1);
    } finally {
      await h.dispose();
    }
  });

  const replacements: Array<{ label: string; frame: (h: ReturnType<typeof harness>) => Frame }> = [
    { label: "unsigned matching Granted", frame: () => ({ type: "granted", binding_id: ID }) },
    {
      label: "Revoked changed to Granted",
      frame: (h) => ({ ...h.event("revocation"), type: "granted" }),
    },
    {
      label: "another bindingId",
      frame: (h) => ({ ...h.event("approval"), binding_id: "other-binding" }),
    },
    {
      label: "modified event signature",
      frame: (h) => {
        const frame = h.event("approval");
        const sig = Buffer.from(String(frame["eventSignature"]), "base64url");
        sig[0] = sig[0]! ^ 1;
        return { ...frame, eventSignature: sig.toString("base64url") };
      },
    },
    {
      label: "pending decision signature reused as approval",
      frame: (h) => ({
        ...h.event("approval"),
        eventSignature: h.current().decision["decisionSignature"],
      }),
    },
    { label: "ACK proof reused as approval", frame: (h) => h.event("credential_ack") },
    ...(["nonce", "hostId", "epoch", "homeRoot"] as const).map((field) => ({
      label: `valid signature for another ${field}`,
      frame(h: ReturnType<typeof harness>): Frame {
        const hello = structuredClone(h.current().hello);
        if (field === "homeRoot") (hello["homeBinding"] as Frame)[field] = "/other/.dsh";
        else
          hello[field] =
            field === "nonce"
              ? Buffer.alloc(32, 7).toString("base64url")
              : field === "epoch"
                ? "18446744073709551613"
                : "other-domain";
        return signedEvent(hello, h.current().decision, "approval");
      },
    })),
    {
      label: "another signed pending decision",
      frame: (h) => {
        const hello = h.current().hello;
        const decision: Frame = { ...h.current().decision, displayCode: "654321" };
        decision["decisionSignature"] = sign(
          null,
          bytes({
            purpose: "tuvis.dsh.binding-decision.v2",
            ...challenge(hello),
            state: "pending_approval",
            bindingId: ID,
            displayCode: "654321",
            pendingExpiresAt: hello["expiry"],
          }),
          keys.privateKey
        ).toString("base64url");
        return signedEvent(hello, decision, "approval");
      },
    },
    {
      label: "another server key",
      frame: (h) => ({
        ...h.event("approval"),
        eventSignature: sign(
          null,
          eventBytes(h.current().hello, h.current().decision, "approval"),
          otherKeys.privateKey
        ).toString("base64url"),
      }),
    },
  ];
  it.each(replacements)("does not exchange or send a credential for $label", async ({ frame }) => {
    const h = harness();
    try {
      await h.start();
      assertPending(h);
      await h.push(frame(h)); // 未调用 approve；独立服务仍拒绝接收 credential。
      expect
        .soft(h.exchanges, "pending decision cannot be upgraded by an unauthenticated push")
        .toEqual([]);
      expect
        .soft(
          h.credentials(),
          "observe the real plugin send boundary, not a fake acceptance result"
        )
        .toEqual([]);
      expect.soft(h.peer.state).not.toBe("bound");
      expect(h.storage.saved()).toMatchObject({ revoked: false, trustAnchorSpki: null });
    } finally {
      await h.dispose();
    }
  });
  it("does not send or falsely bind when an unapproved push and refusal ACK are both replaced", async () => {
    const h = harness({ replaceCredentialReply: () => ({ type: "granted", binding_id: ID }) });
    try {
      await h.start();
      assertPending(h);
      await h.push({ type: "granted", binding_id: ID });
      expect.soft(h.exchanges).toEqual([]);
      expect.soft(h.credentials()).toEqual([]);
      expect.soft(h.peer.state).not.toBe("bound");
      expect(h.storage.saved()["trustAnchorSpki"]).toBeNull();
    } finally {
      await h.dispose();
    }
  });
  it("rejects an approval proof captured on the previous connection after nonce rotation", async () => {
    const h = harness();
    try {
      await h.start();
      assertPending(h);
      h.current().approved = true; // 旧连接确已批准；签出的推送迟到，尚未被插件消费。
      const old = h.event("approval");
      const nonce = h.current().hello["nonce"];
      h.current().close();
      await settle();
      h.scheduler.reconnect();
      await settle();
      assertPending(h);
      expect(h.connections).toHaveLength(2);
      expect(h.current().hello["nonce"]).not.toBe(nonce);
      await h.push(old);
      expect.soft(h.exchanges).toEqual([]);
      expect(h.credentials()).toEqual([]);
    } finally {
      await h.dispose();
    }
  });
});

describe("signed pending: revocation events require their own current proof", () => {
  it("persists a valid current signed revocation and stops the pending connection", async () => {
    const h = harness();
    try {
      await h.start();
      assertPending(h);
      await h.push(h.event("revocation"));
      expect(h.peer.snapshot()).toEqual({ state: "revoked", bindingId: ID, displayCode: null });
      expect(h.storage.saved()).toMatchObject({ revoked: true, revokedBindingId: ID });
      expect(h.current().closed).toBe(true);
      expect(h.exchanges).toEqual([]);
      expect(h.scheduler.delays()).toEqual([]);
    } finally {
      await h.dispose();
    }
  });
  const replacements: Array<{ label: string; frame: (h: ReturnType<typeof harness>) => Frame }> = [
    { label: "unsigned matching Revoked", frame: () => ({ type: "revoked", binding_id: ID }) },
    {
      label: "unsigned PEER_REVOKED error",
      frame: () => ({ type: "error", error: "DSH_PEER_REVOKED", detail: "unproved revocation" }),
    },
    {
      label: "Granted changed to Revoked",
      frame: (h) => ({ ...h.event("approval"), type: "revoked" }),
    },
    {
      label: "another bindingId",
      frame: (h) => ({ ...h.event("revocation"), binding_id: "other-binding" }),
    },
    {
      label: "valid revocation for another nonce",
      frame: (h) =>
        signedEvent(
          { ...h.current().hello, nonce: Buffer.alloc(32, 9).toString("base64url") },
          h.current().decision,
          "revocation"
        ),
    },
  ];
  it.each(replacements)("does not persist a permanent tombstone from $label", async ({ frame }) => {
    const h = harness();
    try {
      await h.start();
      assertPending(h);
      const before = h.storage.data;
      await h.push(frame(h));
      expect.soft(h.peer.state).not.toBe("revoked");
      expect.soft(hash(Buffer.from(h.storage.data!))).toBe(hash(Buffer.from(before!)));
      expect(h.exchanges).toEqual([]);
    } finally {
      await h.dispose();
    }
  });
});

describe("credential ACK: current approval proof cannot be replaced or reused as acceptance", () => {
  it("stays pending until a valid ACK arrives after one legitimate credential delivery", async () => {
    const h = harness({ holdAck: true });
    try {
      await h.start();
      assertPending(h);
      await h.approve();
      expect(h.exchanges).toEqual([COOKIE]);
      expect(h.peer.state).toBe("pending");
      expect(h.storage.saved()["trustAnchorSpki"]).toBeNull();
      await h.push(h.event("credential_ack"));
      expect(h.peer.state).toBe("bound");
      expect(h.credentials()).toHaveLength(1);
      expect(h.storage.saved()["trustAnchorSpki"]).toBe(SPKI);
    } finally {
      await h.dispose();
    }
  });
  const replacements: Array<{ label: string; frame: (h: ReturnType<typeof harness>) => Frame }> = [
    { label: "unsigned matching ACK", frame: () => ({ type: "granted", binding_id: ID }) },
    { label: "approval push reused as ACK", frame: (h) => h.event("approval") },
    {
      label: "revocation proof relabeled ACK",
      frame: (h) => ({ ...h.event("revocation"), type: "granted" }),
    },
    {
      label: "ACK signed for another nonce",
      frame: (h) =>
        signedEvent(
          { ...h.current().hello, nonce: Buffer.alloc(32, 5).toString("base64url") },
          h.current().decision,
          "credential_ack"
        ),
    },
    {
      label: "ACK signed for another decision",
      frame: (h) =>
        signedEvent(
          h.current().hello,
          serverHello(h.current().hello, "authorized"),
          "credential_ack"
        ),
    },
    {
      label: "ACK for another bindingId",
      frame: (h) => ({ ...h.event("credential_ack"), binding_id: "other-binding" }),
    },
  ];
  it.each(replacements)("does not become bound or pin trust after $label", async ({ frame }) => {
    const h = harness({ holdAck: true });
    try {
      await h.start();
      assertPending(h);
      await h.approve();
      expect(h.credentials()).toEqual([{ type: "credential", bindingId: ID, cookie: COOKIE }]);
      expect(h.peer.state).toBe("pending");
      await h.push(frame(h));
      expect.soft(h.peer.state).not.toBe("bound");
      expect.soft(h.storage.saved()["trustAnchorSpki"]).toBeNull();
      expect(h.exchanges).toEqual([COOKIE]);
      expect(h.credentials()).toHaveLength(1);
    } finally {
      await h.dispose();
    }
  });
  it.each(["BindingClient", "BindingSession"] as const)(
    "%s accepts a current signed ACK after a signed authorized first frame",
    async (kind) => {
      const h = harness({ kind, state: "authorized", holdAck: true });
      try {
        await h.start();
        expect(h.credentials()).toHaveLength(1);
        expect(h.peer.state).not.toBe("bound");
        await h.push(h.event("credential_ack"));
        expect(h.peer.snapshot()).toEqual({ state: "bound", bindingId: ID, displayCode: null });
        expect(h.storage.saved()["trustAnchorSpki"]).toBe(SPKI);
        expect(h.exchanges).toEqual([COOKIE]);
      } finally {
        await h.dispose();
      }
    }
  );
  it.each(["BindingClient", "BindingSession"] as const)(
    "%s also rejects an unsigned ACK after a signed authorized first frame",
    async (kind) => {
      const h = harness({ kind, state: "authorized", holdAck: true });
      try {
        await h.start();
        expect(h.credentials()).toHaveLength(1);
        expect(h.peer.state).not.toBe("bound");
        await h.push({ type: "granted", binding_id: ID });
        expect.soft(h.peer.state).not.toBe("bound");
        expect(h.storage.saved()["trustAnchorSpki"]).toBeNull();
      } finally {
        await h.dispose();
      }
    }
  );
});
