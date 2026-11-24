/**
 * R04+R05 绑定生命周期测试（RED 先行）——稳定待批 + 单连接 + 撤权消费 + 真卸载。
 *
 * R04 症状（审查）：pending 握手返回即关连接，3 秒后重建 pending 并换新
 * bindingId/核对码；用户核对完点批准时批到的往往是已被覆盖的旧 id。
 * R05 症状：绑定成功后 socket 留着却没人读（撤权推送无人消费），循环每 3 秒
 * 再连一条（连接与任务泄漏），disposer 只置一个 stopped 标志。
 *
 * 本文件锁定的行为：
 *  (a) 待批期 display_code/bindingId 跨重连保持不变（Rust 按客户端公钥幂等是后盾）；
 *  (b) 任一时点最多一条 IPC 连接；绑定后保持该连接并**真的读**服务端推送；
 *  (c) 撤权（推送帧或 EOF+拒绝重连）→ 停止交付、停止重连（不静默换绑）；
 *  (d) disposer 关连接、清掉它创建的每个定时器；二次 dispose 是 no-op。
 *
 * 假件：脚本化 IPC 传输（真 Ed25519 服务端签名）+ 记录型调度器（可断言零泄漏）。
 */

import { sha256Hex } from "./wire-fixtures";
import { generateKeyPairSync, sign as cryptoSign } from "node:crypto";
import { describe, expect, it } from "vitest";
import {
  BindingSession,
  signingBytes,
  type BindingScheduler,
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
  homeRoot: "/lifecycle-memory-only/.dsh",
  hostPid: 1,
  hostStartedAtMs: 1_760_000_000_000,
};

