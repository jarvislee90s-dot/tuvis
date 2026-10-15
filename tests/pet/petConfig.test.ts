// tests/pet/petConfig.test.ts
import { readFileSync } from "node:fs";
import path from "node:path";
import { beforeEach, describe, expect, it, vi } from "vitest";
// resetGravityPerRun 走 invoke 取后端 run id——hoisted mock 拦截（同 healthCheckCard 手法）
const invokeMock = vi.hoisted(() => vi.fn());
vi.mock("@tauri-apps/api/core", () => ({ invoke: invokeMock }));
import {
  loadConfig,
  saveConfig,
  loadVisible,
  saveVisible,
  loadPosition,
  savePosition,
  petSoundTakeover,
  petSuppressPopup,
  subscribeConfig,
  resetGravityPerRun,
  GRAVITY_RUN_KEY,
} from "@/components/pet/petConfig";
import { saveActiveId, loadActiveName } from "@/components/pet/petRuntime";

describe("petConfig", () => {
  beforeEach(() => localStorage.clear());

  it("无存储时返回默认值（spec §10.1）", () => {
    const c = loadConfig();
    expect(c).toMatchObject({
      alwaysOnTop: true,
      muted: false,
      talkative: true,
      gravity: true,
      scale: 1,
      dblAction: "waving",
      approvalAction: "waiting",
      errorAction: "failed",
      doneAction: "jumping",
      followStartup: true,
    });
    expect(loadVisible()).toBe(false);
    expect(loadPosition()).toBeNull();
  });

  it("非法值回落默认（sanitize）", () => {
    localStorage.setItem(
      "mam-pet-config",
      JSON.stringify({ scale: 9, dblAction: "hack", muted: "yes" })
    );
    const c = loadConfig();
    expect(c.scale).toBe(1);
    expect(c.dblAction).toBe("waving");
    expect(c.muted).toBe(false);
  });

  it("patch 保存与读取回环", () => {
    saveConfig({ scale: 1.25, doneAction: "review" });
    expect(loadConfig().scale).toBe(1.25);
    expect(loadConfig().doneAction).toBe("review");
  });

  it("visible / position 回环", () => {
    saveVisible(true);
    expect(loadVisible()).toBe(true);
    savePosition({ x: 100.6, y: -200 });
    expect(loadPosition()).toEqual({ x: 101, y: -200 });
  });

  // 旧宠物 spec D3/D4 以「置顶」为浮窗抑制条件；特性 spec W1（2026-09-03 §2）改为仅看可见性，以 W1 为准
  it("接管判定：可见即接管声音并抑制浮窗（无论置顶，spec W1）", () => {
    saveVisible(false);
    expect(petSoundTakeover()).toBe(false);
    expect(petSuppressPopup()).toBe(false);
    saveVisible(true);
    expect(petSoundTakeover()).toBe(true);
    saveConfig({ alwaysOnTop: false });
    expect(petSuppressPopup()).toBe(true);
    saveConfig({ alwaysOnTop: true });
    expect(petSuppressPopup()).toBe(true);
  });
});

describe("启动跟随（2026-10-11 用户裁决，评审 M2 补锁）", () => {
  beforeEach(() => localStorage.clear());

  it("旧存档缺 followStartup 键 → 默认 true 兜底（「没设置过 = 默认开」）", () => {
    localStorage.setItem("mam-pet-config", JSON.stringify({ scale: 1.25 }));
    expect(loadConfig().followStartup).toBe(true);
  });

  it("关闭后回环：存 false 读 false，下次启动即不跟出", () => {
    expect(loadConfig().followStartup).toBe(true);
    saveConfig({ followStartup: false });
    expect(loadConfig().followStartup).toBe(false);
  });

  it("启动靴源码锁：requestedVisible 只由 followStartup 驱动且落权威值（评审 B1/M1 接线）", () => {
    const boot = readFileSync(path.join(process.cwd(), "src/pages/pet.tsx"), "utf8");
    // 启动显隐决策 = 跟随键（不是上次 visible——「隐了下次跟出」就是这一行实现的）
    expect(boot).toContain("const requestedVisible = cfg.followStartup;");
    // 决策落为权威值（saveVisible）：窗口实况与键同步，防哑宠物/幽灵语音
    expect(boot).toContain("saveVisible(requestedVisible);");
    // Rust 侧：新窗复位权威标记 + set_pet_visible 登记行使 + 兜底按标记闭嘴
    const petRust = readFileSync(path.join(process.cwd(), "src-tauri/src/commands/pet.rs"), "utf8");
    expect(petRust.match(/PET_PAGE_SPOKE/g)?.length ?? 0).toBeGreaterThanOrEqual(4);
    expect(petRust).toContain("PET_PAGE_SPOKE.store(false");
    // 行使登记（B1 心脏行）不设防的话删掉它余下断言依然绿（复评 I-2）
    expect(petRust).toContain("PET_PAGE_SPOKE.store(true");
    expect(petRust).toContain("PET_PAGE_SPOKE.load");
  });
});

