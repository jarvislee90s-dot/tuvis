/**
 * H05 按实例路径隔离测试——rendezvous/凭据（identity）路径按实例段隔离。
 *
 * 夹具：真实临时目录 + 注入 PID 的路由函数 + 脚本化 Rust 侧（真 Ed25519）。
 * 锁定的行为：
 *  (a) rendezvous 候选集覆盖 legacy 单文件与 instances/<instanceId>/ 段；
 *  (b) 按宿主 PID 选定本实例的 rendezvous（零/多匹配 → null，不猜）；
 *  (c) 身份存储按 hostId（实例 ID）路由：A/B 互不混写、同实例稳定、
 *      无可选 rendezvous 时读为 null、写失败关闭（fail-closed）；
 *  (d) 握手端到端经实例路由：B 绑定后 A 的域内仍是零身份（无别名）；
 *  (e) 未来 bindingProtocol 显式拒绝（协议迁移纪律）。
 */

import { sha256Hex } from "./wire-fixtures";
import { mkdtempSync, mkdirSync, existsSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { generateKeyPairSync, sign as cryptoSign } from "node:crypto";
import { homedir, tmpdir } from "node:os";
import { join } from "node:path";
import { afterEach, describe, expect, it } from "vitest";
import { BindingClient, signingBytes, type IpcConnection, type IpcTransport } from "../src/auth.js";
import type { HomeBinding, Rendezvous } from "../src/auth.js";
import {
  deriveRendezvousPath,
  instanceFileStorage,
  instanceRendezvousReader,
  rendezvousCandidatePaths,
  selectRendezvousForPid,
} from "../src/index.js";
import { BINDING_PROTOCOL } from "../src/types.js";

// ============================================================
// 夹具（真 Ed25519 Rust 侧签名；真实临时目录）
// ============================================================

const rustKeys = generateKeyPairSync("ed25519");
const rustSpkiDer = rustKeys.publicKey.export({ type: "spki", format: "der" }) as Buffer;

function b64url(bytes: Buffer): string {
  return bytes.toString("base64").replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");
}


let cleanup: Array<() => void> = [];

afterEach(() => {
  for (const fn of cleanup.splice(0)) fn();
});

function tempRoot(): string {
  const dir = mkdtempSync(join(tmpdir(), "dsh-instance-test-"));
  cleanup.push(() => rmSync(dir, { recursive: true, force: true }));
  return dir;
}

/** 在实例段种一份 rendezvous 记录文件（instanceId = 32 hex 注册形态）。 */
function seedRendezvous(
  stateDir: string,
  instanceId: string,
  pid: number,
  hostId: string,
): Rendezvous {
  const dir = join(stateDir, "instances", instanceId);
  mkdirSync(dir, { recursive: true });
  const record: Rendezvous = {
    bindingProtocol: BINDING_PROTOCOL,
    address: `ipc-${instanceId}.sock`,
    hostId,
    epoch: "777",
    processIdentity: { platform: "test", pid, startedAtMs: 1_760_000_000_000, executable: "dsh-host" },
    publicKeySpki: b64url(rustSpkiDer),
  };
  writeFileSync(join(dir, "rendezvous.json"), JSON.stringify(record), "utf8");
  return record;
}

// ============================================================
// (a)+(b) 候选集与 PID 选定
// ============================================================

describe("H05 per-instance rendezvous routing", () => {
  it("candidate_paths_cover_legacy_and_instance_segments", () => {
    const root = tempRoot();
    const stateDir = join(root, "state");
    mkdirSync(join(stateDir, "instances", "a".repeat(32)), { recursive: true });
    mkdirSync(join(stateDir, "instances", "b".repeat(32)), { recursive: true });
    mkdirSync(join(stateDir, "instances", "not-hex"), { recursive: true });
    writeFileSync(join(stateDir, "rendezvous.json"), "{}", "utf8");
    writeFileSync(join(stateDir, "instances", "a".repeat(32), "rendezvous.json"), "{}", "utf8");

    const paths = rendezvousCandidatePaths(stateDir);
    expect(paths).toContain(join(stateDir, "rendezvous.json"));
    // legacy 单文件推导不受实例布局影响（旧单例语义逐字保留）。
    expect(deriveRendezvousPath()).toBe(join(homedir(), ".tuvis", "dsh", "rendezvous.json"));
    expect(paths).toContain(join(stateDir, "instances", "a".repeat(32), "rendezvous.json"));
    expect(paths).toContain(join(stateDir, "instances", "b".repeat(32), "rendezvous.json"));
    // 非 32 hex 注册形态的目录名不进候选（防遍历/防猜）。
    expect(paths).not.toContain(join(stateDir, "instances", "not-hex", "rendezvous.json"));
    // 缺文件不影响候选枚举（读时逐个容忍）。
    const empty = rendezvousCandidatePaths(join(root, "missing"));
    expect(empty).toHaveLength(1);
  });

  it("selects_the_record_matching_the_host_pid_and_refuses_ambiguity", () => {
    const recordA = seedRendezvous(tempRoot(), "a".repeat(32), 1001, "host-a");
    const recordB = seedRendezvous(tempRoot(), "b".repeat(32), 1002, "host-b");

    // 宿主进程（插件与宿主同进程 → process.pid 即宿主 pid）唯一对应一份记录。
    expect(selectRendezvousForPid([recordA, recordB], 1001)).toBe(recordA);
    expect(selectRendezvousForPid([recordA, recordB], 1002)).toBe(recordB);
    // 零匹配 / 多匹配 → null（fail-closed，不猜）。
    expect(selectRendezvousForPid([recordA, recordB], 9999)).toBeNull();
    expect(selectRendezvousForPid([], 1001)).toBeNull();
    const samePid: Rendezvous = { ...recordA, hostId: "another" };
    expect(selectRendezvousForPid([recordA, samePid], 1001)).toBeNull();
  });
});

// ============================================================
// (c) 身份存储按实例路由
// ============================================================

describe("H05 per-instance identity storage", () => {
  it("storage_routes_by_host_id_and_never_aliases_instances", async () => {
    const root = tempRoot();
    const dshHome = join(root, "home");
    const stateDir = join(root, "state");
    const idA = "a".repeat(32);
    const idB = "b".repeat(32);
    seedRendezvous(stateDir, idA, 2001, idA);
    seedRendezvous(stateDir, idB, 2002, idB);

    const storageA = instanceFileStorage(dshHome, stateDir, 2001);
    const storageB = instanceFileStorage(dshHome, stateDir, 2002);

    // B 写入自己的身份；A 读不到（互不可见）。
    const identityB = JSON.stringify({ version: 1, marker: "B" });
    await storageB.write(identityB);
    await expect(storageA.read()).resolves.toBeNull();
    await expect(storageB.read()).resolves.toBe(identityB);

    // 落点按 hostId 分目录：A/B 的 identity.json 互不指向。
    const fileA = join(dshHome, "tuvis-remote", idA, "identity.json");
    const fileB = join(dshHome, "tuvis-remote", idB, "identity.json");
    expect(existsSync(fileB)).toBe(true);
    expect(existsSync(fileA)).toBe(false);
    expect(JSON.parse(readFileSync(fileB, "utf8"))).toMatchObject({ marker: "B" });

    // 同实例（同 hostId）跨会话稳定：再次写入覆盖自己的文件，不产生第二份。
    await storageB.write(JSON.stringify({ version: 1, marker: "B2" }));
    expect(JSON.parse(readFileSync(fileB, "utf8"))).toMatchObject({ marker: "B2" });
    expect(existsSync(join(dshHome, "tuvis-remote", idA, "identity.json"))).toBe(false);
  });

  it("storage_without_a_selectable_rendezvous_fails_closed", async () => {
    const root = tempRoot();
    const storage = instanceFileStorage(join(root, "home"), join(root, "state"), 3001);
    // 无 rendezvous：读为 null（ revoked 检查不崩），写明确失败（绝不落到猜测路径）。
    await expect(storage.read()).resolves.toBeNull();
    await expect(storage.write("{}")).rejects.toThrow(/rendezvous/);
  });
});

// ============================================================
// (d) 握手端到端经实例路由 + (e) 协议迁移纪律
// ============================================================

class ScriptedConnection implements IpcConnection {
  readonly sent: string[] = [];
  closed = false;
  private inbound: string[] = [];
  private waiting: ((value: { line: string } | { failed: Error }) => void) | null = null;

  constructor(private readonly onClientFrame: (frame: string) => void) {}

  async send(frame: string): Promise<void> {
    if (this.closed) throw new Error("scripted ipc: closed");
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

  push(line: string): void {
    const waiter = this.waiting;
    if (waiter) waiter({ line });
    else this.inbound.push(line);
  }

  close(): void {
    this.closed = true;
    const waiter = this.waiting;
    if (waiter) waiter({ failed: new Error("scripted ipc: closed") });
  }

  get closedState(): boolean {
    return this.closed;
  }
}

function scriptedTransport(): IpcTransport & { last: ScriptedConnection | null } {
  const box: { last: ScriptedConnection | null } = { last: null };
  return {
    get last() {
      return box.last;
    },
    async connect(): Promise<IpcConnection> {
      let eventProof: { challenge: Buffer; decisionSignature: string } | null = null;
      const conn = new ScriptedConnection((frame) => {
        const hello = JSON.parse(frame) as Record<string, unknown>;
        if (hello["type"] === "credential") {
          // ACK 绑定本连接的完整挑战与决策签名，不接受跨握手证明。
          if (!eventProof) throw new Error("fixture: credential ACK requires a server hello");
          const event = "credential_ack";
          const bindingId = String(hello["bindingId"]);
          const eventBytes = Buffer.from(JSON.stringify({
            purpose: "tuvis.dsh.binding-event.v2",
            ...JSON.parse(eventProof.challenge.toString("utf8")),
            decisionSignature: eventProof.decisionSignature,
            event,
            bindingId,
          }), "utf8");
          conn.push(JSON.stringify({
            type: "granted",
            bindingId,
            event,
            eventSignature: b64url(cryptoSign(null, eventBytes, rustKeys.privateKey)),
          }));
          return;
        }
        if (hello["type"] !== "hello") return;
        // 拒绝未来协议（迁移纪律的 wire 面）：显式错误帧而非忽略。
        if (Number(hello["bindingProtocol"]) !== BINDING_PROTOCOL) {
          conn.push(JSON.stringify({ type: "error", error: "DSH_AUTH_PROTOCOL", detail: "protocol mismatch" }));
          return;
        }
        const msg = {
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
        };
        const challenge = signingBytes(msg);
        const signature = b64url(cryptoSign(null, challenge, rustKeys.privateKey));
        const decision = {
          state: "authorized",
          bindingId: "binding-instance",
          displayCode: null,
          pendingExpiresAt: null,
        };
        const decisionBytes = Buffer.from(JSON.stringify({
          purpose: "tuvis.dsh.binding-decision.v2",
          ...JSON.parse(challenge.toString("utf8")),
          ...decision,
        }), "utf8");
        const decisionSignature = b64url(cryptoSign(null, decisionBytes, rustKeys.privateKey));
        eventProof = { challenge, decisionSignature };
        conn.push(
          JSON.stringify({
            type: "server_hello",
            bindingProtocol: BINDING_PROTOCOL,
            publicKeySpki: b64url(rustSpkiDer),
            nonce: hello["nonce"],
            expiry: hello["expiry"],
            homeBinding: hello["homeBinding"],
            hostId: hello["hostId"],
            epoch: hello["epoch"],
            signature,
            decisionSignature,
            ...decision,
          }),
        );
      });
      box.last = conn;
      return conn;
    },
  };
}

describe("H05 per-instance handshake routing", () => {
  it("handshake_binds_only_the_instance_selected_by_pid_and_never_aliases", async () => {
    const root = tempRoot();
    const dshHome = join(root, "home");
    const stateDir = join(root, "state");
    const idA = "c".repeat(32);
    const idB = "d".repeat(32);
    seedRendezvous(stateDir, idA, 4001, idA);
    seedRendezvous(stateDir, idB, 4002, idB);

    // 实例 B 的客户端（PID 路由选中 B 的 rendezvous）：
    const transportB = scriptedTransport();
    const clientB = new BindingClient({
      homeBinding: { homeRoot: dshHome, hostPid: 4002, hostStartedAtMs: 1_760_000_000_000 },
      transport: transportB,
      rendezvous: { async read() { return selectRendezvousForPid(
        rendezvousCandidatePaths(stateDir)
          .map((path) => { try { return JSON.parse(readFileSync(path, "utf8")) as Rendezvous; } catch { return null; } })
          .filter((r): r is Rendezvous => r !== null),
        4002,
      ); } },
      storage: instanceFileStorage(dshHome, stateDir, 4002),
      exchange: { async exchange() { return "cookie-B"; } },
      clock: () => 1_760_000_000_000,
    });
    const outcomeB = await clientB.runOnce();
    expect(outcomeB.state).toBe("authorized");
    // hello 携带的是 B 的 hostId（路由正确性）。
    const helloB = transportB.last?.sent.map((l) => JSON.parse(l) as Record<string, unknown>).find((f) => f["type"] === "hello");
    expect(helloB?.["hostId"]).toBe(idB);
    // 身份落在 B 的实例目录。
    expect(existsSync(join(dshHome, "tuvis-remote", idB, "identity.json"))).toBe(true);

    // 实例 A 的客户端（同 root、同状态目录，PID 不同）：
    const transportA = scriptedTransport();
    const clientA = new BindingClient({
      homeBinding: { homeRoot: dshHome, hostPid: 4001, hostStartedAtMs: 1_760_000_000_000 },
      transport: transportA,
      rendezvous: { async read() { return selectRendezvousForPid(
        rendezvousCandidatePaths(stateDir)
          .map((path) => { try { return JSON.parse(readFileSync(path, "utf8")) as Rendezvous; } catch { return null; } })
          .filter((r): r is Rendezvous => r !== null),
        4001,
      ); } },
      storage: instanceFileStorage(dshHome, stateDir, 4001),
      exchange: { async exchange() { return "cookie-A"; } },
      clock: () => 1_760_000_000_000,
    });
    const outcomeA = await clientA.runOnce();
    expect(outcomeA.state).toBe("authorized");
    const helloA = transportA.last?.sent.map((l) => JSON.parse(l) as Record<string, unknown>).find((f) => f["type"] === "hello");
    expect(helloA?.["hostId"]).toBe(idA);
    // A/B 各自独立身份文件（不同客户端钥）。
    const rawA = readFileSync(join(dshHome, "tuvis-remote", idA, "identity.json"), "utf8");
    const rawB = readFileSync(join(dshHome, "tuvis-remote", idB, "identity.json"), "utf8");
    const spkiA = (JSON.parse(rawA) as { publicKeySpki: string }).publicKeySpki;
    const spkiB = (JSON.parse(rawB) as { publicKeySpki: string }).publicKeySpki;
    expect(spkiA).not.toBe(spkiB);
  });

  it("future_binding_protocol_is_refused_explicitly", async () => {
    const root = tempRoot();
    const stateDir = join(root, "state");
    const idFuture = "e".repeat(32);
    // 预期未来代际的 rendezvous（bindingProtocol = 当前 + 1）。
    const dir = join(stateDir, "instances", idFuture);
    mkdirSync(dir, { recursive: true });
    writeFileSync(
      join(dir, "rendezvous.json"),
      JSON.stringify({
        bindingProtocol: BINDING_PROTOCOL + 1,
        address: "sock",
        hostId: idFuture,
        epoch: "1",
        processIdentity: { platform: "t", pid: 5001, startedAtMs: 1, executable: "dsh" },
        publicKeySpki: b64url(rustSpkiDer),
      }),
      "utf8",
    );
    const client = new BindingClient({
      homeBinding: { homeRoot: join(root, "home"), hostPid: 5001, hostStartedAtMs: 1 },
      transport: scriptedTransport(),
      rendezvous: { async read() { return selectRendezvousForPid(
        rendezvousCandidatePaths(stateDir)
          .map((path) => { try { return JSON.parse(readFileSync(path, "utf8")) as Rendezvous; } catch { return null; } })
          .filter((r): r is Rendezvous => r !== null),
        5001,
      ); } },
      storage: instanceFileStorage(join(root, "home"), stateDir, 5001),
      exchange: { async exchange() { return "cookie"; } },
      clock: () => 1_760_000_000_000,
    });
    const outcome = await client.runOnce();
    // 插件侧显式拒绝（DSH_AUTH_PROTOCOL），不忽略、不降级续跑。
    expect(outcome.state).toBe("refused");
    if (outcome.state === "refused") expect(outcome.reason).toBe("DSH_AUTH_PROTOCOL");
    expect(existsSync(join(root, "home", "tuvis-remote", idFuture, "identity.json"))).toBe(false);
  });
});

// ============================================================
// 装配面：生产 reader 路由（process.pid 即宿主 pid 的生产形状）
// ============================================================

describe("H05 production assembly routing", () => {
  it("instance_rendezvous_reader_selects_by_the_real_host_pid", async () => {
    // 生产形状：reader 以 process.pid（插件与宿主同进程）路由本实例记录。
    const root = tempRoot();
    const stateDir = join(root, "state");
    const selfId = "f".repeat(32);
    seedRendezvous(stateDir, selfId, process.pid, selfId);
    const rv = await instanceRendezvousReader(stateDir).read();
    expect(rv?.hostId).toBe(selfId);
    // 无本进程的记录 → null（保持 idle，不猜）。
    expect(await instanceRendezvousReader(join(root, "empty")).read()).toBeNull();
    // legacy 路径推导保留（R10 语义）：状态目录 = TUVIS_DSH_STATE_DIR 或 ~/.tuvis/dsh。
    expect(deriveRendezvousPath()).toBe(join(homedir(), ".tuvis", "dsh", "rendezvous.json"));
  });
});

// ============================================================
// H05 独立审查修复回归（I3 / I4 / M1）
// ============================================================

import { renameSync, chmodSync } from "node:fs";
import type { BindingStorage } from "../src/auth.js";

/** 读 stateDir 下全部候选记录（与 reader 同源；测试观测面）。 */
function readRecords(stateDir: string): Rendezvous[] {
  return rendezvousCandidatePaths(stateDir)
    .map((path) => {
      try {
        return JSON.parse(readFileSync(path, "utf8")) as Rendezvous;
      } catch {
        return null;
      }
    })
    .filter((r): r is Rendezvous => r !== null);
}

describe("H05 review fixes: I3 legacy→instance handover", () => {
  it("retired_legacy_marker_restores_single_pid_match", async () => {
    // 「先 legacy 后选择」生命周期：同一宿主 PID 先后有两份记录。
    const root = tempRoot();
    const stateDir = join(root, "state");
    const selfId = "1".repeat(32);
    mkdirSync(join(stateDir, "instances", selfId), { recursive: true });
    // legacy 与实例记录同宿主 PID（先 legacy 后选择的交接场景）；reader 按
    // process.pid 路由，故两条记录都用本进程 PID。
    const legacyRecord: Rendezvous = {
      bindingProtocol: BINDING_PROTOCOL,
      address: "legacy-sock",
      hostId: "legacy-desktop",
      epoch: "1",
      processIdentity: { platform: "test", pid: process.pid, startedAtMs: 1, executable: "host" },
      publicKeySpki: b64url(rustSpkiDer),
    };
    const instanceRecord: Rendezvous = {
      ...seedRendezvous(stateDir, selfId, process.pid, selfId),
    };
    writeFileSync(join(stateDir, "rendezvous.json"), JSON.stringify(legacyRecord), "utf8");

    // 交接前：同 PID 双匹配 → fail-closed（插件保持 idle，不猜）。
    expect(selectRendezvousForPid(readRecords(stateDir), process.pid)).toBeNull();
    // reader（按真实宿主 process.pid 路由）在同 PID 双匹配下同样保持 idle。
    expect(await instanceRendezvousReader(stateDir).read()).toBeNull();

    // 交接（实例域上线时 Rust 改写 legacy 记录 retired=true）→ 同 PID 单匹配。
    writeFileSync(
      join(stateDir, "rendezvous.json"),
      JSON.stringify({ ...legacyRecord, retired: true }),
      "utf8",
    );
    const selected = selectRendezvousForPid(readRecords(stateDir), process.pid);
    expect(selected?.hostId).toBe(selfId);
    // reader 同口径：交接后单匹配（本进程 PID 命中实例记录）。
    expect((await instanceRendezvousReader(stateDir).read())?.hostId).toBe(selfId);
    void instanceRecord;
  });

  it("instance_records_retired_are_skipped_too", () => {
    const root = tempRoot();
    const stateDir = join(root, "state");
    const id = "2".repeat(32);
    const record = seedRendezvous(stateDir, id, 6002, id);
    // 实例记录自身被停用（例如该实例域下线交接）→ 不参与路由。
    writeFileSync(
      join(stateDir, "instances", id, "rendezvous.json"),
      JSON.stringify({ ...record, retired: true }),
      "utf8",
    );
    expect(selectRendezvousForPid(readRecords(stateDir), 6002)).toBeNull();
  });
});

describe("H05 review fixes: I4 legacy storage preserved / explicit migration", () => {
  it("legacy_record_selected_keeps_the_legacy_storage_path_verbatim", async () => {
    // 无选择（只有 legacy 记录）→ 存储走**旧 profile 路径**（逐字保留）：
    // 旧钥/信任锚/撤权墓碑原样可用，升级本身绝不触发重新授权。
    const root = tempRoot();
    const dshHome = join(root, "home");
    const stateDir = join(root, "state");
    mkdirSync(join(stateDir), { recursive: true });
    writeFileSync(
      join(stateDir, "rendezvous.json"),
      JSON.stringify({
        bindingProtocol: BINDING_PROTOCOL,
        address: "legacy-sock",
        hostId: "legacy-desktop",
        epoch: "1",
        processIdentity: { platform: "test", pid: 6101, startedAtMs: 1, executable: "host" },
        publicKeySpki: b64url(rustSpkiDer),
      }),
      "utf8",
    );
    const storage = instanceFileStorage(dshHome, stateDir, 6101);
    await storage.write(JSON.stringify({ version: 1, privateKeyPem: "PEM", publicKeySpki: "SPKI" }));
    // 落点 = 旧路径；实例槽不存在。
    const legacyFile = join(dshHome, "profiles", "desktop", "tuvis-remote", "identity.json");
    expect(existsSync(legacyFile)).toBe(true);
    expect(existsSync(join(dshHome, "tuvis-remote"))).toBe(false);
    await expect(storage.read()).resolves.toContain("PEM");
  });

  it("instance_mode_migrates_legacy_identity_explicitly_and_non_destructively", async () => {
    // 有选择（实例记录）→ 实例槽为空 + legacy 身份在场 → 显式迁移：
    // 同一把钥带进实例域；信任锚清空后由新域全新批准（TOFU）；
    // legacy 文件不动；幂等（第二次读直读实例槽）。
    // A01/A02 起迁移守卫拒绝已撤钥（含 legacy 自身 revoked=true 的墓碑，
    // 见 migrate-revocation-guard.test.ts）——本用例种干净钥测迁移正路径。
    const root = tempRoot();
    const dshHome = join(root, "home");
    const stateDir = join(root, "state");
    const id = "3".repeat(32);
    seedRendezvous(stateDir, id, 6102, id);
    const legacyDir = join(dshHome, "profiles", "desktop", "tuvis-remote");
    mkdirSync(legacyDir, { recursive: true });
    const legacyIdentity = JSON.stringify({
      version: 1,
      privateKeyPem: "LEGACY-PEM",
      publicKeySpki: "LEGACY-SPKI",
      trustAnchorSpki: "OLD-RUST-KEY",
      revoked: false,
      revokedBindingId: null,
    });
    writeFileSync(join(legacyDir, "identity.json"), legacyIdentity, "utf8");

    const storage: BindingStorage = instanceFileStorage(dshHome, stateDir, 6102);
    const migrated = await storage.read();
    expect(migrated).not.toBeNull();
    const parsed = JSON.parse(migrated ?? "{}") as Record<string, unknown>;
    expect(parsed["privateKeyPem"]).toBe("LEGACY-PEM");
    expect(parsed["publicKeySpki"]).toBe("LEGACY-SPKI");
    expect(parsed["trustAnchorSpki"]).toBeNull();
    expect(parsed["revoked"]).toBe(false);
    // 实例槽落地（幂等第二次读直读实例槽）；legacy 原样保留（非破坏）。
    const instanceFile = join(dshHome, "tuvis-remote", id, "identity.json");
    expect(existsSync(instanceFile)).toBe(true);
    expect(readFileSync(join(legacyDir, "identity.json"), "utf8")).toContain("OLD-RUST-KEY");
    const again = await storage.read();
    expect(JSON.parse(again ?? "{}")["privateKeyPem"]).toBe("LEGACY-PEM");
  });
});

describe("H05 review fixes: M1 hostId shape / directory consistency", () => {
  it("instance_record_with_mismatched_or_traversal_host_id_is_rejected", async () => {
    const root = tempRoot();
    const stateDir = join(root, "state");
    // 目录名规范，但记录 hostId 与目录不一致 → 丢弃（不猜、不 join）。
    const idA = "4".repeat(32);
    // reader 按 process.pid 路由：记录统一用本进程 PID，观察点 = reader。
    seedRendezvous(stateDir, idA, process.pid, "../escape");
    expect(await instanceRendezvousReader(stateDir).read()).toBeNull();
    // 记录 hostId = 另一个规范 ID（≠ 目录名）→ 同样丢弃（防错挂到别的实例槽）。
    seedRendezvous(stateDir, idA, process.pid, "5".repeat(32));
    expect(await instanceRendezvousReader(stateDir).read()).toBeNull();
    // 修正为一致 → 通过。
    seedRendezvous(stateDir, idA, process.pid, idA);
    expect((await instanceRendezvousReader(stateDir).read())?.hostId).toBe(idA);
    void renameSync;
    void chmodSync;
  });
});

// ============================================================
// H05 修复轮 2 回归（N2：单条畸形记录不阻塞候选读取）
// ============================================================

describe("H05 review round 2: N2 malformed record isolation", () => {
  it("malformed_hostId_record_is_skipped_without_blocking_valid_ones", async () => {
    const root = tempRoot();
    const stateDir = join(root, "state");
    const badId = "6".repeat(32);
    const goodId = "7".repeat(32);
    // 坏记录：hostId 是带 toString 属性的对象（RegExp.test 的 ToString 转换处
    // 会抛 TypeError——修复前单条坏记录阻塞全部候选读取）。
    mkdirSync(join(stateDir, "instances", badId), { recursive: true });
    writeFileSync(
      join(stateDir, "instances", badId, "rendezvous.json"),
      JSON.stringify({
        bindingProtocol: BINDING_PROTOCOL,
        address: "sock-bad",
        hostId: { toString: "broken" },
        epoch: "1",
        processIdentity: { platform: "t", pid: process.pid, startedAtMs: 1, executable: "dsh" },
        publicKeySpki: b64url(rustSpkiDer),
      }),
      "utf8",
    );
    // 好记录：同一宿主 PID、规范 hostId。
    seedRendezvous(stateDir, goodId, process.pid, goodId);

    // 读取不抛错；坏记录被跳过，好记录照常被路由选中（单条畸形不中止）。
    const rv = await instanceRendezvousReader(stateDir).read();
    expect(rv?.hostId).toBe(goodId);
    // 只有坏记录时 → fail-closed null（不抛错、不猜）。
    const root2 = tempRoot();
    mkdirSync(join(root2, "state", "instances", badId), { recursive: true });
    writeFileSync(
      join(root2, "state", "instances", badId, "rendezvous.json"),
      JSON.stringify({
        bindingProtocol: BINDING_PROTOCOL,
        address: "sock-bad",
        hostId: { toString: "broken" },
        epoch: "1",
        processIdentity: { platform: "t", pid: process.pid, startedAtMs: 1, executable: "dsh" },
        publicKeySpki: b64url(rustSpkiDer),
      }),
      "utf8",
    );
    await expect(instanceRendezvousReader(join(root2, "state")).read()).resolves.toBeNull();
  });
});