function b64url(bytes: Buffer): string {
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

function serverHello(
  hello: Record<string, unknown>,
  state: string,
  bindingId: string | null,
  displayCode: string | null,
  extra: Record<string, unknown> = {},
): string {
  const challenge = challengeBytes(hello);
  const reply = {
    type: "server_hello",
    bindingProtocol: BINDING_PROTOCOL,
    publicKeySpki: b64url(rustSpkiDer),
    nonce: hello["nonce"],
    expiry: hello["expiry"],
    homeBinding: hello["homeBinding"],
    hostId: "local",
    epoch: String(hello["epoch"]),
    signature: b64url(cryptoSign(null, challenge, rustKeys.privateKey)),
    state,
    bindingId,
    displayCode,
    pendingExpiresAt: null,
    ...extra,
  };
  // 先合入记录截止等夹具字段，再签最终决策，避免到期正例携带过时签名。
  const decision = Buffer.from(JSON.stringify({
    purpose: "tuvis.dsh.binding-decision.v2",
    ...JSON.parse(challenge.toString("utf8")),
    state: reply.state,
    bindingId: reply.bindingId,
    displayCode: reply.displayCode,
    pendingExpiresAt: reply.pendingExpiresAt,
  }), "utf8");
  return JSON.stringify({
    ...reply,
    decisionSignature: b64url(cryptoSign(null, decision, rustKeys.privateKey)),
  });
}

// ============================================================
// 脚本化 IPC（连接计数 + 客户端→服务端帧记录 + 服务端主动推帧）
// ============================================================

class ScriptedConnection implements IpcConnection {
  readonly sent: string[] = [];
  closed = false;
  private inbound: string[] = [];
  private waiting: ((value: { line: string } | { failed: Error }) => void) | null = null;
  private decisionSignature: string | null = null;

  /** 下一帧 send 抛错（负向路径注入：交付时连接已坏）。 */
  failNextSend = false;

  async send(frame: string): Promise<void> {
    if (this.closed) throw new Error("scripted ipc: closed");
    if (this.failNextSend) {
      this.failNextSend = false;
      throw new Error("scripted ipc: simulated write failure");
    }
    this.sent.push(frame);
    this.onClientFrame(frame);
  }

  async recv(): Promise<string> {
    const line = this.inbound.shift();
    if (line !== undefined) return line;
    if (this.closed) throw new Error("scripted ipc: connection closed");
    return await new Promise<string>((resolve, reject) => {
      this.waiting = (result) => {
        this.waiting = null;
        if ("failed" in result) reject(result.failed);
        else resolve(result.line);
      };
    });
  }

  /** 服务端推一帧（granted/revoked）。 */
  push(line: string): void {
    const frame = JSON.parse(line) as Record<string, unknown>;
    if (frame["type"] === "server_hello") {
      this.decisionSignature = frame["decisionSignature"] as string;
    }
    const waiter = this.waiting;
    if (waiter) waiter({ line });
    else this.inbound.push(line);
  }

  /** 仅显式事件调用会签名，原有错误帧和发送失败负例保持原样。 */
  pushEvent(event: "approval" | "revocation" | "credential_ack", bindingId: string): void {
    const hello = this.hellos().at(-1);
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
    this.push(JSON.stringify({
      type: event === "revocation" ? "revoked" : "granted",
      bindingId,
      event,
      eventSignature: b64url(cryptoSign(null, bytes, rustKeys.privateKey)),
    }));
  }

  /** 服务端关线（撤权后的真实表现）；等待中的 recv 立刻失败。 */
  closeFromServer(): void {
    this.closed = true;
    const waiter = this.waiting;
    if (waiter) waiter({ failed: new Error("scripted ipc: eof") });
  }

  close(): void {
    this.closed = true;
    const waiter = this.waiting;
    if (waiter) waiter({ failed: new Error("scripted ipc: closed by client") });
  }

  hellos(): Array<Record<string, unknown>> {
    return this.sent
      .map((line) => JSON.parse(line) as Record<string, unknown>)
      .filter((frame) => frame["type"] === "hello");
  }

  credentials(): Array<Record<string, unknown>> {
    return this.sent
      .map((line) => JSON.parse(line) as Record<string, unknown>)
      .filter((frame) => frame["type"] === "credential");
  }

  constructor(private readonly onClientFrame: (frame: string) => void) {}
}

/** 记录型调度器：手动推进时间，暴露「登记未清的定时器」用于零泄漏断言。 */
class RecordingScheduler implements BindingScheduler {
  private nextId = 1;
  private readonly pending = new Map<number, { run: () => void; atMs: number }>();
  elapsedMs = 0;

  setTimeout(run: () => void, ms: number): unknown {
    const id = this.nextId++;
    this.pending.set(id, { run, atMs: this.elapsedMs + ms });
    return id;
  }

  clearTimeout(handle: unknown): void {
    this.pending.delete(handle as number);
  }

  /** 还有多少个未清的定时器（零泄漏断言用）。 */
  get outstanding(): number {
    return this.pending.size;
  }

  /** 推进时间并执行到期的回调（按到期序）。 */
  advance(ms: number): void {
    this.elapsedMs += ms;
    const due = [...this.pending.entries()]
      .filter(([, timer]) => timer.atMs <= this.elapsedMs)
      .sort((a, b) => a[1].atMs - b[1].atMs);
    for (const [id, timer] of due) {
      if (!this.pending.delete(id)) continue;
      timer.run();
    }
  }
}

function fakeStorage(): BindingStorage & { data: string | null } {
  const box: { data: string | null } = { data: null };
  return {
    get data() {
      return box.data;
    },
    set data(value: string | null) {
      box.data = value;
    },
    async read() {
      return box.data;
    },
    async write(data: string) {
      box.data = data;
    },
  };
}

function rendezvousOf(epoch = "777"): Rendezvous {
  return {
    bindingProtocol: BINDING_PROTOCOL,
    address: "scripted://tuvis-dsh-auth",
    hostId: "local",
    epoch,
    processIdentity: {
      platform: "test",
      pid: 1,
      startedAtMs: 1_760_000_000_000,
      executable: "DeepSeek Harness",
    },
    publicKeySpki: b64url(rustSpkiDer),
  };
}

const fakeExchange = (calls: string[]): CookieExchange => ({
  async exchange() {
    calls.push("exchange");
    return "owner-cookie-from-connect";
  },
});

/** 让异步循环跑到下一个等待点（无真实定时器的忙等短轮转）。 */
async function settle(times = 24): Promise<void> {
  for (let i = 0; i < times; i++) await Promise.resolve();
}

// ============================================================
// (a)+(b) 稳定待批与单连接
// ============================================================

describe("R04 稳定待批生命周期", () => {
  it("pending_code_survives_a_dropped_connection_without_churn", async () => {
    const connections: ScriptedConnection[] = [];
    // 假 Rust：同钥幂等——两次 hello 都拿到同一对 id/码（与 Rust 实现一致）。
    const transport: IpcTransport = {
      async connect() {
        const conn = new ScriptedConnection((frame) => {
          const parsed = JSON.parse(frame) as Record<string, unknown>;
          if (parsed["type"] !== "hello") return;
          conn.push(serverHello(parsed, "pending_approval", "binding-1", "AB23CD"));
          // 待批连接在用户操作前不关：第一代随后模拟「网络掉线」由测试侧关。
        });
        connections.push(conn);
        return conn;
      },
    };
    const exchanges: string[] = [];
    const scheduler = new RecordingScheduler();
    const session = new BindingSession({
      homeBinding,
      transport,
      rendezvous: { read: async () => rendezvousOf() },
      storage: fakeStorage(),
      exchange: fakeExchange(exchanges),
      clock: () => 1_760_000_000_000,
      scheduler,
    });

    session.start();
    await settle();

    expect(session.state).toBe("pending");
    expect(session.snapshot()).toEqual({
      state: "pending",
      bindingId: "binding-1",
      displayCode: "AB23CD",
    });
    // 待批连接被保留（不是「握手回完就关」）
    expect(connections).toHaveLength(1);
    expect(connections[0]!.closed).toBe(false);
    // 待批期绝不兑换、绝不交付凭据
    expect(exchanges).toEqual([]);
    expect(connections[0]!.credentials()).toEqual([]);

    // 掉线 → 退避到期后重连一次，同一对 id/码保持不变（Rust 幂等是后盾）
    connections[0]!.closeFromServer();
    await settle();
    scheduler.advance(5_000);
    await settle();

    expect(connections).toHaveLength(2);
    expect(session.snapshot()).toEqual({
      state: "pending",
      bindingId: "binding-1",
      displayCode: "AB23CD",
    });

    session.dispose();
  });

  it("approval_on_the_pending_connection_binds_without_a_new_socket", async () => {
    let connection: ScriptedConnection | undefined;
    const transport: IpcTransport = {
      async connect() {
        const conn = new ScriptedConnection((frame) => {
          const parsed = JSON.parse(frame) as Record<string, unknown>;
          if (parsed["type"] === "credential") {
            conn.pushEvent("credential_ack", String(parsed["bindingId"]));
            return;
          }
          if (parsed["type"] === "hello") {
            conn.push(serverHello(parsed, "pending_approval", "binding-2", "XY45ZV"));
          }
        });
        connection = conn;
        return conn;
      },
    };
    const exchanges: string[] = [];
    const session = new BindingSession({
      homeBinding,
      transport,
      rendezvous: { read: async () => rendezvousOf() },
      storage: fakeStorage(),
      exchange: fakeExchange(exchanges),
      clock: () => 1_760_000_000_000,
      scheduler: new RecordingScheduler(),
    });

    session.start();
    await settle();
    expect(session.state).toBe("pending");

    // 用户在兔维斯点批准：Rust 向**同一条待批连接**推 Granted
    connection!.pushEvent("approval", "binding-2");
    await settle();

    expect(session.state).toBe("bound");
    expect(session.snapshot().bindingId).toBe("binding-2");
    expect(exchanges).toEqual(["exchange"]);
    expect(connection!.credentials()).toHaveLength(1);
    // 全程一条连接（不是「待批一条 + 绑定一条」）
    expect(connection!.closed).toBe(false);

    session.dispose();
  });
});

describe("R05 单连接与撤权消费", () => {
  it("bound_phase_holds_one_connection_and_consumes_the_revoke_push", async () => {
    const connections: ScriptedConnection[] = [];
    const transport: IpcTransport = {
      async connect() {
        const conn = new ScriptedConnection((frame) => {
          const parsed = JSON.parse(frame) as Record<string, unknown>;
          if (parsed["type"] === "credential") {
            conn.pushEvent("credential_ack", String(parsed["bindingId"]));
            return;
          }
          if (parsed["type"] === "hello") {
            conn.push(serverHello(parsed, "authorized", "binding-3", null));
          }
        });
        connections.push(conn);
        return conn;
      },
    };
    const exchanges: string[] = [];
    const scheduler = new RecordingScheduler();
    const session = new BindingSession({
      homeBinding,
      transport,
      rendezvous: { read: async () => rendezvousOf() },
      storage: fakeStorage(),
      exchange: fakeExchange(exchanges),
      clock: () => 1_760_000_000_000,
      scheduler,
    });

    session.start();
    await settle();
    expect(session.state).toBe("bound");
    // 绑定后 = 恰好一条连接，且连接仍在（读循环持有）
    expect(connections).toHaveLength(1);
    expect(connections[0]!.closed).toBe(false);

    // 时间推进不再制造新连接（旧实现每 3 秒重连一条）
    scheduler.advance(30_000);
    await settle();
    expect(connections).toHaveLength(1);

    // 撤权：Rust 推 Revoked 后关线
    connections[0]!.pushEvent("revocation", "binding-3");
    connections[0]!.closeFromServer();
    await settle();

    expect(session.state).toBe("revoked");
    expect(session.snapshot()).toEqual({
      state: "revoked",
      bindingId: "binding-3",
      displayCode: null,
    });

    // 撤权后停止重连（不静默换绑），也不再兑换/交付
    scheduler.advance(60_000);
    await settle();
    expect(connections).toHaveLength(1);
    expect(exchanges).toEqual(["exchange"]);
    for (const conn of connections) expect(conn.credentials()).toHaveLength(1);

    session.dispose();
  });

  it("revoked_refusal_after_eof_stops_reconnecting", async () => {
    let attempt = 0;
    const connections: ScriptedConnection[] = [];
    const transport: IpcTransport = {
      async connect() {
        attempt += 1;
        const conn = new ScriptedConnection((frame) => {
          const parsed = JSON.parse(frame) as Record<string, unknown>;
          if (parsed["type"] !== "hello") return;
          if (attempt === 1) {
            // 第一代：Rust 用签名决策确认本钥已撤权，插件才落盘并停止重连。
            conn.push(serverHello(parsed, "revoked", "binding-revoked", null));
          }
        });
        connections.push(conn);
        return conn;
      },
    };
    const scheduler = new RecordingScheduler();
    const session = new BindingSession({
      homeBinding,
      transport,
      rendezvous: { read: async () => rendezvousOf() },
      storage: fakeStorage(),
      exchange: fakeExchange([]),
      clock: () => 1_760_000_000_000,
      scheduler,
    });

    session.start();
    await settle();

    expect(session.state).toBe("revoked");
    scheduler.advance(120_000);
    await settle();
    expect(attempt).toBe(1);
    // 撤权落盘（重启后仍不交付）
    session.dispose();
  });

  it("eof_then_peer_revoked_rehello_transitions_to_revoked_and_stops", async () => {
    // 撤权的真实到达顺序之一：Rust 关连接（EOF，无推送帧），插件重连一次，
    // 服务端以签名的 server_hello(state=revoked) 确认 → 进撤权态、不再重连。
    // 未批准仍可恢复；未认证的首帧错误码本身不能触发永久墓碑。
    // 这里保留「卸载撤权 → EOF → 重连 → 永久拒绝」的全部行为断言。
    let attempt = 0;
    const connections: ScriptedConnection[] = [];
    const transport: IpcTransport = {
      async connect() {
        attempt += 1;
        const nth = attempt;
        const conn = new ScriptedConnection((frame) => {
          const parsed = JSON.parse(frame) as Record<string, unknown>;
          if (parsed["type"] === "credential") {
            conn.pushEvent("credential_ack", String(parsed["bindingId"]));
            return;
          }
          if (parsed["type"] !== "hello") return;
          if (nth === 1) {
            conn.push(serverHello(parsed, "authorized", "binding-5", null));
          } else {
            conn.push(serverHello(parsed, "revoked", "binding-5", null));
          }
        });
        connections.push(conn);
        return conn;
      },
    };
    const storage = fakeStorage();
    const scheduler = new RecordingScheduler();
    const session = new BindingSession({
      homeBinding,
      transport,
      rendezvous: { read: async () => rendezvousOf() },
      storage,
      exchange: fakeExchange([]),
      clock: () => 1_760_000_000_000,
      scheduler,
    });

    session.start();
    await settle();
    expect(session.state).toBe("bound");

    // 撤权：服务端直接关线（不带 Revoked 帧）
    connections[0]!.closeFromServer();
    await settle();
    scheduler.advance(5_000); // 退避到期 → 重连一次
    await settle();

    expect(attempt).toBe(2);
    expect(session.state).toBe("revoked");
    // 撤权落盘（重启后仍不交付），且不再重连
    expect(JSON.parse(storage.data ?? "{}")["revoked"]).toBe(true);
    scheduler.advance(300_000);
    await settle();
    expect(attempt).toBe(2);

    session.dispose();
  });

  it("disposer_closes_the_socket_and_leaves_no_timer_behind", async () => {
    const connections: ScriptedConnection[] = [];
    const transport: IpcTransport = {
      async connect() {
        const conn = new ScriptedConnection((frame) => {
          const parsed = JSON.parse(frame) as Record<string, unknown>;
          if (parsed["type"] === "hello") {
            conn.push(serverHello(parsed, "pending_approval", "binding-4", "QQ11WW"));
          }
        });
        connections.push(conn);
        return conn;
      },
    };
    const scheduler = new RecordingScheduler();
    const session = new BindingSession({
      homeBinding,
      transport,
      rendezvous: { read: async () => rendezvousOf() },
      storage: fakeStorage(),
      exchange: fakeExchange([]),
      clock: () => 1_760_000_000_000,
      scheduler,
    });

    session.start();
    await settle();
    expect(connections).toHaveLength(1);

    session.dispose();
    await settle();

    // 套接字关闭 + 定时器全部取消
    expect(connections[0]!.closed).toBe(true);
    expect(scheduler.outstanding).toBe(0);

    // 二次 dispose 是 no-op，且不会再开连接
    expect(() => session.dispose()).not.toThrow();
    scheduler.advance(60_000);
    await settle();
    expect(connections).toHaveLength(1);
    expect(scheduler.outstanding).toBe(0);
  });

  it("no_rendezvous_means_idle_without_connecting", async () => {
    let connects = 0;
    const scheduler = new RecordingScheduler();
    const session = new BindingSession({
      homeBinding,
      transport: {
        async connect() {
          connects += 1;
          throw new Error("must not connect");
        },
      },
      rendezvous: { read: async () => null },
      storage: fakeStorage(),
      exchange: fakeExchange([]),
      clock: () => 1_760_000_000_000,
      scheduler,
    });

    session.start();
    await settle();

    expect(connects).toBe(0);
    expect(session.snapshot()).toEqual({ state: "idle", bindingId: null, displayCode: null });

    session.dispose();
    expect(scheduler.outstanding).toBe(0);
  });
});

// ============================================================
// K5 (M-2) 未批准 ≠ 永久撤权；K6 (M-4) 兑换失败不得假显已绑定
// ============================================================
//
// M-2：`DSH_AUTH_NOT_APPROVED`（绑定尚未批准 / 并发替换 / 待批记录过期）被当成
// 与 `DSH_PEER_REVOKED`（永久）同档：插件把自己钉成 revoked、写 identity.json、
// 此后永不重连，而 `resetIdentity()` 没有生产调用点——一次竞争就能让插件永久停摆。
//
// 契约状态机：
//   DSH_PEER_REVOKED      → revoked（永久；停止兑换/交付/重连，落盘墓碑）
//   DSH_AUTH_NOT_APPROVED → 退回 pending/idle + 退避重连（**不写 revoked**），
//                           凭据交付路径保持可用（下一次批准/重连即可继续）
//
// M-4：快照在 `deliverCredential` **之前**就被置成 bound，而该函数的两个
// `catch { return; }` 吞掉兑换/IPC 失败——兔维斯卡显示「已绑定」，Rust 侧却
// 没有任何 cookie。契约：交付失败必须回退快照（pending），两端都以 Rust 真值为准。

/** 只回 pending、之后按脚本推进的假 Rust（K5/K6 共用脚手架）。 */
function scriptedPendingTransport(onHello: (conn: ScriptedConnection) => void): {
  transport: IpcTransport;
  connections: ScriptedConnection[];
} {
  const connections: ScriptedConnection[] = [];
  const transport: IpcTransport = {
    async connect() {
      const conn = new ScriptedConnection((frame) => {
        const parsed = JSON.parse(frame) as Record<string, unknown>;
        if (parsed["type"] === "hello") onHello(conn);
      });
      connections.push(conn);
      return conn;
    },
  };
  return { transport, connections };
}

describe("K5 未批准与已撤权必须区分（M-2）", () => {
  it("not_approved_keeps_retrying_and_never_writes_a_revoked_tombstone", async () => {
    let attempt = 0;
    const transport: IpcTransport = {
      async connect() {
        attempt += 1;
        const conn = new ScriptedConnection((frame) => {
          const parsed = JSON.parse(frame) as Record<string, unknown>;
          if (parsed["type"] !== "hello") return;
          // 服务端说「本钥尚未批准 / 待批记录已被替换」——可恢复，不是撤权。
          conn.push(
            JSON.stringify({
              type: "error",
              error: "DSH_AUTH_NOT_APPROVED",
              detail: "binding is not approved yet",
            }),
          );
        });
        return conn;
      },
    };
    const storage = fakeStorage();
    const scheduler = new RecordingScheduler();
    const session = new BindingSession({
      homeBinding,
      transport,
      rendezvous: { read: async () => rendezvousOf() },
      storage,
      exchange: fakeExchange([]),
      clock: () => 1_760_000_000_000,
      scheduler,
    });

    session.start();
    await settle();

    // 不得把自己钉成撤权态（旧实现写 revoked 并永久停摆）
    expect(session.state).not.toBe("revoked");
    const persisted = JSON.parse(storage.data ?? "{}") as Record<string, unknown>;
    expect(persisted["revoked"]).not.toBe(true);
    // 退避后重试（不是一次就放弃）
    expect(attempt).toBe(1);
    scheduler.advance(5_000);
    await settle();
    expect(attempt).toBe(2);
    // 反复未批准也不得变成撤权
    expect(session.state).not.toBe("revoked");

    session.dispose();
  });

  it("not_approved_after_a_grant_attempt_returns_to_pending_instead_of_revoked", async () => {
    // 待批连接上用户批准 → 插件兑换并交付 → 服务端回「未批准」（并发替换/过期）。
    const connections: ScriptedConnection[] = [];
    const transport: IpcTransport = {
      async connect() {
        const conn = new ScriptedConnection((frame) => {
          const parsed = JSON.parse(frame) as Record<string, unknown>;
          if (parsed["type"] === "credential") {
            conn.push(
              JSON.stringify({
                type: "error",
                error: "DSH_AUTH_NOT_APPROVED",
                detail: "binding was replaced concurrently",
              }),
            );
            return;
          }
          if (parsed["type"] !== "hello") return;
          conn.push(serverHello(parsed, "pending_approval", "binding-k5b", "CD34EF"));
        });
        connections.push(conn);
        return conn;
      },
    };
    const storage = fakeStorage();
    const scheduler = new RecordingScheduler();
    const session = new BindingSession({
      homeBinding,
      transport,
      rendezvous: { read: async () => rendezvousOf() },
      storage,
      exchange: fakeExchange([]),
      clock: () => 1_760_000_000_000,
      scheduler,
    });

    session.start();
    await settle();
    expect(session.state).toBe("pending");

    // 用户批准 → 插件兑换交付 → 服务端答「未批准」（并发替换/待批过期）。
    connections[0]!.pushEvent("approval", "binding-k5b");
    await settle(48);

    // 可恢复：不进撤权、不落盘墓碑、快照退回待批
    expect(session.state).not.toBe("revoked");
    expect(session.state).toBe("pending");
    const persisted = JSON.parse(storage.data ?? "{}") as Record<string, unknown>;
    expect(persisted["revoked"]).not.toBe(true);

    // 连接真掉线后仍会重连（可恢复路径没有被自己掐断）
    connections[0]!.closeFromServer();
    await settle();
    scheduler.advance(60_000);
    await settle();
    expect(connections.length).toBeGreaterThan(1);
    expect(session.state).not.toBe("revoked");

    session.dispose();
  });

  it("peer_revoked_still_stops_permanently", async () => {
    let attempt = 0;
    const transport: IpcTransport = {
      async connect() {
        attempt += 1;
        const conn = new ScriptedConnection((frame) => {
          const parsed = JSON.parse(frame) as Record<string, unknown>;
          if (parsed["type"] !== "hello") return;
          conn.push(serverHello(parsed, "revoked", "binding-k5-revoked", null));
        });
        return conn;
      },
    };
    const storage = fakeStorage();
    const scheduler = new RecordingScheduler();
    const session = new BindingSession({
      homeBinding,
      transport,
      rendezvous: { read: async () => rendezvousOf() },
      storage,
      exchange: fakeExchange([]),
      clock: () => 1_760_000_000_000,
      scheduler,
    });

    session.start();
    await settle();

    expect(session.state).toBe("revoked");
    // 永久语义保留：落盘墓碑 + 不再重连
    expect((JSON.parse(storage.data ?? "{}") as Record<string, unknown>)["revoked"]).toBe(true);
    scheduler.advance(300_000);
    await settle();
    expect(attempt).toBe(1);

    session.dispose();
  });
});

describe("K6 兑换/交付失败不得假显已绑定（M-4）", () => {
  it("failed_exchange_after_grant_does_not_report_bound", async () => {
    const connections: ScriptedConnection[] = [];
    const transport: IpcTransport = {
      async connect() {
        const conn = new ScriptedConnection((frame) => {
          const parsed = JSON.parse(frame) as Record<string, unknown>;
          if (parsed["type"] !== "hello") return;
          conn.push(serverHello(parsed, "pending_approval", "binding-k6", "GH56IJ"));
        });
        connections.push(conn);
        return conn;
      },
    };
    const failingExchange: CookieExchange = {
      async exchange() {
        throw new Error('dsh /connect exchange failed at stage "connect-status"');
      },
    };
    const scheduler = new RecordingScheduler();
    const session = new BindingSession({
      homeBinding,
      transport,
      rendezvous: { read: async () => rendezvousOf() },
      storage: fakeStorage(),
      exchange: failingExchange,
      clock: () => 1_760_000_000_000,
      scheduler,
    });

    session.start();
    await settle();
    expect(session.state).toBe("pending");

    // 用户批准 → 插件兑换失败（Rust 侧没有 cookie）
    connections[0]!.pushEvent("approval", "binding-k6");
    await settle(48);

    // Rust 真值是「无凭据」⇒ 快照不得报 bound（旧实现先置 bound 再吞异常）
    expect(session.snapshot().state).not.toBe("bound");
    expect(session.snapshot().state).toBe("pending");
    expect(connections[0]!.credentials()).toEqual([]);

    session.dispose();
  });

  it("failed_credential_send_after_grant_does_not_report_bound", async () => {
    const connections: ScriptedConnection[] = [];
    const transport: IpcTransport = {
      async connect() {
        const conn = new ScriptedConnection((frame) => {
          const parsed = JSON.parse(frame) as Record<string, unknown>;
          if (parsed["type"] !== "hello") return;
          conn.push(serverHello(parsed, "pending_approval", "binding-k6c", "OP90QR"));
        });
        connections.push(conn);
        return conn;
      },
    };
    const storage = fakeStorage();
    const session = new BindingSession({
      homeBinding,
      transport,
      rendezvous: { read: async () => rendezvousOf() },
      storage,
      exchange: fakeExchange([]),
      clock: () => 1_760_000_000_000,
      scheduler: new RecordingScheduler(),
    });

    session.start();
    await settle();
    expect(session.state).toBe("pending");

    // 让这条连接的 send 在交付时抛错（连接已坏），随后再推批准帧。
    const conn = connections[0]!;
    conn.failNextSend = true;
    conn.pushEvent("approval", "binding-k6c");
    await settle(48);

    expect(session.snapshot().state).not.toBe("bound");

    session.dispose();
  });

  it("successful_delivery_still_marks_bound", async () => {
    const connections: ScriptedConnection[] = [];
    const transport: IpcTransport = {
      async connect() {
        const conn = new ScriptedConnection((frame) => {
          const parsed = JSON.parse(frame) as Record<string, unknown>;
          if (parsed["type"] === "credential") {
            conn.pushEvent("credential_ack", String(parsed["bindingId"]));
            return;
          }
          if (parsed["type"] !== "hello") return;
          conn.push(serverHello(parsed, "pending_approval", "binding-k6b", "KL78MN"));
        });
        connections.push(conn);
        return conn;
      },
    };
    const exchanges: string[] = [];
    const session = new BindingSession({
      homeBinding,
      transport,
      rendezvous: { read: async () => rendezvousOf() },
      storage: fakeStorage(),
      exchange: fakeExchange(exchanges),
      clock: () => 1_760_000_000_000,
      scheduler: new RecordingScheduler(),
    });

    session.start();
    await settle();
    expect(session.state).toBe("pending");

    connections[0]!.pushEvent("approval", "binding-k6b");
    await settle(48);

    expect(session.state).toBe("bound");
    expect(session.snapshot()).toEqual({
      state: "bound",
      bindingId: "binding-k6b",
      displayCode: null,
    });
    expect(exchanges).toEqual(["exchange"]);

    session.dispose();
  });
});

// ============================================================
// P1-①（2026-10-30 第一阶段复审 T08 前门槛）：待批 60 秒到期自动断开重握手
// ============================================================

describe("P1-① 待批到期生命周期", () => {
  it("pending_expiry_closes_and_rehandshakes_with_a_fresh_code", async () => {
    const connections: ScriptedConnection[] = [];
    let seq = 0;
    const transport: IpcTransport = {
      async connect() {
        seq += 1;
        const bindingId = `binding-${seq}`;
        const displayCode = `CODE${seq}X`;
        const conn = new ScriptedConnection((frame) => {
          const parsed = JSON.parse(frame) as Record<string, unknown>;
          if (parsed["type"] !== "hello") return;
          conn.push(serverHello(parsed, "pending_approval", bindingId, displayCode));
        });
        connections.push(conn);
        return conn;
      },
    };
    const scheduler = new RecordingScheduler();
    const session = new BindingSession({
      homeBinding,
      transport,
      rendezvous: { async read() { return rendezvousOf(); } },
      storage: fakeStorage(),
      exchange: fakeExchange([]),
      clock: () => scheduler.elapsedMs,
      scheduler,
    });
    session.start();
    await settle();
    expect(session.snapshot()).toMatchObject({
      state: "pending",
      bindingId: "binding-1",
      displayCode: "CODE1X",
    });

    // 跨过 60 秒挑战窗口：到期定时器必须关旧连接并重握手拿新码。
    scheduler.advance(61_000);
    await settle();
    scheduler.advance(2_000);
    await settle();

    expect(connections[0].closed).toBe(true);
    expect(connections.length).toBe(2);
    const snap = session.snapshot();
    expect(snap.state).toBe("pending");
    expect(snap.bindingId).toBe("binding-2");
    expect(snap.displayCode).toBe("CODE2X");
    session.dispose();
    expect(scheduler.outstanding).toBe(0);
  });

  it("approval_before_expiry_keeps_the_bound_connection_alive", async () => {
    // 复审 Blocker-1：待批到期定时器在 readLoop **之前**武装，而批准发生在
    // readLoop **之内**（granted → 交付凭据 → 快照翻 bound，连接保留）。旧实现只在
    // readLoop 返回后才清定时器——于是 T+60s 定时器到点仍会关掉一条**活着的已绑定
    // 连接**，制造无谓的重握手 + 重新兑换（Rust 侧看到连接抖动）。
    const connections: ScriptedConnection[] = [];
    const transport: IpcTransport = {
      async connect() {
        const conn = new ScriptedConnection((frame) => {
          const parsed = JSON.parse(frame) as Record<string, unknown>;
          if (parsed["type"] === "credential") {
            conn.pushEvent("credential_ack", String(parsed["bindingId"]));
            return;
          }
          if (parsed["type"] === "hello") {
            conn.push(serverHello(parsed, "pending_approval", "binding-1", "AB23CD"));
          }
        });
        connections.push(conn);
        return conn;
      },
    };
    const exchanges: string[] = [];
    const scheduler = new RecordingScheduler();
    const session = new BindingSession({
      homeBinding,
      transport,
      rendezvous: { async read() { return rendezvousOf(); } },
      storage: fakeStorage(),
      exchange: fakeExchange(exchanges),
      clock: () => scheduler.elapsedMs,
      scheduler,
    });

    session.start();
    await settle();
    expect(session.snapshot()).toMatchObject({ state: "pending", bindingId: "binding-1" });

    // 用户在到期前批准：同一连接上交付凭据 → bound（连接保留、进入读循环）。
    connections[0]!.pushEvent("approval", "binding-1");
    await settle(48);
    expect(session.state).toBe("bound");
    expect(connections).toHaveLength(1);
    expect(connections[0]!.closed).toBe(false);

    // 跨过原待批到期点：活着的已绑定连接**不得**被到期定时器关掉。
    scheduler.advance(61_000);
    await settle(48);
    expect(connections[0]!.closed).toBe(false);
    expect(connections).toHaveLength(1);
    expect(session.state).toBe("bound");
    expect(exchanges).toEqual(["exchange"]);

    session.dispose();
    expect(scheduler.outstanding).toBe(0);
  });

  it("first_reconnect_after_a_pending_drop_uses_the_base_backoff", async () => {
    // 复审 Blocker-2：`bound`/`pending` 分支各自又排了一次重连，与 readLoop 尾部的
    // 排程重复 → backoffStep 双步进，**首次**重连要等 2s 而不是 1s。这里把基值钉死：
    // 待批连接掉线后恰好 1s（不是 2s）就该发起第一次重连。
    const connections: ScriptedConnection[] = [];
    const transport: IpcTransport = {
      async connect() {
        const conn = new ScriptedConnection((frame) => {
          const parsed = JSON.parse(frame) as Record<string, unknown>;
          if (parsed["type"] !== "hello") return;
          conn.push(serverHello(parsed, "pending_approval", "binding-1", "AB23CD"));
        });
        connections.push(conn);
        return conn;
      },
    };
    const scheduler = new RecordingScheduler();
    const session = new BindingSession({
      homeBinding,
      transport,
      rendezvous: { async read() { return rendezvousOf(); } },
      storage: fakeStorage(),
      exchange: fakeExchange([]),
      clock: () => scheduler.elapsedMs,
      scheduler,
    });
    session.start();
    await settle();
    expect(connections).toHaveLength(1);

    // 服务端掉线（未到期）→ 退避重连。
    connections[0]!.closeFromServer();
    await settle();

    // 基值 1s 到点即重连（重复排程的旧实现要等 2s）。
    scheduler.advance(1_000);
    await settle();
    expect(connections).toHaveLength(2);
    expect(session.state).toBe("pending");

    session.dispose();
    expect(scheduler.outstanding).toBe(0);
  });

  it("pending_expiry_timer_honours_the_records_deadline_not_the_echoed_expiry", async () => {
    // F3（复审 Important-3）：T+55s 重连的应答里 `expiry` 是**本次挑战**的新到期
    // （now+60s），而记录的真实审批截止仍是原始那一个。若定时器只看 `expiry`，
    // 这个「实际 5 秒后就批不了」的码会显示成还能活 60 秒。契约：取两者较早者。
    const connections: ScriptedConnection[] = [];
    const transport: IpcTransport = {
      async connect() {
        const conn = new ScriptedConnection((frame) => {
          const parsed = JSON.parse(frame) as Record<string, unknown>;
          if (parsed["type"] !== "hello") return;
          // 本次挑战 expiry = now + 60s；记录真实到期 = now + 10s。
          conn.push(
            serverHello(parsed, "pending_approval", "binding-1", "AB23CD", {
              pendingExpiresAt: scheduler.elapsedMs + 10_000,
            }),
          );
        });
        connections.push(conn);
        return conn;
      },
    };
    const scheduler = new RecordingScheduler();
    const session = new BindingSession({
      homeBinding,
      transport,
      rendezvous: { async read() { return rendezvousOf(); } },
      storage: fakeStorage(),
      exchange: fakeExchange([]),
      clock: () => scheduler.elapsedMs,
      scheduler,
    });
    session.start();
    await settle();
    expect(session.state).toBe("pending");
    expect(connections).toHaveLength(1);

    // 走到记录截止之后（但远未到本轮挑战的 60s）：必须已关线并重握手。
    scheduler.advance(11_000);
    await settle();
    expect(connections[0]!.closed).toBe(true);
    scheduler.advance(2_000);
    await settle();
    expect(connections.length).toBeGreaterThanOrEqual(2);

    session.dispose();
    expect(scheduler.outstanding).toBe(0);
  });
});

describe("F3 待批应答携带记录真实到期", () => {
  it("fallback_to_expiry_when_the_record_deadline_is_absent", async () => {
    // 无记录截止时，v2 显式携带并签名 pendingExpiresAt=null，仍回退到本轮 expiry。
    const connections: ScriptedConnection[] = [];
    const transport: IpcTransport = {
      async connect() {
        const conn = new ScriptedConnection((frame) => {
          const parsed = JSON.parse(frame) as Record<string, unknown>;
          if (parsed["type"] !== "hello") return;
          conn.push(serverHello(parsed, "pending_approval", "binding-1", "AB23CD"));
        });
        connections.push(conn);
        return conn;
      },
    };
    const scheduler = new RecordingScheduler();
    const session = new BindingSession({
      homeBinding,
      transport,
      rendezvous: { async read() { return rendezvousOf(); } },
      storage: fakeStorage(),
      exchange: fakeExchange([]),
      clock: () => scheduler.elapsedMs,
      scheduler,
    });
    session.start();
    await settle();

    // 未到 60s 不关线（退回 expiry 语义）。
    scheduler.advance(30_000);
    await settle();
    expect(connections[0]!.closed).toBe(false);
    // 到 60s 才关。
    scheduler.advance(31_000);
    await settle();
    expect(connections[0]!.closed).toBe(true);

    session.dispose();
    expect(scheduler.outstanding).toBe(0);
  });
});
