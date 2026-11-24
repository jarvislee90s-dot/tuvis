/**
 * Rust 专用伴随测试。入口在 src-tauri/tests/dsh_auth_v2_pending_events_test.rs。
 * 未设置桥接环境时显式 skip；禁止独立命令把 skip 说成真实 Rust 联调通过。
 * 实际 BindingSession/BindingClient + NodeIpcTransport，服务帧逐字转发、不补签。
 * 身份从空存储生成；后续复用的信任只来自第一次真实批准及 ACK。
 */
import { it, expect } from "vitest";
import {
  BindingClient,
  BindingSession,
  type BindingScheduler,
  type BindingStorage,
  type HomeBinding,
  type IpcConnection,
  type Rendezvous,
} from "../src/auth.js";
import { NodeIpcTransport } from "../src/ipc.js";

type Frame = Record<string, unknown>;
type Scenario =
  | "pending_lifecycle"
  | "authorized_ack"
  | "replay_approval"
  | "replay_ack"
  | "replay_revocation";
const COOKIE = "pending-event-fixture-memory-only-cookie";
const scenario = process.env["TUVIS_TEST_PENDING_EVENT_SCENARIO"];
const scenarios: Scenario[] = [
  "pending_lifecycle",
  "authorized_ack",
  "replay_approval",
  "replay_ack",
  "replay_revocation",
];

class Storage implements BindingStorage {
  data: string | null = null;
  async read(): Promise<string | null> { return this.data; }
  async write(data: string): Promise<void> { this.data = data; }
  saved(): Frame { return JSON.parse(this.data!) as Frame; }
}
class Scheduler implements BindingScheduler {
  private next = 0;
  private readonly jobs = new Map<number, { run: () => void; delay: number }>();
  setTimeout(run: () => void, delay: number): unknown {
    const id = ++this.next;
    this.jobs.set(id, { run, delay });
    return id;
  }
  clearTimeout(handle: unknown): void { this.jobs.delete(handle as number); }
  delays(): number[] { return [...this.jobs.values()].map((job) => job.delay); }
}

/** 下一个 recv 或 close 是生产客户端已处理上一帧的边界，绝不用 sleep 猜消费完成。 */
class ObservedConnection implements IpcConnection {
  reads = 0;
  readonly incoming: Frame[] = [];
  readonly sent: Frame[] = [];
  private readonly waiters = new Set<() => void>();
  private didClose = false;
  constructor(private readonly conn: IpcConnection) {}
  async send(line: string): Promise<void> {
    this.sent.push(JSON.parse(line) as Frame);
    await this.conn.send(line);
  }
  async recv(): Promise<string> {
    this.reads += 1;
    this.notify();
    const line = await this.conn.recv();
    this.incoming.push(JSON.parse(line) as Frame);
    return line;
  }
  close(): void {
    this.didClose = true;
    this.conn.close();
    this.notify();
  }
  get closed(): boolean { return this.didClose || this.conn.closed; }
  private notify(): void { for (const resolve of [...this.waiters]) resolve(); }
  async boundary(reads: number): Promise<void> {
    if (this.reads >= reads || this.closed) return;
    await new Promise<void>((resolve) => {
      const check = (): void => {
        if (this.reads >= reads || this.closed) {
          this.waiters.delete(check);
          resolve();
        }
      };
      this.waiters.add(check);
      check();
    });
  }
}

