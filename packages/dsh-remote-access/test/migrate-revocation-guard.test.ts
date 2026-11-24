/**
 * A01/A02 回归：已迁入 legacy 钥的 Web 实例退役（撤权）后重装，不得再次迁入
 * 同一把已撤钥；其他实例的身份不受影响。
 *
 * 根因（controller 源码实锤）：`migrateLegacyIdentity`（src/index.ts）把 legacy
 * 身份带进实例域时不查任何撤权证据——
 *  ① legacy 文件自身的 `revoked` 墓碑不被检查；
 *  ② 实例域的撤权完全不可见：Web 实例曾用迁移钥完成授权、随后被退役（撤权翻
 *     墓碑，账本 = `<stateDir>/instances/<hostId>/bindings.json` 里
 *     `state: "revoked"` 且 `revokedKeySpki` 即被撤钥），重装时实例槽为空 →
 *     同一把钥以 `revoked: false` 再次迁入 → 已撤钥复活。
 *
 * 验收走**真实插件存储链**（不手工换钥）：真实形态的 identity.json（旧 profile
 * 存储路径）、bindings.json（Rust serde wire 形态：camelCase 字段 + snake_case
 * state；撤权墓碑行 = 钥移入 `revokedKeySpki`、`publicKeySpki` 清空）、
 * rendezvous 记录文件；经 `instanceFileStorage().read()` 触发迁移守卫。
 * 迁移被拒 = `read()` 为 null = BindingClient 的全新生成路径（TOFU pending
 * 重新批准），插件启动不得因此抛错卡死。
 */

import { existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { generateKeyPairSync } from "node:crypto";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { afterEach, describe, expect, it } from "vitest";
import { instanceFileStorage } from "../src/index.js";
import { BINDING_PROTOCOL } from "../src/types.js";
import type { Rendezvous } from "../src/auth.js";

// ============================================================
// 夹具（真实临时目录 + 真实 Ed25519 钥 + Rust wire 形态账本）
// ============================================================

let cleanup: Array<() => void> = [];

afterEach(() => {
  for (const fn of cleanup.splice(0)) fn();
});

function tempRoot(): string {
  const dir = mkdtempSync(join(tmpdir(), "dsh-migrate-guard-test-"));
  cleanup.push(() => rmSync(dir, { recursive: true, force: true }));
  return dir;
}

function b64url(bytes: Buffer): string {
  return bytes.toString("base64").replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");
}

/** 一把真实 Ed25519 钥（PKCS#8 PEM + SPKI DER base64url），与生产写入链同形。 */
function ed25519Key(): { pem: string; spki: string } {
  const { publicKey, privateKey } = generateKeyPairSync("ed25519");
  return {
    pem: privateKey.export({ type: "pkcs8", format: "pem" }) as string,
    spki: b64url(publicKey.export({ type: "spki", format: "der" }) as Buffer),
  };
}

const INSTANCE_A = "a".repeat(32); // 实例 ID 注册形态（32 位小写十六进制）
const INSTANCE_B = "b".repeat(32);

function identityFileOf(dshHome: string, instanceId: string): string {
  return join(dshHome, "tuvis-remote", instanceId, "identity.json");
}

function legacyFileOf(dshHome: string): string {
  return join(dshHome, "profiles", "desktop", "tuvis-remote", "identity.json");
}

/** 种实例 rendezvous 记录（hostId == 目录名；与 instance.test.ts 夹具同形）。 */
function seedRendezvous(stateDir: string, instanceId: string, pid: number): void {
  const { publicKey } = generateKeyPairSync("ed25519");
  const record: Rendezvous = {
    bindingProtocol: BINDING_PROTOCOL,
    address: `ipc-${instanceId}.sock`,
    hostId: instanceId,
    epoch: "777",
    processIdentity: {
      platform: "test",
      pid,
      startedAtMs: 1_760_000_000_000,
      executable: "dsh-host",
    },
    publicKeySpki: b64url(publicKey.export({ type: "spki", format: "der" }) as Buffer),
  };
  const dir = join(stateDir, "instances", instanceId);
  mkdirSync(dir, { recursive: true });
  writeFileSync(join(dir, "rendezvous.json"), JSON.stringify(record), "utf8");
}

/** 种 legacy 身份（旧 profile 存储路径逐字保留；I4 语义）。 */
function seedLegacyIdentity(
  dshHome: string,
  key: { pem: string; spki: string },
  revoked: boolean,
): string {
  mkdirSync(join(dshHome, "profiles", "desktop", "tuvis-remote"), { recursive: true });
  writeFileSync(
    legacyFileOf(dshHome),
    JSON.stringify({
      version: 1,
      privateKeyPem: key.pem,
      publicKeySpki: key.spki,
      trustAnchorSpki: null,
      revoked,
      revokedBindingId: revoked ? "binding-old" : null,
    }),
    "utf8",
  );
  return key.spki;
}

/** 绑定账本行（Rust `StoredBinding` serde wire 形态逐字：camelCase + snake_case state）。 */
interface LedgerRow {
  bindingId: string;
  publicKeySpki: string;
  revokedKeySpki: string;
  executable: string;
  pid: number;
  startedAtMs: number;
  profileDir: string;
  createdAtMs: number;
  state: "bound" | "revoked";
}

/** 撤权墓碑行（`revoke_profile_in_ledger` 同形：钥移入 revokedKeySpki、原位清空）。 */
function revokedRow(revokedKeySpki: string, bindingId = "binding-retired"): LedgerRow {
  return {
    bindingId,
    publicKeySpki: "",
    revokedKeySpki,
    executable: "dsh-host",
    pid: 4242,
    startedAtMs: 1_760_000_000_000,
    profileDir: "/mock/.dsh/profiles/desktop",
    createdAtMs: 1_760_000_000_000,
    state: "revoked",
  };
}

/** 写实例绑定账本（`<stateDir>/instances/<instanceId>/bindings.json`）。 */
function writeLedger(stateDir: string, instanceId: string, rows: LedgerRow[]): void {
  writeFileSync(
    join(stateDir, "instances", instanceId, "bindings.json"),
    JSON.stringify({ bindings: rows }),
    "utf8",
  );
}

// ============================================================
// 迁移撤权守卫（RED 核心：旧代码把已撤钥迁入实例域）
// ============================================================

describe("A01/A02 migration revocation guard", () => {
  it("legacy_identity_marked_revoked_is_never_migrated_into_the_instance_slot", async () => {
    // 缺口①：legacy 文件自身 revoked=true（旧域内已撤的钥）→ 迁移必须拒绝。
    const root = tempRoot();
    const dshHome = join(root, "home");
    const stateDir = join(root, "state");
    const legacyKey = ed25519Key();
    seedLegacyIdentity(dshHome, legacyKey, true);
    seedRendezvous(stateDir, INSTANCE_A, 7001);

    const storage = instanceFileStorage(dshHome, stateDir, 7001);
    // 拒绝迁移 = null（全新生成路径信号）；实例槽绝不落地已撤钥。
    await expect(storage.read()).resolves.toBeNull();
    expect(existsSync(identityFileOf(dshHome, INSTANCE_A))).toBe(false);
    // legacy 文件原样（非破坏：撤权证据不被消费、不改写）。
    expect(JSON.parse(readFileSync(legacyFileOf(dshHome), "utf8"))).toMatchObject({
      revoked: true,
      publicKeySpki: legacyKey.spki,
    });
  });

  it("instance_tombstone_matching_the_legacy_key_blocks_re_migration", async () => {
    // 缺口②（RED 核心）：Web 实例曾用迁移钥完成授权 → 退役（撤权翻墓碑）→
    // 重装时实例槽为空 → 旧代码把同一把已撤钥以 revoked:false 迁回（复活）。
    const root = tempRoot();
    const dshHome = join(root, "home");
    const stateDir = join(root, "state");
    const legacyKey = ed25519Key();
    seedLegacyIdentity(dshHome, legacyKey, false); // legacy 文件自身无墓碑（撤权只发生在实例域）
    seedRendezvous(stateDir, INSTANCE_A, 7101);
    writeLedger(stateDir, INSTANCE_A, [revokedRow(legacyKey.spki)]);

    const storage = instanceFileStorage(dshHome, stateDir, 7101);
    // 实例域墓碑指名本钥 → 拒绝迁移（read() = null = 全新生成路径）。
    await expect(storage.read()).resolves.toBeNull();
    expect(existsSync(identityFileOf(dshHome, INSTANCE_A))).toBe(false);
    // legacy 文件原样。
    expect(JSON.parse(readFileSync(legacyFileOf(dshHome), "utf8"))).toMatchObject({
      revoked: false,
      publicKeySpki: legacyKey.spki,
    });
  });

  it("tombstone_for_a_different_key_does_not_block_migration", async () => {
    // 不误伤：墓碑里的钥是别的钥 → 正常迁入同一把 legacy 钥。
    const root = tempRoot();
    const dshHome = join(root, "home");
    const stateDir = join(root, "state");
    const legacyKey = ed25519Key();
    const otherKey = ed25519Key();
    seedLegacyIdentity(dshHome, legacyKey, false);
    seedRendezvous(stateDir, INSTANCE_A, 7201);
    writeLedger(stateDir, INSTANCE_A, [revokedRow(otherKey.spki)]);

    const storage = instanceFileStorage(dshHome, stateDir, 7201);
    const migrated = await storage.read();
    expect(migrated).not.toBeNull();
    expect(JSON.parse(migrated ?? "{}")).toMatchObject({
      version: 1,
      privateKeyPem: legacyKey.pem,
      publicKeySpki: legacyKey.spki,
      trustAnchorSpki: null,
      revoked: false,
    });
    expect(existsSync(identityFileOf(dshHome, INSTANCE_A))).toBe(true);
  });

  it("bound_rows_are_not_revocation_evidence_even_when_the_key_matches", async () => {
    // 精确口径：只有 state=revoked 的行是撤权证据；bound 行（Rust 不会给它带
    // revokedKeySpki，守卫不猜）即便字段撞上也不得当作墓碑。
    const root = tempRoot();
    const dshHome = join(root, "home");
    const stateDir = join(root, "state");
    const legacyKey = ed25519Key();
    seedLegacyIdentity(dshHome, legacyKey, false);
    seedRendezvous(stateDir, INSTANCE_A, 7301);
    const bound: LedgerRow = { ...revokedRow(legacyKey.spki), state: "bound" };
    writeLedger(stateDir, INSTANCE_A, [bound]);

    const storage = instanceFileStorage(dshHome, stateDir, 7301);
    const migrated = await storage.read();
    expect(JSON.parse(migrated ?? "{}")).toMatchObject({ publicKeySpki: legacyKey.spki });
  });

  it("missing_or_malformed_instance_ledger_does_not_block_migration", async () => {
    // 账本缺失 / 畸形 JSON / bindings 非数组 → 视为无证据（不因此拒绝迁移）。
    const legacyKey = ed25519Key();

    // (a) 无 bindings.json。
    const rootA = tempRoot();
    const dshHomeA = join(rootA, "home");
    const stateDirA = join(rootA, "state");
    seedLegacyIdentity(dshHomeA, legacyKey, false);
    seedRendezvous(stateDirA, INSTANCE_A, 7401);
    const migratedA = await instanceFileStorage(dshHomeA, stateDirA, 7401).read();
    expect(JSON.parse(migratedA ?? "{}")).toMatchObject({ publicKeySpki: legacyKey.spki });

    // (b) 畸形 JSON。
    const rootB = tempRoot();
    const dshHomeB = join(rootB, "home");
    const stateDirB = join(rootB, "state");
    seedLegacyIdentity(dshHomeB, legacyKey, false);
    seedRendezvous(stateDirB, INSTANCE_A, 7402);
    mkdirSync(join(stateDirB, "instances", INSTANCE_A), { recursive: true });
    writeFileSync(join(stateDirB, "instances", INSTANCE_A, "bindings.json"), "{not-json", "utf8");
    const migratedB = await instanceFileStorage(dshHomeB, stateDirB, 7402).read();
    expect(JSON.parse(migratedB ?? "{}")).toMatchObject({ publicKeySpki: legacyKey.spki });

    // (c) bindings 不是数组（格式不认）。
    const rootC = tempRoot();
    const dshHomeC = join(rootC, "home");
    const stateDirC = join(rootC, "state");
    seedLegacyIdentity(dshHomeC, legacyKey, false);
    seedRendezvous(stateDirC, INSTANCE_A, 7403);
    writeFileSync(
      join(stateDirC, "instances", INSTANCE_A, "bindings.json"),
      JSON.stringify({ bindings: "garbage" }),
      "utf8",
    );
    const migratedC = await instanceFileStorage(dshHomeC, stateDirC, 7403).read();
    expect(JSON.parse(migratedC ?? "{}")).toMatchObject({ publicKeySpki: legacyKey.spki });
  });

  it("revocation_tombstones_are_scoped_by_host_id_and_never_leak_across_instances", async () => {
    // 按 hostId 作用域：实例 A 被撤（墓碑指名本钥）→ A 拒迁；实例 B 用同一把
    // legacy 钥路径 → B 只读自己的账本（无墓碑）→ 不受 A 影响，正常迁入。
    const root = tempRoot();
    const dshHome = join(root, "home");
    const stateDir = join(root, "state");
    const legacyKey = ed25519Key();
    seedLegacyIdentity(dshHome, legacyKey, false);
    seedRendezvous(stateDir, INSTANCE_A, 7501);
    seedRendezvous(stateDir, INSTANCE_B, 7502);
    writeLedger(stateDir, INSTANCE_A, [revokedRow(legacyKey.spki)]);
    // 实例 B 无账本（从未授权过）。

    const storageA = instanceFileStorage(dshHome, stateDir, 7501);
    const storageB = instanceFileStorage(dshHome, stateDir, 7502);
    // A：自己的墓碑指名本钥 → 拒迁。
    await expect(storageA.read()).resolves.toBeNull();
    expect(existsSync(identityFileOf(dshHome, INSTANCE_A))).toBe(false);
    // B：A 的墓碑不可见 → 正常迁入同一把 legacy 钥。
    const migratedB = await storageB.read();
    expect(JSON.parse(migratedB ?? "{}")).toMatchObject({
      publicKeySpki: legacyKey.spki,
      revoked: false,
    });
    expect(existsSync(identityFileOf(dshHome, INSTANCE_B))).toBe(true);
    expect(existsSync(identityFileOf(dshHome, INSTANCE_A))).toBe(false);
  });

  it("refused_migration_falls_through_to_a_usable_fresh_identity_path", async () => {
    // 迁移拒绝后全新生成路径可用（BindingClient read-null → generateAndStore
    // 的落点）：不抛错、不卡死；写入的全新钥与已撤 legacy 钥无关。
    const root = tempRoot();
    const dshHome = join(root, "home");
    const stateDir = join(root, "state");
    const legacyKey = ed25519Key();
    seedLegacyIdentity(dshHome, legacyKey, false);
    seedRendezvous(stateDir, INSTANCE_A, 7601);
    writeLedger(stateDir, INSTANCE_A, [revokedRow(legacyKey.spki)]);

    const storage = instanceFileStorage(dshHome, stateDir, 7601);
    // 前置：迁移被拒（read() = null）。
    await expect(storage.read()).resolves.toBeNull();

    // 全新生成（与 BindingClient.generateIdentity 同形）→ 写入可用。
    const fresh = ed25519Key();
    const freshIdentity = JSON.stringify({
      version: 1,
      privateKeyPem: fresh.pem,
      publicKeySpki: fresh.spki,
      trustAnchorSpki: null,
      revoked: false,
      revokedBindingId: null,
    });
    await expect(storage.write(freshIdentity)).resolves.toBeUndefined();
    // 回读 = 全新钥；实例槽与已撤 legacy 钥无关；重复读不再触发迁移。
    await expect(storage.read()).resolves.toBe(freshIdentity);
    const slot = JSON.parse(readFileSync(identityFileOf(dshHome, INSTANCE_A), "utf8")) as {
      publicKeySpki: string;
    };
    expect(slot.publicKeySpki).toBe(fresh.spki);
    expect(slot.publicKeySpki).not.toBe(legacyKey.spki);
    // legacy 文件全程原样。
    expect(JSON.parse(readFileSync(legacyFileOf(dshHome), "utf8"))).toMatchObject({
      publicKeySpki: legacyKey.spki,
      revoked: false,
    });
  });
});
