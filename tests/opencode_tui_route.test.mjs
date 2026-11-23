import assert from "node:assert/strict";
import { mkdtemp, readFile, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";
import { setTimeout as delay } from "node:timers/promises";

test("restarting the same TUI plugin instance recreates its health boot without changing PID", async (t) => {
  const home = await mkdtemp(join(tmpdir(), "tuvis-tui-route-"));
  const homeKey = process.platform === "win32" ? "USERPROFILE" : "HOME";
  const previousHome = process.env[homeKey];
  process.env[homeKey] = home;
  let cleanup;
  t.after(async () => {
    try {
      cleanup?.();
    } finally {
      if (previousHome === undefined) delete process.env[homeKey];
      else process.env[homeKey] = previousHome;
      await rm(home, { recursive: true, force: true });
    }
  });

  const { default: plugin } = await import("../src-tauri/src/monitor/opencode_tui_route.mjs");
  const ctx = { ui: { router: { current: () => ({ type: "home" }) } } };
  const registration = join(home, ".tuvis", "opencode-tui", `${process.pid}.json`);

  async function readHealth() {
    const deadline = performance.now() + 2_000;
    let raw;
    for (;;) {
      try {
        raw = await readFile(registration, "utf8");
        break;
      } catch (error) {
        if (error.code !== "ENOENT" || performance.now() >= deadline) throw error;
        await delay(10);
      }
    }
    const { port } = JSON.parse(raw);
    const response = await fetch(`http://127.0.0.1:${port}/health`, {
      headers: { connection: "close" },
      signal: AbortSignal.timeout(2_000),
    });
    assert.equal(response.status, 200);
    return response.json();
  }

  cleanup = plugin.setup(ctx);
  const first = await readHealth();
  cleanup();
  cleanup = undefined;
  await assert.rejects(readFile(registration), { code: "ENOENT" });

  cleanup = plugin.setup(ctx);
  const second = await readHealth();
  assert.equal(first.pid, process.pid);
  assert.equal(second.pid, first.pid);
  for (const { boot } of [first, second]) {
    assert.equal(typeof boot, "string");
    assert.ok(boot.length > 0);
  }
  assert.notEqual(second.boot, first.boot, "health boot must change after cleanup and setup");
});
