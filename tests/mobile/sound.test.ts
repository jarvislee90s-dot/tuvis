// tests/mobile/sound.test.ts — 移动端提示音：开关持久化 + 合成音播放契约
//
// 契约（2026-09-19 用户裁决，与桌面端口径统一）：
// - 开关：默认开；toggle 写 localStorage `mam-mobile-sound`；读写抛错（隐私模式）不崩；
// - 播放：两音上行；每音有增益包络（**去电子感的关键**，原实现硬起硬停）；
// - 任何失败静默降级，不得抛出（提示音是锦上添花，不能中断提醒链路）。
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { getSoundEnabled, toggleSoundEnabled } from "@/mobile/sound";

const KEY = "mam-mobile-sound";

// Web Audio 最小桩：记录创建的 oscillator / gain 节点，供断言「两音 + 有包络」。
// **包络作用在 createGain() 产的 gain 节点上**（osc.connect(gain).connect(dest)），
// 不在 oscillator 自身——故两者分开记录，别把 osc.gain 当包络断言对象
interface StubNode {
  type?: string;
  frequency: { value: number };
  connect: ReturnType<typeof vi.fn>;
  start: ReturnType<typeof vi.fn>;
  stop: ReturnType<typeof vi.fn>;
}

interface StubGain {
  gain: {
    setValueAtTime: ReturnType<typeof vi.fn>;
    linearRampToValueAtTime: ReturnType<typeof vi.fn>;
    exponentialRampToValueAtTime: ReturnType<typeof vi.fn>;
  };
  connect: ReturnType<typeof vi.fn>;
}

function installAudioStub(state: AudioContextState = "running") {
  const nodes: StubNode[] = [];
  const gains: StubGain[] = [];
  // Web Audio 的 connect 返回**目标节点**（支持 osc.connect(gain).connect(dest) 链式调用）——
  // 桩必须同样返回入参，否则链式调用对 undefined 取 .connect 抛错，被播放器的
  // try/catch 静默吞掉（断言 start 未被调用，测试假绿/假红皆由此）
  const chainConnect = () => vi.fn((dest: unknown) => dest);
  class FakeOsc {
    type = "sine";
    frequency = { value: 0 };
    connect = chainConnect();
    start = vi.fn();
    stop = vi.fn();
    constructor() {
      nodes.push(this as unknown as StubNode);
    }
  }
  const ctx = {
    state,
    currentTime: 0,
    destination: {},
    createOscillator: () => new FakeOsc(),
    createGain: () => {
      const g: StubGain = {
        gain: {
          setValueAtTime: vi.fn(),
          linearRampToValueAtTime: vi.fn(),
          exponentialRampToValueAtTime: vi.fn(),
        },
        connect: chainConnect(),
      };
      gains.push(g);
      return g;
    },
    resume: vi.fn().mockResolvedValue(undefined),
  };
  // 必须用**普通函数**（不能箭头函数）：代码走 `new Ctx()`，箭头函数无 [[Construct]]
  // 会抛 "is not a constructor"，被播放器的 try/catch 静默吞掉 → nodes 恒空、
  // 断言全假绿（实测踩坑）
  // @ts-expect-error 测试注入的 Web Audio 桩（jsdom 无实现）
  window.AudioContext = vi.fn(function () {
    return ctx;
  });
  return { nodes, gains, ctx };
}

// 模块级单例 AudioContext 会跨用例复用：每个用例前重置模块缓存
beforeEach(() => {
  vi.resetModules();
  localStorage.clear();
});

afterEach(() => {
  localStorage.clear();
  vi.restoreAllMocks();
});

