// tests/settings/SignalHealthSection.test.tsx — T5：设置页「信号健康度」分区。
// 覆盖：挂载即 invoke hook_signal_health 渲染四态（未注册灰 / 正常绿含相对时间 /
// 无活跃会话中性 / 判据命中待办文案 + 复制命令）/ 复制按钮调用剪贴板写入 "/hooks" /
// 空载荷 → 空态、失败 → 错误态（不伪装空态）/ 刷新按钮触发再次 invoke /
// 1A 未注册原因细分（helperNotShipped → 蓝 badge 替代裸「未注册」；无 reason 维持灰）/
// i18n zh-en 无缺键（settings.signalHealth 子树，对齐 AuditLogSection.test.tsx 扫描）。
// mock 模式沿用 AuditLogSection.test.tsx（vi.hoisted + vi.mock，自带 invoke/event/clipboard）。
import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { readFileSync } from "node:fs";
import path from "node:path";

const { invokeMock } = vi.hoisted(() => ({ invokeMock: vi.fn() }));
vi.mock("@tauri-apps/api/core", () => ({ invoke: invokeMock }));
// useAppTranslation 内部 listen("@tauri-apps/api/event") 在 jsdom 无 Tauri 内核，须 mock
vi.mock("@tauri-apps/api/event", () => ({
  listen: vi.fn(async () => () => {}),
  emit: vi.fn(async () => {}),
}));

// tests/setup.ts 未初始化 i18n，显式引入并按默认英文断言（jsdom navigator.language=en）
import i18n from "@/i18n";
import { SignalHealthSection } from "@/components/settings/SignalHealthSection";

void i18n;

// Rust ToolSignalHealth（serde camelCase）同构样例：四态各占一行
const rowsOf = () => [
  {
    toolId: "claude",
    label: "Claude Code",
    registered: false,
    hasActiveSessions: false,
    lastEventAt: null,
  },
  {
    toolId: "codex",
    label: "Codex",
    registered: true,
    hasActiveSessions: true,
    lastEventAt: null,
  },
  {
    toolId: "kimi",
    label: "Kimi Code",
    registered: true,
    hasActiveSessions: false,
    lastEventAt: null,
  },
  {
    toolId: "workbuddy",
    label: "WorkBuddy",
    registered: true,
    hasActiveSessions: false,
    // 10 秒前的事件（绿态相对时间稳定落在「秒」档，不踩 60s 边界）
    lastEventAt: new Date(Date.now() - 10_000).toISOString(),
  },
  {
    // 非 codex 的判据命中（复评：通用零事件文案，无 codex 专属复制按钮）
    toolId: "zcode",
    label: "ZCode",
    registered: true,
    hasActiveSessions: true,
    lastEventAt: null,
  },
];

// hook_signal_health 调用次数（只数命令名，不关心附带参数形态）
const healthCalls = () => invokeMock.mock.calls.filter((c) => c[0] === "hook_signal_health").length;

// jsdom 无剪贴板内核：stub writeText（复制按钮断言入口）
const clipboardWrite = vi.fn(async () => {});

beforeEach(() => {
  invokeMock.mockReset();
  clipboardWrite.mockClear();
  Object.defineProperty(window.navigator, "clipboard", {
    value: { writeText: clipboardWrite },
    configurable: true,
  });
  invokeMock.mockImplementation(async (cmd: string) => {
    if (cmd === "hook_signal_health") return rowsOf();
    return null;
  });
});

afterEach(() => {
  vi.clearAllMocks();
});