for (const mode of scenarios) {
  it.skipIf(!process.env["TUVIS_TEST_PENDING_EVENT_WIRE"] || scenario !== mode)(
    `Rust pending events wire: ${mode}`,
    async () => {
      const config = JSON.parse(process.env["TUVIS_TEST_PENDING_EVENT_WIRE"]!) as {
        rv: Rendezvous;
        home: HomeBinding;
        now: number;
        control: string;
      };
      const storage = new Storage();
      const scheduler = new Scheduler();
      const connections: ObservedConnection[] = [];
      const exchanges: string[] = [];
      const connectionWaiters = new Set<() => void>();
      const control = await new NodeIpcTransport().connect(config.control);
      const options = {
        storage, scheduler,
        clock: () => config.now,
        homeBinding: config.home,
        rendezvous: { async read() { return config.rv; } },
        exchange: { async exchange() { exchanges.push(COOKIE); return COOKIE; } },
        transport: {
          async connect(address: string) {
            const observed = new ObservedConnection(await new NodeIpcTransport().connect(address));
            connections.push(observed);
            for (const ready of [...connectionWaiters]) ready();
            return observed;
          },
        },
      };
      const connection = async (index: number): Promise<ObservedConnection> => {
        if (!connections[index]) {
          await new Promise<void>((resolve) => {
            const check = (): void => {
              if (connections[index]) { connectionWaiters.delete(check); resolve(); }
            };
            connectionWaiters.add(check);
            check();
          });
        }
        return connections[index]!;
      };
      const first = new BindingSession(options);
      const next = new BindingSession(options);
      const client = new BindingClient(options);
      let clientAttempt: ReturnType<BindingClient["runOnce"]> | undefined;
      try {
        expect(storage.data).toBeNull();
        if (mode === "authorized_ack") {
          clientAttempt = client.runOnce();
          const wire = await connection(0);
          await wire.boundary(2); // 授权首帧已验证，正在等待凭据 ACK。
          expect(wire.incoming[0]).toMatchObject({ type: "server_hello", state: "authorized" });
          expect(client.state).not.toBe("bound");
          expect(storage.saved()["trustAnchorSpki"]).toBeNull();
          await control.send("credential_waiting");
          expect(await clientAttempt).toMatchObject({ state: "authorized" });
          expect(client.state).toBe("bound");
          expect(wire.incoming.at(-1)).toMatchObject({
            type: "granted", event: "credential_ack", eventSignature: expect.any(String),
          });
          expect(storage.saved()["trustAnchorSpki"]).toBe(config.rv.publicKeySpki);
          expect(exchanges).toEqual([COOKIE]);
          await control.send("bound");
          return;
        }

        first.start();
        const original = await connection(0);
        await original.boundary(2); // 已消费 signed pending，并进入真实读循环。
        const pending = original.incoming[0]!;
        expect(pending).toMatchObject({ type: "server_hello", state: "pending_approval" });
        const id = pending["bindingId"];
        expect(first.snapshot()).toEqual({ state: "pending", bindingId: id, displayCode: pending["displayCode"] });
        expect(storage.saved()).toMatchObject({ trustAnchorSpki: null, revoked: false });
        expect(exchanges).toEqual([]);
        expect(original.sent.filter((frame) => frame["type"] === "credential")).toEqual([]);
        await control.send("pending");

        await original.boundary(3); // 真实 approval 已消费，credential 已发，ACK 尚被 Rust 持有。
        expect(first.state).toBe("pending");
        expect(storage.saved()["trustAnchorSpki"]).toBeNull();
        expect(exchanges).toEqual([COOKIE]);
        expect(original.incoming.at(-1)).toMatchObject({
          type: "granted", binding_id: id, event: "approval", eventSignature: expect.any(String),
        });
        expect(original.sent.filter((frame) => frame["type"] === "credential")).toEqual([
          { type: "credential", bindingId: id, cookie: COOKIE },
        ]);
        await control.send("credential_waiting");
        await original.boundary(4); // 有效 ACK 后进入持续读循环，才可声称 bound。
        expect(first.snapshot()).toEqual({ state: "bound", bindingId: id, displayCode: null });
        expect(original.incoming.at(-1)).toMatchObject({
          type: "granted", event: "credential_ack", eventSignature: expect.any(String),
        });
        expect(storage.saved()).toMatchObject({ trustAnchorSpki: config.rv.publicKeySpki, revoked: false });
        expect(scheduler.delays()).toEqual([]);
        await control.send("bound");

        if (mode === "pending_lifecycle") {
          await original.boundary(Number.POSITIVE_INFINITY); // 等实际客户端处理撤权并 close。
          expect(original.incoming.at(-1)).toMatchObject({
            type: "revoked", binding_id: id, event: "revocation", eventSignature: expect.any(String),
          });
          expect(first.snapshot()).toEqual({ state: "revoked", bindingId: id, displayCode: null });
          expect(storage.saved()).toMatchObject({ revoked: true, revokedBindingId: id });
          expect(exchanges).toEqual([COOKIE]);
          expect(scheduler.delays()).toEqual([]);
          expect(connections).toHaveLength(1);
          await control.send("revoked");
          return;
        }

        const before = storage.data;
        if (mode !== "replay_revocation") first.dispose();
        await control.send("reconnect");
        next.start();
        const current = await connection(1);
        await current.boundary(2); // 同一真实 TS 身份的新连接，首帧 authorized，已发新 credential。
        expect(current.incoming[0]).toMatchObject({ type: "server_hello", state: "authorized", bindingId: id });
        expect(current.sent[0]!["nonce"]).not.toBe(original.sent[0]!["nonce"]);
        expect(current.incoming[0]!["decisionSignature"]).not.toBe(pending["decisionSignature"]);
        expect(next.state).not.toBe("bound");
        expect(exchanges).toEqual([COOKIE, COOKIE]);
        await control.send("current_credential_waiting");

        if (mode === "replay_revocation") {
          await current.boundary(3); // 第二条连接也用真实 ACK 完成绑定，随后 Rust 对其撤权。
          expect(next.state).toBe("bound");
          await control.send("secondary_bound");
          await original.boundary(5); // 新连接的真实 revocation 被注入旧连接，必须拒绝。
          expect(original.incoming.at(-1)).toMatchObject({ type: "revoked", binding_id: id, event: "revocation" });
          expect(first.state).not.toBe("revoked");
        } else {
          await current.boundary(3); // 旧 approval/ACK 已被当前客户端消费，下一 recv 或 close。
          expect(current.incoming.at(-1)).toMatchObject({
            type: "granted", binding_id: id,
            event: mode === "replay_ack" ? "credential_ack" : "approval",
          });
          expect(next.state).not.toBe("bound");
        }
        expect(storage.data).toBe(before); // 历史合法信任保留，但重放不能写墓碑或新信任。
        expect(storage.saved()["revoked"]).toBe(false);
        expect(exchanges).toEqual([COOKIE, COOKIE]);
        expect(connections).toHaveLength(2);
        await control.send("replay_rejected");
      } finally {
        first.dispose(); next.dispose();
        connections.forEach((conn) => conn.close());
        await clientAttempt?.catch(() => undefined);
        control.close();
      }
    },
    15_000
  );
}
