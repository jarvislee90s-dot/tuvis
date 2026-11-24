/**
 * 数据根与启动门（R10 插件侧 + 启动门纪律）测试。
 *
 * R10：两端必须只有一个数据根来源。Rust 侧（group 2 已改）把
 * `TUVIS_DSH_STATE_DIR` 定义为「**就是**状态目录」，未设时回落
 * `<tuvis_home>/dsh`（release = `~/.tuvis/dsh`）。插件必须逐字同义：
 * 不再读 `TUVIS_HOME`（Rust 只在 debug 读它，插件跟着猜会差一层 `.tuvis`），
 * 也不再保留插件私有的 `TUVIS_DSH_RENDEZVOUS`。
 */

import { homedir } from "node:os";
import { join } from "node:path";
import { afterEach, describe, expect, it } from "vitest";
import { bindingAutoStartEnabled, deriveRendezvousPath, type HostContext } from "../lib/index.js";

const ENV_KEYS = [
  "TUVIS_DSH_STATE_DIR",
  "TUVIS_HOME",
  "TUVIS_DSH_RENDEZVOUS",
  "TUVIS_DSH_BINDING",
] as const;

function setEnv(values: Partial<Record<(typeof ENV_KEYS)[number], string>>): void {
  for (const key of ENV_KEYS) delete process.env[key];
  for (const [key, value] of Object.entries(values)) {
    if (value !== undefined) process.env[key] = value;
  }
}

afterEach(() => setEnv({}));

describe("rendezvous 数据根（两端同义）", () => {
  it("state_dir_env_is_the_state_directory_itself", () => {
    setEnv({ TUVIS_DSH_STATE_DIR: "/tmp/tuvis-shadow" });
    // 「变量即目录」：不再拼一层 dsh/
    expect(deriveRendezvousPath()).toBe(join("/tmp/tuvis-shadow", "rendezvous.json"));
  });

  it("unset_state_dir_falls_back_to_the_release_default", () => {
    setEnv({});
    expect(deriveRendezvousPath()).toBe(join(homedir(), ".tuvis", "dsh", "rendezvous.json"));
  });

  it("tuvis_home_is_ignored_entirely", () => {
    // R10 的核心：debug 旋钮不得影响插件的根（否则影子/CI 两端各看一个根）
    setEnv({ TUVIS_HOME: "/tmp/shadow-home" });
    expect(deriveRendezvousPath()).toBe(join(homedir(), ".tuvis", "dsh", "rendezvous.json"));

    // 即使与 STATE_DIR 同时设置，STATE_DIR 说了算且不被改写成 <home>/.tuvis/...
    setEnv({ TUVIS_HOME: "/tmp/shadow-home", TUVIS_DSH_STATE_DIR: "/tmp/state" });
    expect(deriveRendezvousPath()).toBe(join("/tmp/state", "rendezvous.json"));
  });

  it("plugin_private_rendezvous_override_is_gone", () => {
    setEnv({ TUVIS_DSH_RENDEZVOUS: "/tmp/other/rendezvous.json" });
    expect(deriveRendezvousPath()).toBe(join(homedir(), ".tuvis", "dsh", "rendezvous.json"));
  });
});

describe("绑定启动门（保守激活）", () => {
  it("enabled_only_with_the_authenticated_url_probe_or_the_explicit_override", () => {
    setEnv({});
    const withProbe = {
      webServer: {} as HostContext["webServer"],
      connection: { authenticatedUrl: () => "http://127.0.0.1:1/?token=x" },
    } as HostContext;
    expect(bindingAutoStartEnabled(withProbe)).toBe(true);

    // 没有探针信号 → 不自发外连（测试/非官方宿主环境）
    const withoutProbe = {
      webServer: {} as HostContext["webServer"],
      connection: {},
    } as HostContext;
    expect(bindingAutoStartEnabled(withoutProbe)).toBe(false);

    // 连 connection 都没有（只有 webServer）→ 同样不动
    expect(bindingAutoStartEnabled({ webServer: {} as HostContext["webServer"] })).toBe(false);

    // 显式覆盖仍然有效（G1 影子/诊断通道）
    setEnv({ TUVIS_DSH_BINDING: "enable" });
    expect(bindingAutoStartEnabled(withoutProbe)).toBe(true);
  });

  it("a_non_callable_authenticated_url_does_not_open_the_gate", () => {
    setEnv({});
    const ctx = {
      webServer: {} as HostContext["webServer"],
      connection: { authenticatedUrl: "not-a-function" },
    } as HostContext;
    expect(bindingAutoStartEnabled(ctx)).toBe(false);
  });
});