describe("SignalHealthSection 四态渲染（T5）", () => {
  it("挂载即 invoke hook_signal_health，各行按判据渲染各自状态", async () => {
    render(<SignalHealthSection />);
    expect(await screen.findByText("Claude Code")).toBeTruthy();
    expect(invokeMock).toHaveBeenCalledWith("hook_signal_health");
    // 行数 = 工具数（data-signal-row 稳定钩子）
    expect(document.querySelectorAll("[data-signal-row]").length).toBe(5);
    // 未注册（灰）
    expect(screen.getByText("Not registered")).toBeTruthy();
    // 判据命中（已注册 ∧ 活跃会话 ∧ 零事件）→ codex 信任门文案（含工具名）
    expect(
      screen.getByText("Enter /hooks in the Codex terminal and trust the Tuvis entry (one-time)")
    ).toBeTruthy();
    // 非 codex 判据命中 → 通用零事件文案（无信任门措辞）
    expect(
      screen.getByText("Zero hook events, check that ZCode is registered and firing correctly")
    ).toBeTruthy();
    // 中性（已注册但无活跃会话）
    expect(screen.getByText("No active sessions, waiting for events")).toBeTruthy();
    // 绿态（正常 · 相对时间落在秒档）
    expect(screen.getByText(/OK · last event \d+s ago/)).toBeTruthy();
  });

  it("复制命令按钮仅 codex 信任门行持有，点击写入剪贴板 /hooks 并 toast 成功", async () => {
    render(<SignalHealthSection />);
    const btn = await screen.findByRole("button", { name: /copy command/i });
    // 5 行中仅 codex 显示复制按钮（/hooks 是 codex 信任门专属操作）
    expect(screen.getAllByRole("button", { name: /copy command/i })).toHaveLength(1);
    fireEvent.click(btn);
    await waitFor(() => expect(clipboardWrite).toHaveBeenCalledWith("/hooks"));
  });

  it("空载荷 → 空态，不渲染任何工具行", async () => {
    invokeMock.mockImplementation(async (cmd: string) =>
      cmd === "hook_signal_health" ? [] : null
    );
    render(<SignalHealthSection />);
    expect(await screen.findByText("No tools with hook channels")).toBeTruthy();
    expect(document.querySelectorAll("[data-signal-row]").length).toBe(0);
  });

  it("刷新按钮触发再次 invoke（调用计数 1 → 2）", async () => {
    render(<SignalHealthSection />);
    expect(await screen.findByText("Claude Code")).toBeTruthy();
    expect(healthCalls()).toBe(1);
    fireEvent.click(screen.getByRole("button", { name: /refresh/i }));
    await waitFor(() => expect(healthCalls()).toBe(2));
  });
});

// 加载失败可见性（P3 同款口径：失败 ≠ 空态；失败保留旧数据可重试）
describe("SignalHealthSection 加载失败态", () => {
  it("失败且无数据 → 显示加载失败错误态（不伪装成空态）", async () => {
    invokeMock.mockImplementation(async (cmd: string) => {
      if (cmd === "hook_signal_health") throw new Error("signal boom");
      return null;
    });
    const errSpy = vi.spyOn(console, "error").mockImplementation(() => {});
    try {
      render(<SignalHealthSection />);
      expect(
        await screen.findByTestId("signal-health-load-error").then((el) => el.textContent)
      ).toContain("Failed to load signal health");
      expect(screen.queryByText("No tools with hook channels")).toBeNull();
      expect(document.querySelectorAll("[data-signal-row]").length).toBe(0);
    } finally {
      errSpy.mockRestore();
    }
  });

  it("失败但有旧数据 → 保留旧数据（错误态不顶掉列表，可重试）", async () => {
    render(<SignalHealthSection />);
    expect(await screen.findByText("Claude Code")).toBeTruthy();
    invokeMock.mockImplementation(async (cmd: string) => {
      if (cmd === "hook_signal_health") throw new Error("signal boom");
      return null;
    });
    const errSpy = vi.spyOn(console, "error").mockImplementation(() => {});
    try {
      fireEvent.click(screen.getByRole("button", { name: /refresh/i }));
      await waitFor(() => expect(healthCalls()).toBe(2));
      expect(screen.getByText("Claude Code")).toBeTruthy();
      expect(screen.queryByTestId("signal-health-load-error")).toBeNull();
      expect(document.querySelectorAll("[data-signal-row]").length).toBe(5);
    } finally {
      errSpy.mockRestore();
    }
  });
});

