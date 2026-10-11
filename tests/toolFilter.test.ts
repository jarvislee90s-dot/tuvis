// tests/toolFilter.test.ts — 桌面看板「按工具筛选」纯函数层（2026-10-11）
// 覆盖：过滤语义（all 引用保持 / 单工具严格相等）+ localStorage 记忆闭环
//（存读往返 / 脏值防御回落 / 读失败静默）。
import { beforeEach, describe, expect, it, vi } from "vitest";
import { filterByAgent, loadToolFilter, saveToolFilter } from "@/lib/toolFilter";
import type { Session } from "@/types/session";

const session = (id: string, agentType: Session["agentType"]): Session => ({
  id,
  agentType,
  projectName: "proj",
  projectPath: "/tmp/proj",
  title: null,
  gitBranch: null,
  githubUrl: null,
  status: "idle",
  lastMessage: null,
  lastMessageRole: null,
  lastMessageSubagentReport: false,
  flapFromSubagentActivity: false,
  lastActivityAt: "2026-10-11T00:00:00Z",
  pid: 1,
  cpuUsage: 0,
  activeSubagentCount: 0,
  form: "cli",
  jumpSupported: false,
  unread: false,
});

const SESSIONS = [
  session("a", "claude"),
  session("b", "codex"),
  session("c", "claude"),
  session("d", "kimi"),
];

describe("filterByAgent", () => {
  it('"all" 原样返回（引用保持，不过滤不拷贝）', () => {
    expect(filterByAgent(SESSIONS, "all")).toBe(SESSIONS);
  });

  it("单工具过滤：只留该 agentType；无此工具会话 → 空数组（空态入口）", () => {
    expect(filterByAgent(SESSIONS, "claude").map((s) => s.id)).toEqual(["a", "c"]);
    expect(filterByAgent(SESSIONS, "workbuddy")).toEqual([]);
  });
});

describe("localStorage 记忆（重启保持上次选择）", () => {
  beforeEach(() => {
    localStorage.clear();
  });

  it("未存过 → 默认 all；存读往返成立", () => {
    expect(loadToolFilter()).toBe("all");
    saveToolFilter("kimi");
    expect(loadToolFilter()).toBe("kimi");
  });

  it("脏值防御：超出八工具清单/损坏的残留值一律回落 all（AgentType 增删值后不卡死）", () => {
    localStorage.setItem("board.toolFilter", "future-tool");
    expect(loadToolFilter()).toBe("all");
    localStorage.setItem("board.toolFilter", "{oops");
    expect(loadToolFilter()).toBe("all");
  });

  it("读失败静默：localStorage 抛异常仍回落 all（隐私模式等），筛选交互不受阻塞", () => {
    const spy = vi.spyOn(Storage.prototype, "getItem").mockImplementation(() => {
      throw new DOMException("denied");
    });
    expect(loadToolFilter()).toBe("all");
    spy.mockRestore();
  });
});
