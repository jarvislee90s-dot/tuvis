import { spawnSync } from "node:child_process";
import {
  cpSync,
  existsSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  rmSync,
  statSync,
  symlinkSync,
  utimesSync,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";
import { describe, expect, it } from "vitest";

const repoRoot = resolve(dirname(fileURLToPath(import.meta.url)), "..");

function withFixture(run: (root: string, temp: string) => void) {
  const temp = mkdtempSync(join(tmpdir(), "dsh-check-test-"));
  // 空格和 shell 元字符同时检验路径作为参数传递。
  const root = join(temp, "workspace with spaces;$(touch SHELL_EXECUTED)");
  const plugin = join(root, "packages", "dsh-remote-access");
  const originalPlugin = join(repoRoot, "packages", "dsh-remote-access");
  try {
    mkdirSync(join(root, "packages"), { recursive: true });
    mkdirSync(join(root, "scripts"), { recursive: true });
    mkdirSync(join(root, "resources"), { recursive: true });
    cpSync(originalPlugin, plugin, {
      recursive: true,
      filter: (path) =>
        !["node_modules", "test", ".vitest", "lib"].includes(path.split(/[\\/]/).pop()!),
    });
    cpSync(
      join(repoRoot, "scripts", "build-dsh-plugin.mjs"),
      join(root, "scripts", "build-dsh-plugin.mjs")
    );
    cpSync(join(repoRoot, "resources", "dsh"), join(root, "resources", "dsh"), { recursive: true });
    for (const name of ["package.json", "pnpm-workspace.yaml", "pnpm-lock.yaml"]) {
      cpSync(join(repoRoot, name), join(root, name));
    }
    const linkType = process.platform === "win32" ? "junction" : "dir";
    symlinkSync(join(repoRoot, "node_modules"), join(root, "node_modules"), linkType);
    symlinkSync(join(originalPlugin, "node_modules"), join(plugin, "node_modules"), linkType);
    const privateTemp = join(root, "temporary output");
    mkdirSync(privateTemp);
    run(root, privateTemp);
  } finally {
    rmSync(temp, { recursive: true, force: true });
  }
}

function buildFixtureLib(root: string) {
  const plugin = join(root, "packages", "dsh-remote-access");
  const tsc = join(root, "node_modules", "typescript", "bin", "tsc");
  for (const config of ["tsconfig.json", "tsconfig.client.json"]) {
    const result = spawnSync(process.execPath, [tsc, "-p", join(plugin, config)], {
      cwd: plugin,
      encoding: "utf8",
      timeout: 120_000,
    });
    expect(result.status, `${result.stdout}\n${result.stderr}\n${result.error ?? ""}`).toBe(0);
  }
}

function check(root: string, temp: string) {
  return spawnSync(process.execPath, [join(root, "scripts", "build-dsh-plugin.mjs"), "--check"], {
    cwd: root,
    env: {
      ...process.env,
      TMPDIR: temp,
      TMP: temp,
      TEMP: temp,
      SHELL: join(temp, "missing-shell"),
    },
    encoding: "utf8",
    timeout: 120_000,
  });
}

function output(result: ReturnType<typeof check>) {
  return `${result.stdout}\n${result.stderr}\n${result.error ?? ""}`;
}

describe("DSH archive --check", () => {
  it("imports without throwing when process.argv[1] points to a nonexistent path", () => {
    const temp = mkdtempSync(join(tmpdir(), "dsh-import-test-"));
    try {
      const missingEntry = join(temp, "nonexistent-entry.js");
      expect(existsSync(missingEntry)).toBe(false);
      const result = spawnSync(
        process.execPath,
        [
          "--input-type=module",
          "-e",
          'await import(process.argv[2]); process.stdout.write("import-ok");',
          missingEntry,
          pathToFileURL(join(repoRoot, "scripts", "build-dsh-plugin.mjs")).href,
        ],
        { encoding: "utf8", timeout: 10_000 }
      );
      expect(result.status, `${result.stdout}\n${result.stderr}\n${result.error ?? ""}`).toBe(0);
      expect(result.stdout).toBe("import-ok");
    } finally {
      rmSync(temp, { recursive: true, force: true });
    }
  });

  it("rejects changed source despite a newer unrelated lib file and accepts restored source", () => {
    withFixture((root, temp) => {
      const plugin = join(root, "packages", "dsh-remote-access");
      const sourcePath = join(plugin, "src", "index.ts");
      const builtPath = join(plugin, "lib", "index.js");
      const authPath = join(plugin, "lib", "auth.js");
      expect(existsSync(join(plugin, "lib"))).toBe(false);
      buildFixtureLib(root);
      const source = readFileSync(sourcePath, "utf8");
      const built = readFileSync(builtPath);
      const changed = `${source}\nexport const m07FreshCompilationProbe = "changed source";\n`;
      expect(changed).not.toBe(source);
      writeFileSync(sourcePath, changed);
      const future = new Date(Date.now() + 60_000);
      utimesSync(authPath, future, future);
      expect(statSync(authPath).mtimeMs).toBeGreaterThan(statSync(sourcePath).mtimeMs);
      expect(readFileSync(builtPath).equals(built)).toBe(true);

      const changedResult = check(root, temp);
      writeFileSync(sourcePath, source);
      const restoredResult = check(root, temp);
      expect(restoredResult.status, output(restoredResult)).toBe(0);
      expect(readFileSync(builtPath).equals(built)).toBe(true);
      expect(changedResult.status, output(changedResult)).toBe(1);
      expect(output(changedResult)).toMatch(/remote-access\.tgz.*不一致/);
    });
  }, 180_000);

  it("rebuilds matching archive bytes without a workspace lib or a usable SHELL", () => {
    withFixture((root, temp) => {
      rmSync(join(root, "packages", "dsh-remote-access", "lib"), { recursive: true, force: true });
      const result = check(root, temp);
      expect(result.status, output(result)).toBe(0);
      expect(output(result)).toContain("逐字节");
      expect(existsSync(join(root, "SHELL_EXECUTED"))).toBe(false);
    });
  }, 180_000);
});
