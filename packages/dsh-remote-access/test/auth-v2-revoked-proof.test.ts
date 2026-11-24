/**
 * v2 已撤权首帧的待实现合同；仅新增测试，不实现协议。
 * signature 保留七字段挑战签名；统一 decisionSignature 独立绑定用途和全部决策。
 * 固定顺序：purpose、完整 challenge、state、bindingId、displayCode、pendingExpiresAt。
 * 最后两项显式包含 null；授权/待批/撤权均需决策签名，不得靠未签状态切换权限。
 * Rust 对应 tests/dsh_auth_v2_revoked_proof_test.rs；普通握手签名不能改作撤权证明。
 * crypto、客户端握手、持久身份读写逻辑均真实；单测仅隔离 IPC、存储、兑换和调度。
 * 最后一例须由 Rust 启动并提供桥接端点，独立运行 TS 时显式 skip，不代表联调成功。
 */
import { sha256Hex as digest } from "./wire-fixtures";
import { createPublicKey, generateKeyPairSync, sign, verify } from "node:crypto";
import { mkdtempSync, mkdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { setImmediate as settle } from "node:timers/promises";
import { describe, expect, it } from "vitest";
import {
  BindingClient,
  BindingSession,
  type BindingScheduler,
  type BindingStorage,
  type HomeBinding,
  type IpcConnection,
  type IpcTransport,
  type Rendezvous,
} from "../src/auth.js";
import { NodeIpcTransport } from "../src/ipc.js";
import { instanceFileStorage } from "../src/index.js";

type Frame = Record<string, unknown>;
type Kind = "BindingClient" | "BindingSession";
const NOW = 1_760_000_000_000;
const ID = "v2-revoked-proof-binding";
const COOKIE = "memory-only-owner-cookie";
const PURPOSE = "tuvis.dsh.binding-decision.v2";
const HOME: HomeBinding = {
  homeRoot: "/revoked-proof-memory/自定义 home/.dsh",
  hostPid: 4201,
  hostStartedAtMs: NOW - 10_000,
};
const keys = generateKeyPairSync("ed25519");
const otherKeys = generateKeyPairSync("ed25519");
const SPKI = keys.publicKey.export({ type: "spki", format: "der" }).toString("base64url");
const RV: Rendezvous = {
  bindingProtocol: 2,
  address: "memory://revoked-proof",
  hostId: "revoked-proof-instance",
  epoch: "18446744073709551614",
  processIdentity: {
    platform: "memory-test",
    executable: "memory-only-dsh-desktop",
    pid: HOME.hostPid,
    startedAtMs: HOME.hostStartedAtMs,
  },
  publicKeySpki: SPKI,
};

/** 独立编码，禁止调用生产 signingBytes；Rust 独立结构保持此字段顺序。 */
function challenge(hello: Frame, spki = SPKI): Frame {
  const home = hello["homeBinding"] as HomeBinding;
  return {
    bindingProtocol: hello["bindingProtocol"],
    keyDigests: [
      digest(Buffer.from(spki, "base64url")),
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
  };
}
const bytes = (value: Frame): Buffer => Buffer.from(JSON.stringify(value), "utf8");
function proofBytes(
  hello: Frame,
  options: {
    bindingId?: string;
    spki?: string;
    state?: string;
    purpose?: string;
    displayCode?: string | null;
    pendingExpiresAt?: number | null;
  } = {}
): Buffer {
  const {
    bindingId = ID,
    spki = SPKI,
    state = "revoked",
    purpose = PURPOSE,
    displayCode = null,
    pendingExpiresAt = null,
  } = options;
  return bytes({
    purpose,
    ...challenge(hello, spki),
    state,
    bindingId,
    displayCode,
    pendingExpiresAt,
  });
}
function reply(hello: Frame, revoked: boolean): Frame {
  return {
    type: "server_hello",
    bindingProtocol: hello["bindingProtocol"],
    publicKeySpki: SPKI,
    nonce: hello["nonce"],
    hostId: hello["hostId"],
    epoch: hello["epoch"],
    expiry: hello["expiry"],
    homeBinding: { ...(hello["homeBinding"] as HomeBinding) },
    signature: sign(null, bytes(challenge(hello)), keys.privateKey).toString("base64url"),
    decisionSignature: sign(
      null,
      proofBytes(hello, {
        state: revoked ? "revoked" : "authorized",
      }),
      keys.privateKey
    ).toString("base64url"),
    state: revoked ? "revoked" : "authorized",
    bindingId: ID,
    displayCode: null,
    pendingExpiresAt: null,
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
  private readonly jobs = new Map<number, { run: () => void; ms: number }>();
  setTimeout(run: () => void, ms: number): unknown {
    const id = ++this.next;
    this.jobs.set(id, { run, ms });
    return id;
  }
  clearTimeout(handle: unknown): void {
    this.jobs.delete(handle as number);
  }
  delays(): number[] {
    return [...this.jobs.values()].map((job) => job.ms);
  }
  fireRetry(): void {
    expect(this.delays()).toEqual([1_000]);
    const [id, job] = [...this.jobs.entries()][0]!;
    this.jobs.delete(id);
    job.run();
  }
}
class Connection implements IpcConnection {
  closed = false;
  readonly sent: Frame[] = [];
  private readonly queue: string[] = [];
  private hello: Frame | null = null;
  private decision: Frame | null = null;
  private waiter: { resolve: (line: string) => void; reject: (e: Error) => void } | null = null;
  constructor(private readonly firstReply: (hello: Frame) => Frame) {}
  async send(line: string): Promise<void> {
    if (this.closed) throw new Error("memory IPC closed");
    const frame = JSON.parse(line) as Frame;
    this.sent.push(frame);
    if (frame["type"] === "hello") {
      // 必须收到当前真实客户端签出的 v2 hello，不能用写死 nonce 的伪握手。
      const publicKey = createPublicKey({
        key: Buffer.from(String(frame["publicKeySpki"]), "base64url"),
        type: "spki",
        format: "der",
      });
      expect(
        verify(
          null,
          bytes(challenge(frame)),
          publicKey,
          Buffer.from(String(frame["signature"]), "base64url")
        )
      ).toBe(true);
      this.hello = frame;
      this.decision = this.firstReply(frame);
      this.push(this.decision);
    } else if (frame["type"] === "credential") {
      if (!this.hello || !this.decision) throw new Error("credential before hello");
      const event = "credential_ack";
      const data = bytes({
        purpose: "tuvis.dsh.binding-event.v2",
        ...challenge(this.hello),
        decisionSignature: this.decision["decisionSignature"],
        event,
        bindingId: frame["bindingId"],
      });
      this.push({ type: "granted", binding_id: frame["bindingId"], event,
        eventSignature: sign(null, data, keys.privateKey).toString("base64url") }); // 保留 Rust binding_id。
    } else throw new Error(`unexpected client frame: ${String(frame["type"])}`);
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
  private push(frame: Frame): void {
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
function harness(kind: Kind, storage: BindingStorage, firstReply: (hello: Frame) => Frame) {
  const connections: Connection[] = [];
  const scheduler = new Scheduler();
  const exchanges: string[] = [];
  const options = {
    homeBinding: HOME,
    storage,
    scheduler,
    clock: () => NOW,
    rendezvous: {
      async read() {
        return RV;
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
        const conn = new Connection(
          connections.length === 0 ? firstReply : (hello) => reply(hello, false)
        );
        connections.push(conn);
        return conn;
      },
    },
  };
  const peer = kind === "BindingClient" ? new BindingClient(options) : new BindingSession(options);
  return {
    peer,
    connections,
    scheduler,
    exchanges,
    async run() {
      if (peer instanceof BindingClient) return await peer.runOnce();
      peer.start();
      await settle();
      return undefined;
    },
    async dispose() {
      if (peer instanceof BindingSession) peer.dispose();
      connections.forEach((conn) => conn.close());
      await settle();
    },
  };
}
async function pinnedStorage(): Promise<Storage> {
  const storage = new Storage();
  const initial = harness("BindingClient", storage, (hello) => reply(hello, false));
  try {
    expect(await initial.run()).toEqual({
      state: "authorized",
      bindingId: ID,
      cookieDelivered: true,
    });
    expect(storage.saved()).toMatchObject({
      trustAnchorSpki: SPKI,
      revoked: false,
      revokedBindingId: null,
    });
  } finally {
    await initial.dispose();
  }
  return storage;
}

type Defect = { label: string; make: (hello: Frame) => Frame };
function tamper(label: string, mutate: (frame: Frame) => void): Defect {
  return {
    label,
    make(hello) {
      const frame = reply(hello, true);
      mutate(frame);
      return frame;
    },
  };
}
function foreignChallenge(label: string, mutate: (hello: Frame) => void): Defect {
  return {
    label,
    make(hello) {
      const foreign = structuredClone(hello);
      mutate(foreign);
      return reply(foreign, true); // 外域/旧挑战的签名真实有效，仍不能用于本次握手。
    },
  };
}
const defects: Defect[] = [
  { label: "unsigned revoked notification", make: () => ({ type: "revoked", binding_id: ID }) },
  {
    label: "unsigned PEER_REVOKED error",
    make: () => ({ type: "error", error: "DSH_PEER_REVOKED", detail: "unsigned" }),
  },
  tamper("missing challenge signature", (frame) => {
    delete frame["signature"];
  }),
  tamper("missing proof signature", (frame) => {
    delete frame["decisionSignature"];
  }),
  tamper("empty proof signature", (frame) => {
    frame["decisionSignature"] = "";
  }),
  tamper("revoked decision relabeled authorized", (frame) => {
    frame["state"] = "authorized";
  }),
  tamper("changed displayCode", (frame) => {
    frame["displayCode"] = "654321";
  }),
  tamper("changed pendingExpiresAt", (frame) => {
    frame["pendingExpiresAt"] = NOW + 120_000;
  }),
  tamper("changed bindingId", (frame) => {
    frame["bindingId"] = "other-binding";
  }),
  tamper("changed nonce after signing", (frame) => {
    frame["nonce"] = Buffer.alloc(32, 8).toString("base64url");
  }),
  tamper("changed home after signing", (frame) => {
    (frame["homeBinding"] as Frame)["homeRoot"] = "/other/.dsh";
  }),
  tamper("changed signature bytes", (frame) => {
    const signature = Buffer.from(String(frame["decisionSignature"]), "base64url");
    signature[0] = signature[0]! ^ 1;
    frame["decisionSignature"] = signature.toString("base64url");
  }),
  {
    label: "proof signed by another server key",
    make(hello) {
      return {
        ...reply(hello, true),
        decisionSignature: sign(null, proofBytes(hello), otherKeys.privateKey).toString(
          "base64url"
        ),
      };
    },
  },
  {
    label: "ordinary challenge signature relabeled as revoked",
    make(hello) {
      return { ...reply(hello, false), state: "revoked" };
    },
  },
  {
    label: "proof for another client key",
    make(hello) {
      const foreign = {
        ...hello,
        publicKeySpki: otherKeys.publicKey
          .export({ type: "spki", format: "der" })
          .toString("base64url"),
      };
      return {
        ...reply(hello, true),
        decisionSignature: sign(null, proofBytes(foreign), keys.privateKey).toString("base64url"),
      };
    },
  },
  {
    label: "ordinary challenge signature reused as decision proof",
    make(hello) {
      const frame = reply(hello, true);
      frame["decisionSignature"] = frame["signature"];
      return frame;
    },
  },
  {
    label: "valid signature for another purpose",
    make(hello) {
      return {
        ...reply(hello, true),
        decisionSignature: sign(
          null,
          proofBytes(hello, { purpose: "tuvis.dsh.other-purpose.v2" }),
          keys.privateKey
        ).toString("base64url"),
      };
    },
  },
  {
    label: "authorized decision signed without explicit null metadata",
    make(hello) {
      const omitted = JSON.parse(
        proofBytes(hello, { state: "authorized" }).toString("utf8")
      ) as Frame;
      delete omitted["displayCode"];
      delete omitted["pendingExpiresAt"];
      return {
        ...reply(hello, false),
        decisionSignature: sign(null, bytes(omitted), keys.privateKey).toString("base64url"),
      };
    },
  },
  {
    label: "authorized decision signed for another purpose",
    make(hello) {
      return {
        ...reply(hello, false),
        decisionSignature: sign(
          null,
          proofBytes(hello, { state: "authorized", purpose: "tuvis.dsh.other-purpose.v2" }),
          keys.privateKey
        ).toString("base64url"),
      };
    },
  },
  foreignChallenge("validly signed proof for another nonce", (hello) => {
    hello["nonce"] = Buffer.alloc(32, 9).toString("base64url");
  }),
  foreignChallenge("validly signed proof for another domain", (hello) => {
    hello["hostId"] = "other-instance";
  }),
  foreignChallenge("validly signed proof for another epoch", (hello) => {
    hello["epoch"] = "18446744073709551613";
  }),
  foreignChallenge("validly signed proof for another expiry", (hello) => {
    hello["expiry"] = NOW - 1;
  }),
  foreignChallenge("validly signed proof for v1", (hello) => {
    hello["bindingProtocol"] = 1;
  }),
  ...(["homeRoot", "hostPid", "hostStartedAtMs"] as const).map((field) =>
    foreignChallenge(`validly signed proof for another ${field}`, (hello) => {
      (hello["homeBinding"] as Frame)[field] =
        field === "homeRoot" ? "/other/.dsh" : HOME[field] + 1;
    })
  ),
];
const decisionDefects: Defect[] = (["authorized", "pending_approval"] as const).flatMap((state) =>
  [
    {
      field: "state",
      change(frame: Frame) {
        frame["state"] = state === "pending_approval" ? "authorized" : "pending_approval";
      },
    },
    {
      field: "bindingId",
      change(frame: Frame) {
        frame["bindingId"] = "other-binding";
      },
    },
    {
      field: "displayCode",
      change(frame: Frame) {
        frame["displayCode"] = "654321";
      },
    },
    {
      field: "pendingExpiresAt",
      change(frame: Frame) {
        frame["pendingExpiresAt"] = NOW + 120_000;
      },
    },
    {
      field: "missing decisionSignature",
      change(frame: Frame) {
        delete frame["decisionSignature"];
      },
    },
    {
      field: "ordinary challenge used as decisionSignature",
      change(frame: Frame) {
        frame["decisionSignature"] = frame["signature"];
      },
    },
  ].map(({ field, change }) => ({
    label:
      field === "state"
        ? state === "pending_approval"
          ? "pending to authorized"
          : "authorized to pending"
        : `${state === "pending_approval" ? "pending" : "authorized"} ${field}`,
    make(hello: Frame) {
      const displayCode = state === "pending_approval" ? "123456" : null;
      const pendingExpiresAt = state === "pending_approval" ? NOW + 60_000 : null;
      const signed = proofBytes(hello, { state, displayCode, pendingExpiresAt });
      const signature = sign(null, signed, keys.privateKey);
      expect(verify(null, signed, keys.publicKey, signature)).toBe(true);
      const frame: Frame = {
        ...reply(hello, false),
        state,
        displayCode,
        pendingExpiresAt,
        decisionSignature: signature.toString("base64url"),
      };
      change(frame); // 原挑战签名仍有效，决策签名未重签，必须在兑换前拒绝。
      return frame;
    },
  }))
);

for (const kind of ["BindingClient", "BindingSession"] as const) {
  describe(`v2 authenticated first-frame revocation: ${kind}`, () => {
    it("keeps a valid signed pending decision pending without exchanging credentials", async () => {
      const storage = await pinnedStorage();
      const h = harness(kind, storage, (hello) => ({
        ...reply(hello, false),
        state: "pending_approval",
        displayCode: "123456",
        pendingExpiresAt: NOW + 60_000,
        decisionSignature: sign(
          null,
          proofBytes(hello, {
            state: "pending_approval",
            displayCode: "123456",
            pendingExpiresAt: NOW + 60_000,
          }),
          keys.privateKey
        ).toString("base64url"),
      }));
      try {
        await h.run();
        expect(h.peer.snapshot()).toEqual({
          state: "pending",
          bindingId: ID,
          displayCode: "123456",
        });
        expect(h.exchanges).toEqual([]);
        expect(h.connections[0]!.sent.map((frame) => frame["type"])).toEqual(["hello"]);
        expect(storage.saved()).toMatchObject({ revoked: false, revokedBindingId: null });
      } finally {
        await h.dispose();
      }
    });

    it("persists a tombstone from a current signed revocation proof and stops delivery after restart", async () => {
      const storage = await pinnedStorage();
      const before = storage.saved();
      const h = harness(kind, storage, (hello) => reply(hello, true));
      let restarted: ReturnType<typeof harness> | undefined;
      try {
        const outcome = await h.run();
        expect
          .soft(h.peer.snapshot())
          .toEqual({ state: "revoked", bindingId: ID, displayCode: null });
        expect.soft(storage.saved()).toMatchObject({ revoked: true, revokedBindingId: ID });
        expect.soft(h.exchanges).toEqual([]);
        expect.soft(h.connections[0]!.sent.map((frame) => frame["type"])).toEqual(["hello"]);
        expect.soft(h.connections[0]!.closed).toBe(true);
        expect.soft(h.scheduler.delays()).toEqual([]);
        if (h.peer instanceof BindingClient)
          expect.soft(outcome).toEqual({ state: "revoked", bindingId: ID });
        expect(storage.saved()).toMatchObject({
          publicKeySpki: before["publicKeySpki"],
          privateKeyPem: before["privateKeyPem"],
          trustAnchorSpki: SPKI,
        });
        await h.dispose();
        restarted = harness(kind, storage, (hello) => reply(hello, false));
        await restarted.run();
        expect.soft(restarted.peer.state).toBe("revoked");
        expect.soft(restarted.connections).toHaveLength(0);
        expect.soft(restarted.exchanges).toEqual([]);
        expect(restarted.scheduler.delays()).toEqual([]);
      } finally {
        await h.dispose();
        await restarted?.dispose();
      }
    });

    it.each([...defects, ...decisionDefects])(
      "rejects $label without persisting a tombstone or preventing recovery",
      async ({ make }) => {
        const storage = await pinnedStorage();
        const before = storage.data;
        const h = harness(kind, storage, make);
        let recovered: ReturnType<typeof harness> | undefined;
        try {
          const outcome = await h.run();
          expect(h.connections).toHaveLength(1);
          expect(h.connections[0]!.sent.map((frame) => frame["type"])).toEqual(["hello"]);
          expect(h.exchanges).toEqual([]);
          expect(h.peer.state).toBe("idle");
          expect(
            storage.data,
            "invalid proof must leave the entire pinned identity untouched"
          ).toBe(before);
          expect(h.connections[0]!.closed).toBe(true);
          if (h.peer instanceof BindingSession) {
            h.scheduler.fireRetry();
            await settle();
            expect(h.connections).toHaveLength(2);
            expect(h.peer.state).toBe("bound");
            expect(h.scheduler.delays()).toEqual([]);
          } else {
            expect(outcome).toMatchObject({ state: "refused" });
            recovered = harness(kind, storage, (hello) => reply(hello, false));
            await recovered.run();
            expect(recovered.peer.state).toBe("bound");
          }
          expect(storage.saved()).toMatchObject({
            revoked: false,
            revokedBindingId: null,
            trustAnchorSpki: SPKI,
          });
        } finally {
          await h.dispose();
          await recovered?.dispose();
        }
      }
    );

    it("rejects a previously valid proof replayed against a fresh runtime nonce", async () => {
      const storage = await pinnedStorage();
      let previousHello: Frame | undefined;
      let previousProof: Frame | undefined;
      const capture = harness(kind, storage, (hello) => {
        previousHello = hello;
        previousProof = reply(hello, true);
        return reply(hello, false); // 只保留证明，首轮仍正常绑定，不先写墓碑。
      });
      try {
        await capture.run();
        expect(capture.peer.state).toBe("bound");
      } finally {
        await capture.dispose();
      }
      const before = storage.data;
      const h = harness(kind, storage, () => previousProof!);
      try {
        await h.run();
        expect(h.connections[0]!.sent[0]!["nonce"]).not.toBe(previousHello!["nonce"]);
        expect(h.peer.state).toBe("idle");
        expect(storage.data).toBe(before);
        expect(h.exchanges).toEqual([]);
        expect(h.connections[0]!.closed).toBe(true);
      } finally {
        await h.dispose();
      }
    });
  });
}

describe("v2 revocation proof survives reopening production file storage", () => {
  it.each(["BindingClient", "BindingSession"] as const)(
    "%s saves the tombstone to disk and suppresses delivery after reopening storage",
    async (kind) => {
      const root = mkdtempSync(join(tmpdir(), "tuvis-revoked-proof-"));
      const home = join(root, "isolated-dsh-home");
      const state = join(root, "isolated-tuvis-state");
      const identityFile = join(home, "profiles", "desktop", "tuvis-remote", "identity.json");
      mkdirSync(state);
      writeFileSync(join(state, "rendezvous.json"), JSON.stringify(RV), { mode: 0o600 });
      const storage = instanceFileStorage(home, state, HOME.hostPid);
      const initial = harness("BindingClient", storage, (hello) => reply(hello, false));
      const revoked = harness(kind, storage, (hello) => reply(hello, true));
      let reopened: ReturnType<typeof harness> | undefined;
      try {
        expect(await initial.run()).toMatchObject({ state: "authorized", bindingId: ID });
        await initial.dispose();
        const before = JSON.parse(readFileSync(identityFile, "utf8")) as Frame;
        expect(before).toMatchObject({ trustAnchorSpki: SPKI, revoked: false });
        await revoked.run();
        const onDisk = JSON.parse(readFileSync(identityFile, "utf8")) as Frame;
        expect.soft(onDisk).toMatchObject({ revoked: true, revokedBindingId: ID });
        expect(onDisk).toMatchObject({
          publicKeySpki: before["publicKeySpki"],
          trustAnchorSpki: SPKI,
        });
        expect(revoked.exchanges).toEqual([]);
        await revoked.dispose();
        reopened = harness(kind, instanceFileStorage(home, state, HOME.hostPid), (hello) =>
          reply(hello, false)
        );
        await reopened.run();
        expect
          .soft(reopened.peer.snapshot())
          .toEqual({ state: "revoked", bindingId: ID, displayCode: null });
        expect.soft(reopened.connections).toHaveLength(0);
        expect.soft(reopened.exchanges).toEqual([]);
        expect(reopened.scheduler.delays()).toEqual([]);
      } finally {
        await initial.dispose();
        await revoked.dispose();
        await reopened?.dispose();
        rmSync(root, { recursive: true, force: true });
      }
    }
  );
});

/** Rust 独占临时 UDS 转发 mem IPC，来回均不解析重写；身份与进程证据仍为夹具。 */
it.skipIf(!process.env["TUVIS_TEST_REVOKED_WIRE"])(
  "consumes Rust service revocation wire with the actual TS plugin handshake",
  async () => {
    const config = JSON.parse(process.env["TUVIS_TEST_REVOKED_WIRE"]!) as {
      rv: Rendezvous;
      home: HomeBinding;
      now: number;
    };
    const storage = new Storage();
    const scheduler = new Scheduler();
    const connections: IpcConnection[] = [];
    const exchanges: string[] = [];
    const incoming: Frame[] = [];
    let finishSecondClose!: () => void;
    const secondClosed = new Promise<void>((resolve) => {
      finishSecondClose = resolve;
    });
    const transport: IpcTransport = {
      async connect(address) {
        const conn = await new NodeIpcTransport().connect(address);
        connections.push(conn);
        const attempt = connections.length;
        return {
          send: (line) => conn.send(line),
          async recv() {
            const line = await conn.recv();
            incoming.push(JSON.parse(line) as Frame);
            return line;
          },
          close() {
            conn.close();
            if (attempt === 2) finishSecondClose();
          },
          get closed() {
            return conn.closed;
          },
        };
      },
    };
    const options = {
      storage,
      scheduler,
      transport,
      homeBinding: config.home,
      rendezvous: {
        async read() {
          return config.rv;
        },
      },
      clock: () => config.now,
      exchange: {
        async exchange() {
          exchanges.push(COOKIE);
          return COOKIE;
        },
      },
    };
    const first = new BindingClient(options);
    const next = new BindingSession(options);
    try {
      const bound = await first.runOnce();
      expect(bound.state).toBe("authorized");
      expect(storage.saved()["trustAnchorSpki"]).toBe(config.rv.publicKeySpki);
      connections[0]!.close();
      // Rust 在第二条连接收 hello 前执行真实 revoke_local；插件没有收到旧连接通知。
      next.start();
      // 完成点是握手处理后的客户端关线；Vitest 超时只作死锁护栏，不作时序判据。
      await secondClosed;
      await settle();
      expect(incoming.at(-1)).toMatchObject({
        type: "server_hello",
        state: "revoked",
        bindingId: bound.state === "authorized" ? bound.bindingId : "unreachable",
      });
      expect(next.snapshot()).toEqual({
        state: "revoked",
        bindingId: storage.saved()["revokedBindingId"],
        displayCode: null,
      });
      expect(storage.saved()).toMatchObject({
        revoked: true,
        revokedBindingId: expect.any(String),
      });
      expect(exchanges).toEqual([COOKIE]);
      expect(scheduler.delays()).toEqual([]);
      expect(connections).toHaveLength(2);
    } finally {
      next.dispose();
      connections.forEach((conn) => conn.close());
    }
  },
  15_000
);