describe("mobile sound：开关持久化", () => {
  it("默认开（无已存值）", () => {
    expect(getSoundEnabled()).toBe(true);
  });

  it("已存 off → 关", () => {
    localStorage.setItem(KEY, "off");
    expect(getSoundEnabled()).toBe(false);
  });

  it("已存 on → 开", () => {
    localStorage.setItem(KEY, "on");
    expect(getSoundEnabled()).toBe(true);
  });

  it("非法值按开处理（只有 off 才关）", () => {
    localStorage.setItem(KEY, "banana");
    expect(getSoundEnabled()).toBe(true);
  });

  it("toggle 往返：开→关→开，且写回 localStorage", () => {
    expect(toggleSoundEnabled()).toBe(false);
    expect(localStorage.getItem(KEY)).toBe("off");
    expect(toggleSoundEnabled()).toBe(true);
    expect(localStorage.getItem(KEY)).toBe("on");
  });

  it("localStorage 读抛错（隐私模式）：按开处理，不崩", async () => {
    const spy = vi.spyOn(Storage.prototype, "getItem").mockImplementation(() => {
      throw new Error("SecurityError");
    });
    const { getSoundEnabled: fresh } = await import("@/mobile/sound");
    expect(fresh()).toBe(true);
    spy.mockRestore();
  });

  it("localStorage 写抛错：toggle 仍返回翻转值，不崩（本次会话内可继续往返）", async () => {
    const spy = vi.spyOn(Storage.prototype, "setItem").mockImplementation(() => {
      throw new Error("QuotaExceededError");
    });
    const { toggleSoundEnabled: fresh } = await import("@/mobile/sound");
    expect(fresh()).toBe(false);
    spy.mockRestore();
  });
});

describe("mobile sound：合成音播放契约", () => {
  it("播放两音（上行 E5→A5），每音独立 oscillator", async () => {
    const { nodes } = installAudioStub();
    const { playCompletionChime: play } = await import("@/mobile/sound");
    play();
    expect(nodes).toHaveLength(2);
    expect(nodes.map((n) => n.frequency.value)).toEqual([659.25, 880.0]);
  });

  it("每音有增益包络（起音 + 指数衰减）——原实现硬起硬停是难听根因", async () => {
    const { gains } = installAudioStub();
    const { playCompletionChime: play } = await import("@/mobile/sound");
    play();
    expect(gains).toHaveLength(2); // 每音一个 gain 节点
    for (const g of gains) {
      expect(g.gain.setValueAtTime).toHaveBeenCalled();
      expect(g.gain.linearRampToValueAtTime).toHaveBeenCalled(); // attack
      expect(g.gain.exponentialRampToValueAtTime).toHaveBeenCalled(); // release
    }
  });

  it("第二音错开触发（非同时起播）", async () => {
    const { nodes, ctx } = installAudioStub();
    const { playCompletionChime: play } = await import("@/mobile/sound");
    play();
    const startTimes = nodes.map((n) => n.start.mock.calls[0]?.[0] as number);
    expect(startTimes[0]).toBe(0);
    expect(startTimes[1]).toBeGreaterThan(0);
    // 起播时刻基于 ctx.currentTime（0），故第二音 at > 0
    expect(ctx.createOscillator).toBeDefined();
  });

  it("无 Web Audio 的环境（jsdom / 老浏览器）：跳过，不抛错", async () => {
    // @ts-expect-error 刻意移除
    delete window.AudioContext;
    const { playCompletionChime: play } = await import("@/mobile/sound");
    expect(() => play()).not.toThrow();
  });

  it("AudioContext 构造抛错：静默降级，不抛错（不得中断提醒链路）", async () => {
    // @ts-expect-error 测试注入
    window.AudioContext = vi.fn(() => {
      throw new Error("AudioContext unavailable");
    });
    const { playCompletionChime: play } = await import("@/mobile/sound");
    expect(() => play()).not.toThrow();
  });

  it("suspended 态：尝试 resume（自动播放策略），不抛错", async () => {
    const { ctx } = installAudioStub("suspended");
    const { playCompletionChime: play } = await import("@/mobile/sound");
    play();
    expect(ctx.resume).toHaveBeenCalled();
  });
});

describe("mobile sound：与桌面端 statusToColor 口径一致性（三色语义单源）", () => {
  it("STATUS_COLOR_KIND 与桌面 statusToColor 逐值一致（两端提示音口径统一的前提）", async () => {
    const { STATUS_COLOR_KIND } = await import("@/mobile/board-logic");
    // 桌面 hooks/useNotification.ts 的 statusToColor 逐值（2026-09-19 核对）
    expect(STATUS_COLOR_KIND).toEqual({
      waiting: "red",
      processing: "yellow",
      thinking: "yellow",
      compacting: "yellow",
      idle: "green",
      finished: "green",
    });
  });
});
