/**
 * 生产 apply 接线回归：身份文件必须与宿主 profileContext.home 同根。
 * 保留真实会话、rendezvous 选择和文件存储，仅截断 IPC 外连；不完成授权握手。
 * HOME / USERPROFILE / cwd / 状态目录均隔离到每例临时目录，禁止并发执行。
 * cwd 隔离需要 Vitest forks 池（默认池），不支持 threads 池。
 */
import { generateKeyPairSync } from "node:crypto";
import { mkdtempSync, mkdirSync, readFileSync, readdirSync, rmSync, writeFileSync } from "node:fs";
import { homedir, tmpdir } from "node:os";
import { isAbsolute, join, relative } from "node:path";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { Rendezvous } from "../src/auth.js";
import { apply, type HostContext } from "../src/index.js";
import { NodeIpcTransport } from "../src/ipc.js";
import { BINDING_PROTOCOL } from "../src/types.js";

const HOST_ID = "a".repeat(32);
const rustKeys = generateKeyPairSync("ed25519");
const originalCwd = process.cwd();
let root: string | undefined;
let dispose: (() => void) | undefined;
let userHome: string;
let cwd: string;
let hostHome: string;
let stateDir: string;

beforeEach(() => {
  root = mkdtempSync(join(tmpdir(), "dsh-apply-home-"));
  userHome = join(root, "user-home");
  cwd = join(root, "cwd");
  hostHome = join(root, "effective-host-home");
  stateDir = join(root, "tuvis-state");
  for (const dir of [userHome, cwd, hostHome, stateDir]) mkdirSync(dir, { recursive: true });

  vi.stubEnv("HOME", userHome);
  vi.stubEnv("USERPROFILE", userHome);
  vi.stubEnv("DSH_HOME", undefined);
  vi.stubEnv("TUVIS_DSH_STATE_DIR", stateDir);
  vi.stubEnv("TUVIS_DSH_BINDING", "enable");
  process.chdir(cwd);
  // 先验证真实 os.homedir 的隔离；失败时尚未调用任何生产存储代码。
  expect(homedir()).toBe(userHome);
  vi.useFakeTimers();
});

afterEach(async () => {
  try {
    dispose?.();
    // dispose 后让已启动的异步会话收场，再清理环境和目录。
    await vi.advanceTimersByTimeAsync(0);
  } finally {
    dispose = undefined;
    vi.clearAllTimers();
    vi.useRealTimers();
    vi.restoreAllMocks();
    process.chdir(originalCwd);
    vi.unstubAllEnvs();
    if (root !== undefined) rmSync(root, { recursive: true, force: true });
    root = undefined;
  }
});

function seedRendezvous(origin: "legacy" | "instance"): void {
  const dir = origin === "legacy" ? stateDir : join(stateDir, "instances", HOST_ID);
  mkdirSync(dir, { recursive: true });
  const record: Rendezvous = {
    bindingProtocol: BINDING_PROTOCOL,
    address: "test-only://no-network",
    hostId: HOST_ID,
    epoch: "1",
    processIdentity: {
      platform: process.platform,
      pid: process.pid,
      startedAtMs: 1_760_000_000_000,
      executable: "isolated-dsh-host",
    },
    publicKeySpki: rustKeys.publicKey.export({ type: "spki", format: "der" }).toString("base64url"),
  };
  writeFileSync(join(dir, "rendezvous.json"), JSON.stringify(record), { mode: 0o600 });
}

const homeCases = [
  { label: "unset", env: () => undefined },
  { label: "empty", env: () => "" },
  { label: "whitespace", env: () => " \t " },
  { label: "tilde", env: () => "~" },
  { label: "tilde_custom_path", env: () => "~/custom-dsh" },
  { label: "relative_custom_path", env: () => "relative-dsh" },
  { label: "absolute_custom_path", env: () => join(root!, "other-dsh-home") },
  // 正向对照：确认接线已跑到真实身份落盘，避免把启动失败误判为路径缺陷。
  { label: "matching_host_home", env: () => hostHome },
];

for (const origin of ["legacy", "instance"] as const) {
  describe(`apply ${origin} identity storage follows the effective host home`, { concurrent: false }, () => {
    it.each(homeCases)("persists_identity_under_profile_home_when_DSH_HOME_is_$label", async ({ env }) => {
      vi.stubEnv("DSH_HOME", env());
      seedRendezvous(origin);
      const connect = vi.spyOn(NodeIpcTransport.prototype, "connect").mockRejectedValue(
        new Error("test boundary: stop after real identity persistence"),
      );
      const ctx: HostContext = {
        profileContext: { home: hostHome, dir: join(hostHome, "profiles", "desktop"), name: "desktop" },
        webServer: {
          protocol: "http:",
          port: 1,
          register: () => () => {},
        } as HostContext["webServer"],
      };

      dispose = apply(ctx);
      await vi.advanceTimersByTimeAsync(0);
      // 阶段哨兵：只有真实身份生成和 storage.write 完成后才会尝试连接。
      expect(connect).toHaveBeenCalledWith("test-only://no-network");

      const expectedFile = origin === "legacy"
        ? join(hostHome, "profiles", "desktop", "tuvis-remote", "identity.json")
        : join(hostHome, "tuvis-remote", HOST_ID, "identity.json");
      const withinSandbox = relative(root!, expectedFile);
      expect(isAbsolute(withinSandbox) || withinSandbox.startsWith("..")).toBe(false);
      const actualFiles = readdirSync(root!, { recursive: true, encoding: "utf8" })
        .filter((file) => file.endsWith(`${process.platform === "win32" ? "\\" : "/"}identity.json`))
        .sort();
      // 精确比较唯一落点，同时抓到 cwd / 字面 ~ / 环境 home 下的误写。
      expect(actualFiles).toEqual([withinSandbox]);
      const identity = JSON.parse(readFileSync(expectedFile, "utf8")) as Record<string, unknown>;
      expect(identity).toMatchObject({ version: 1, revoked: false });
      expect(typeof identity["publicKeySpki"]).toBe("string");
    });
  });
}
