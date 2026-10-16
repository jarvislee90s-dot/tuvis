// tests/pet/foxbell-events.test.tsx — 差分事件触发语音与任务姿态（spec D1-D4）
// 注意：本文件全部使用假定时器。RTL 的 waitFor 在 vitest 假定时器下会死锁
// （其 asyncWrapper 用 setTimeout(0) 排空微任务，被假定时器接管且无 jest 全局可推进），
// 故字幕断言采用「act 推进后直接同步查询」模式（数据 effect 在 rerender 的 act 内同步刷出气泡）。
import { render, screen, act } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

vi.mock("@tauri-apps/api/window", () => ({
  getCurrentWindow: () => ({
    show: vi.fn(),
    hide: vi.fn(),
    setAlwaysOnTop: vi.fn(),
    setIgnoreCursorEvents: vi.fn(),
    setPosition: vi.fn(),
    setSize: vi.fn(),
    outerPosition: vi.fn(),
    outerSize: vi.fn(),
    scaleFactor: vi.fn(),
    currentMonitor: vi.fn(),
  }),
}));
// 组件自 Task 12 起 import 了 emit（onHide 广播显隐）：补齐导出，避免 vitest mock 代理缺属性在运行时报错
vi.mock("@tauri-apps/api/event", () => ({
  listen: vi.fn(async () => () => {}),
  emit: vi.fn(async () => {}),
}));
const manifest = [
  { index: 0, group: "done", name: "搞定咯", file: "done/x.m4a" },
  { index: 1, group: "approval", name: "快批快批", file: "approval/y.m4a" },
];
const fetchMock = vi.fn(async () => ({ ok: true, json: async () => manifest }));
vi.stubGlobal("fetch", fetchMock);

let data: unknown = undefined;
vi.mock("@/lib/query/queries/sessions", () => ({ useSessionsQuery: () => ({ data }) }));

import { FoxbellPet } from "@/components/pet/FoxbellPet";

const mk = (id: string, status: string) => ({
  id,
  agentType: "claude",
  projectName: "P",
  projectPath: "/",
  title: null,
  gitBranch: null,
  githubUrl: null,
  status,
  lastMessage: "m",
  lastMessageRole: null,
  lastActivityAt: "",
  pid: 1,
  cpuUsage: 0,
  activeSubagentCount: 0,
  form: "cli",
  jumpSupported: true,
});

describe("FoxbellPet 事件接线", () => {
  beforeEach(() => {
    vi.useFakeTimers();
    localStorage.clear();
    // 语音线闸门（问题 6）：桌宠可见时 playVoice 才生效，事件/双击语音用例需显式开启
    localStorage.setItem("mam-pet-visible", "1");
    // tests/setup.ts 的 beforeAll(server.listen) 运行时机晚于模块级 stubGlobal，
    // 会用 MSW 拦截器重写 globalThis.fetch —— 每个用例前重打桩保证 fetchMock 生效（Task 9 同款处理）
    vi.stubGlobal("fetch", fetchMock);
  });
  afterEach(() => vi.useRealTimers());

  it("运行中 → idle：播 done 组语音 + 绿卡（spec D1/D2）", async () => {
    data = { sessions: [mk("a", "thinking")], totalCount: 1, waitingCount: 0 };
    const { rerender } = render(<FoxbellPet />);
    await act(async () => {
      await vi.advanceTimersByTimeAsync(50);
    }); // 首帧 + manifest
    data = { sessions: [mk("a", "idle")], totalCount: 1, waitingCount: 0 };
    rerender(<FoxbellPet />);
    await act(async () => {
      await vi.advanceTimersByTimeAsync(50);
    });
    expect(screen.queryByTestId("pet-bubble")?.textContent).toBe("搞定咯");
    expect(screen.getByTestId("pet-card-a").textContent).toContain("已完成");
  });

  it("运行中 → waiting：播 approval 组语音且绿卡不出现（spec D2/D3）", async () => {
    data = { sessions: [mk("a", "processing")], totalCount: 1, waitingCount: 0 };
    const { rerender } = render(<FoxbellPet />);
    await act(async () => {
      await vi.advanceTimersByTimeAsync(50);
    });
    data = { sessions: [mk("a", "waiting")], totalCount: 1, waitingCount: 1 };
    rerender(<FoxbellPet />);
    await act(async () => {
      await vi.advanceTimersByTimeAsync(50);
    });
    expect(screen.queryByTestId("pet-bubble")?.textContent).toBe("快批快批");
    expect(screen.getByTestId("pet-card-a").textContent).toContain("等待操作");
  });

  it("waiting 持续：10s 内再次出现不重复播（spec D3 限频）", async () => {
    // Fix 2：序列改为 waiting → processing → waiting（无 completion 腿，避免完成气泡顶掉首个审批气泡
    // 造成「任意 2500ms 气泡都会过期」的假阳性）。判别窗口推导：
    //   t≈50 首次 waiting → 气泡1 至 ≈2550；t≈2100 二次 waiting（10s 闸门内）；
    //   断言点 t≈3100 ∈ (2550, 4600)：无闸门时气泡2（≈2100 起、至 ≈4600）仍在 → 测试失败；有闸门 → 通过
    data = { sessions: [mk("a", "processing")], totalCount: 1, waitingCount: 0 };
    const { rerender } = render(<FoxbellPet />);
    await act(async () => {
      await vi.advanceTimersByTimeAsync(50);
    }); // 首帧 + manifest
    data = { sessions: [mk("a", "waiting")], totalCount: 1, waitingCount: 1 };
    rerender(<FoxbellPet />);
    await act(async () => {
      await vi.advanceTimersByTimeAsync(50);
    });
    expect(screen.queryByTestId("pet-bubble")?.textContent).toBe("快批快批");
    data = { sessions: [mk("a", "processing")], totalCount: 1, waitingCount: 0 };
    rerender(<FoxbellPet />);
    await act(async () => {
      await vi.advanceTimersByTimeAsync(2000);
    }); // t≈2100，仍处 10s 限频窗
    data = { sessions: [mk("a", "waiting")], totalCount: 1, waitingCount: 1 };
    rerender(<FoxbellPet />);
    await act(async () => {
      await vi.advanceTimersByTimeAsync(1000);
    }); // t≈3100：气泡1 已过期、气泡2（若有）未过期
    expect(screen.queryByTestId("pet-bubble")).toBeNull();
  });
});
