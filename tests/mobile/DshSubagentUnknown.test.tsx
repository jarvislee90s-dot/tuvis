// H08：从真实 FilePanel/sheet 入口消费 unknown wire 载荷，覆盖清单到详情的状态传播。
// 不 mock 组件/API 转换，不增加生产 props；仅替换 HTTP，夹具均为合成数据。
import { dshSession, json } from "./dshSessionFixture";
import { cleanup, fireEvent, render, screen, within } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import i18n from "@/i18n";
import SessionDetail from "@/mobile/SessionDetail";
import type { Session } from "@/types/session";

const session = dshSession({
  id: "h08-parent",
  projectName: "h08-project",
  projectPath: "/tmp/h08-project",
  title: "父任务",
  status: "idle",
  lastMessageSubagentReport: false,
  flapFromSubagentActivity: false,
  lastActivityAt: "2026-10-08T07:36:45Z",
  activeSubagentCount: 0,
  form: "cli",
  dshInstanceId: "h08-instance",
});

const unknownChild = {
  id: "h08-child",
  name: "Worker",
  description: "读取目录",
  status: "unknown",
  spawnTs: "2026-10-08T07:00:00Z",
  endTs: null,
  tokens: { input: 12, cacheRead: 0, cacheCreation: 0, output: 3 },
};

beforeEach(async () => {
  await i18n.changeLanguage("zh");
  // 窄屏：文件看板 -> 子任务清单 -> 详情，避免依赖开发机视口或上个用例的媒体查询。
  vi.stubGlobal("matchMedia", undefined);
  vi.stubGlobal(
    "fetch",
    vi.fn(async (input: RequestInfo | URL) => {
      const { pathname } = new URL(String(input), "http://localhost");
      switch (pathname) {
        case "/m/api/v1/session-messages":
          return json({ messages: [], truncated: false });
        case "/m/api/v1/session-files":
          return json({
            files: [
              {
                path: "/tmp/h08-project/notes.md",
                lastSeq: 1,
                lastTs: null,
                hits: 1,
                modified: false,
                origin: "tool_read",
              },
            ],
            truncated: false,
          });
        case "/m/api/v1/session-subagents":
          return json({ subagents: [unknownChild] });
        case "/m/api/v1/session-subagent-messages":
          return json({
            supported: true,
            truncated: false,
            messages: [
              {
                seq: 0,
                role: "assistant",
                kind: "assistant",
                content: "正在分析目录结构",
                ts: null,
                toolName: null,
                toolArgs: null,
                collapsed: false,
              },
            ],
          });
        case "/m/api/v1/session-send-info":
          return json({ injectable: false, channels: [], visibility: "none" });
        case "/m/api/v1/channel":
          return json({ via: "direct", limited: false });
        default:
          throw new Error(`unexpected fetch: ${String(input)}`);
      }
    })
  );
});

afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
});

async function openSubagentList() {
  render(<SessionDetail session={session} onBack={() => {}} />);
  fireEvent.click(await screen.findByTestId("file-panel-button"));
  await screen.findByTestId("file-row-0-open");
  fireEvent.click(await screen.findByTestId("sheet-tab-subagents"));
  return screen.findByTestId("subagent-card-h08-child");
}

async function openSubagentDetail() {
  fireEvent.click(await openSubagentList());
  await screen.findByTestId("subagent-msg-0");
  return screen.getByTestId("subagent-detail");
}

describe("H08：DSH unknown 子任务从文件面板进入详情", () => {
  it("只有 unknown 子任务时仍能进入清单，并可切回原文件看板", async () => {
    const card = await openSubagentList();
    expect(card.textContent).toContain("Worker");
    expect(screen.queryByTestId("subagent-chip-h08-child")).toBeNull();
    fireEvent.click(screen.getByTestId("sheet-tab-files"));
    expect((await screen.findByTestId("file-row-0-open")).textContent).toContain("notes.md");
    expect(screen.getByTestId("file-row-0-origin").textContent).toBe("工具读取");
    expect(screen.queryByTestId("subagent-list")).toBeNull();
  });

  it("详情保留清单的未知状态，不把 unknown 压成已结束", async () => {
    const detail = await openSubagentDetail();
    const header = detail.querySelector("header")!;
    expect(header.textContent).toContain("Worker");
    expect(header.textContent).toMatch(/未知/);
    expect(header.textContent).not.toMatch(/已完成|已结束|定格|completed|finished/i);
    expect(screen.getByTestId("subagent-dot").className).not.toContain("bg-emerald-500");
  });

  it("未知详情仍显示真实转写，不宣称实时运行、完成或冻结时长", async () => {
    const detail = await openSubagentDetail();
    expect(within(detail).getByText("正在分析目录结构")).toBeTruthy();
    expect(detail.textContent).not.toMatch(/实时|已完成|已结束|定格|completed|finished|\d+:\d{2}/i);
    expect(within(detail).queryByRole("textbox")).toBeNull();
    expect(screen.getByTestId("subagent-back")).toBeTruthy();
  });
});