// 1A 未注册原因细分（PR #146）：helperNotShipped = 本平台不随附 Hook 通道的设计内
// 状态（如 macOS 上的 kimi）→ 蓝 badge 解释性文案替代裸「未注册」；无 reason /
// reason 为 null（helper 在场的注册失败）→ 维持裸「未注册」待排查。四组合的 reason
// 判定已在 Rust 侧入测（monitor/hooks.rs reason 判定四组合），此处锁前端分支互斥
describe("SignalHealthSection 未注册原因细分（1A）", () => {
  it("kimi 未注册 + helperNotShipped → 蓝 badge 文案，裸「未注册」不出现", async () => {
    invokeMock.mockImplementation(async (cmd: string) => {
      if (cmd === "hook_signal_health") {
        return [
          {
            toolId: "kimi",
            label: "Kimi Code",
            registered: false,
            hasActiveSessions: false,
            lastEventAt: null,
            unregisteredReason: "helperNotShipped",
          },
        ];
      }
      return null;
    });
    render(<SignalHealthSection />);
    expect(await screen.findByText("Kimi Code")).toBeTruthy();
    expect(
      screen.getByText("Hook channel not shipped on this platform — no action needed")
    ).toBeTruthy();
    expect(screen.queryByText("Not registered")).toBeNull();
    expect(document.querySelectorAll("[data-signal-row]").length).toBe(1);
  });

  it("kimi 未注册但 reason 为 null（helper 在场的注册失败）→ 维持裸「未注册」", async () => {
    invokeMock.mockImplementation(async (cmd: string) => {
      if (cmd === "hook_signal_health") {
        return [
          {
            toolId: "kimi",
            label: "Kimi Code",
            registered: false,
            hasActiveSessions: false,
            lastEventAt: null,
            unregisteredReason: null,
          },
        ];
      }
      return null;
    });
    render(<SignalHealthSection />);
    expect(await screen.findByText("Not registered")).toBeTruthy();
    expect(screen.queryByText(/no action needed/i)).toBeNull();
  });
});

// i18n 契约：SignalHealthSection 源码引用的 settings.signalHealth.* 键必须在 zh 与
// en 两个 locale 同齐备，且两 locale 的 settings.signalHealth 键集相等
describe("SignalHealthSection i18n zh/en 无缺键（T5）", () => {
  // vitest jsdom 环境下 import.meta.url 非 file 协议，用进程 cwd（vitest 以仓库根启动）
  const root = process.cwd();
  const flat = (obj: Record<string, unknown>, prefix = ""): string[] =>
    Object.entries(obj).flatMap(([k, v]) =>
      typeof v === "object" && v !== null
        ? flat(v as Record<string, unknown>, `${prefix}${k}.`)
        : [`${prefix}${k}`]
    );

  it("源码引用键 zh/en 双语齐备，且两 locale 的 settings.signalHealth 键集相等", () => {
    const zh = JSON.parse(readFileSync(path.join(root, "src/i18n/locales/zh.json"), "utf8"));
    const en = JSON.parse(readFileSync(path.join(root, "src/i18n/locales/en.json"), "utf8"));
    const zhKeys = new Set(flat(zh.settings.signalHealth).map((k) => `settings.signalHealth.${k}`));
    const enKeys = new Set(flat(en.settings.signalHealth).map((k) => `settings.signalHealth.${k}`));
    expect([...zhKeys].filter((k) => !enKeys.has(k))).toEqual([]);
    expect([...enKeys].filter((k) => !zhKeys.has(k))).toEqual([]);

    // 源码扫描：组件引用的键必须双 locale 存在
    const text = readFileSync(
      path.join(root, "src/components/settings/SignalHealthSection.tsx"),
      "utf8"
    );
    const used = new Set<string>();
    for (const m of text.matchAll(/settings\.signalHealth\.([A-Za-z0-9_]+)/g)) used.add(m[0]);
    expect(used.size).toBeGreaterThanOrEqual(12);
    expect([...used].filter((k) => !zhKeys.has(k))).toEqual([]);
    expect([...used].filter((k) => !enKeys.has(k))).toEqual([]);
  });
});