describe("petConfig 激活指针订阅（P1-6）", () => {
  beforeEach(() => localStorage.clear());

  it("同窗口 saveActiveId 经本地事件即时触发 subscribeConfig 回调", () => {
    const fn = vi.fn();
    const unsub = subscribeConfig(fn);
    fn.mockClear();
    saveActiveId("starry-dew", true, "Starry Dew");
    expect(fn).toHaveBeenCalledTimes(1); // storage 事件只跨窗口：本窗口写入靠本地事件触达
    expect(loadActiveName()).toBe("Starry Dew");
    // 跨窗口 storage 事件同样触达（激活键/展示名键已纳入过滤）
    window.dispatchEvent(new StorageEvent("storage", { key: "mam-pet-active" }));
    expect(fn).toHaveBeenCalledTimes(2);
    window.dispatchEvent(new StorageEvent("storage", { key: "mam-pet-active-name" }));
    expect(fn).toHaveBeenCalledTimes(3);
    unsub();
    saveActiveId("builtin-foxbell", true);
    expect(fn).toHaveBeenCalledTimes(3); // 退订后不再触达
  });
});

describe("物理坠落按应用重启归位（2026-10-13 用户裁决）", () => {
  beforeEach(() => {
    localStorage.clear();
    invokeMock.mockReset();
  });

  it("标记不一致（新一轮运行）→ 开着的 gravity 被重置为关，标记落新 run id", async () => {
    saveConfig({ gravity: true });
    invokeMock.mockResolvedValue("run-1");
    await resetGravityPerRun();
    expect(loadConfig().gravity).toBe(false);
    expect(localStorage.getItem(GRAVITY_RUN_KEY)).toBe("run-1");
  });

  it("标记一致（同进程窗口重开/幂等重调）→ 不动会话内手动开启的选择", async () => {
    invokeMock.mockResolvedValue("run-1");
    await resetGravityPerRun(); // 首次：落标记（默认 gravity 本就 true → 一并归关）
    saveConfig({ gravity: true }); // 会话内手动再开
    await resetGravityPerRun(); // 同 run id 再跑（窗口后开/重复调用的路径）
    expect(loadConfig().gravity).toBe(true); // 不被覆盖
  });

  it("gravity 已为关时只落标记、不写配置（无谓 storage 事件不打扰订阅方）", async () => {
    saveConfig({ gravity: false });
    const fn = vi.fn();
    const unsub = subscribeConfig(fn);
    invokeMock.mockResolvedValue("run-2");
    await resetGravityPerRun();
    expect(localStorage.getItem(GRAVITY_RUN_KEY)).toBe("run-2");
    expect(loadConfig().gravity).toBe(false);
    expect(fn).not.toHaveBeenCalled();
    unsub();
  });

  it("后端不可用（invoke 失败）→ 保持现状且不抛（不阻塞启动）", async () => {
    saveConfig({ gravity: true });
    invokeMock.mockRejectedValue(new Error("no backend"));
    await expect(resetGravityPerRun()).resolves.toBeUndefined();
    expect(loadConfig().gravity).toBe(true);
    expect(localStorage.getItem(GRAVITY_RUN_KEY)).toBeNull();
  });

  it("main.tsx 启动接线源码锁：所有窗口入口模块顶层统一前置触发（幂等）", () => {
    const boot = readFileSync(path.join(process.cwd(), "src/main.tsx"), "utf8");
    expect(boot).toContain("resetGravityPerRun");
    // Rust 侧命令注册锁（漏注册 invoke 恒走 catch → 重置静默失效，界面看不出）
    const lib = readFileSync(path.join(process.cwd(), "src-tauri/src/lib.rs"), "utf8");
    expect(lib).toContain("fn app_run_id()");
    expect(lib).toContain("app_run_id,");
  });
});
