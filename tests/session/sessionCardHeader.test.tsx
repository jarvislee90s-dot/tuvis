// tests/session/sessionCardHeader.test.tsx — Task 6：APP 类卡片头部重排（全工具统一）。
// 首行：标签→目录名→分支 | 状态指示区 | X 最右；会话名尾巴独立第二行
// （实机反馈轮 2026-10-27 #1：分支可截断先让路、首组 overflow-hidden 防压状态点）；
// 未读与状态灯合并为单一指示（光环徽章，不再渲染独立小绿点）；X 仍 stopPropagation。
import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";

const { invokeMock } = vi.hoisted(() => ({ invokeMock: vi.fn() }));

vi.mock("@tauri-apps/api/core", () => ({ invoke: invokeMock }));
vi.mock("sonner", () => ({ toast: { info: vi.fn(), error: vi.fn(), success: vi.fn() } }));
vi.mock("@tauri-apps/api/event", () => ({ listen: vi.fn(async () => () => {}) }));
// Fix 4 起 SessionCard 用 useQueryClient 在关闭成功后失效 ["sessions"]；
// 本文件只断言 DOM 结构，注入无副作用桩即可（避免为 13 处 render 各包一层 Provider）
vi.mock("@tanstack/react-query", () => ({
  useQueryClient: () => ({ invalidateQueries: vi.fn(), setQueryData: vi.fn() }),
}));
// tests/setup.ts 未初始化 i18n，显式引入（fallbackLng=en，与断言文案一致）
import i18n from "@/i18n";
import { SessionCard } from "@/components/sessions/SessionCard";
import type { Session } from "@/types/session";

void i18n;

const mk = (over: Partial<Session>): Session => ({
  id: "s1",
  agentType: "workbuddy",
  projectName: "项目A",
  projectPath: "/a",
  title: null,
  gitBranch: null,
  githubUrl: null,
  status: "waiting",
  lastMessage: "运行中",
  lastMessageRole: null,
  lastActivityAt: "",
  pid: 42,
  cpuUsage: 0,
  activeSubagentCount: 0,
  form: "app",
  jumpSupported: true,
  unread: false,
  ...over,
});

beforeEach(() => {
  invokeMock.mockReset();
  invokeMock.mockResolvedValue(undefined);
});

/** 首行（徽标+项目名+分支 | 状态区）：卡内唯一带 justify-between 的行 */
const headerRow = (container: HTMLElement) => container.querySelector(".justify-between")!;
/** 第二行：会话标题尾巴——首行的下一个兄弟（独立行元素） */
const titleRow = (container: HTMLElement) => headerRow(container).nextElementSibling!;

describe("APP 卡头部重排（Task 6）", () => {
  it("顺序：标签→目录名→分支 | 状态区 | X 最右；会话名独立第二行", () => {
    const { container } = render(
      <SessionCard
        session={mk({
          status: "idle",
          form: "app",
          unread: false,
          title: "总结标题",
          gitBranch: "main",
        })}
      />
    );
    const header = headerRow(container);
    // 首行左侧信息组：工具标签 → 项目目录名 → 分支（会话名已移出首行）
    const left = header.firstElementChild!;
    const texts = Array.from(left.children).map((el) => el.textContent);
    expect(texts).toEqual(["WorkBuddy", "项目A", "main"]);
    // 第二行：会话名尾巴（整行、truncate、title 全文）
    const tail = titleRow(container);
    expect(tail.textContent).toBe("总结标题");
    expect(tail.className).toContain("truncate");
    expect(tail.getAttribute("title")).toBe("总结标题");
    // 右侧：状态指示区 + 关闭 X（X 位于整行最右）
    const right = header.lastElementChild!;
    expect(right.children.length).toBe(2);
    expect(right.lastElementChild!.tagName).toBe("BUTTON");
    expect(right.lastElementChild!.getAttribute("aria-label")).toContain("Hide for now");
  });

  it("无总结时回退 8 位 hash，hash 渲染在第二行尾巴", () => {
    const { container } = render(
      <SessionCard session={mk({ id: "abcdefgh1234", title: null, form: "app" })} />
    );
    const left = headerRow(container).firstElementChild!;
    const texts = Array.from(left.children).map((el) => el.textContent);
    expect(texts).toEqual(["WorkBuddy", "项目A"]);
    expect(titleRow(container).textContent).toBe("abcdefgh");
  });

  it("实机反馈轮 #1a：会话名尾巴独立第二行，不进首行 flex 信息组", () => {
    const { container } = render(
      <SessionCard session={mk({ title: "很长的会话标题不被挤压", gitBranch: "feat/x" })} />
    );
    const header = headerRow(container);
    const left = header.firstElementChild!;
    // 首行信息组不再包含会话名
    expect(left.textContent).not.toContain("很长的会话标题不被挤压");
    // 尾巴是首行的下一个兄弟（独立行元素），带 truncate + 全文 title
    const tail = titleRow(container);
    expect(header.nextElementSibling).toBe(tail);
    expect(tail.textContent).toBe("很长的会话标题不被挤压");
    expect(tail.className).toContain("truncate");
    expect(tail.getAttribute("title")).toBe("很长的会话标题不被挤压");
    // 旧容器查询隐藏已移除：第二行整行在场，不再依赖 @min-[374px]
    expect(tail.className).not.toContain("hidden");
  });

  it("实机反馈轮 #1b：分支名可截断让路——truncate + min-w-0，且不再 shrink-0", () => {
    const branchName = "feat/desktop-workspace-speed-v2-session-card-two-row";
    const { container } = render(<SessionCard session={mk({ gitBranch: branchName })} />);
    const branch = Array.from(headerRow(container).querySelectorAll("span")).find(
      (el) => el.textContent === branchName
    )!;
    expect(branch.className).toContain("truncate");
    expect(branch.className).toContain("min-w-0");
    expect(branch.className).not.toContain("shrink-0");
    // 悬停可见完整分支名
    expect(branch.getAttribute("title")).toBe(branchName);
  });

  it("实机反馈轮 #1c：首行信息组 overflow-hidden——任何溢出都裁在状态点之前", () => {
    const { container } = render(
      <SessionCard session={mk({ projectName: "很长很长的项目目录名", gitBranch: "main" })} />
    );
    expect(headerRow(container).firstElementChild!.className).toContain("overflow-hidden");
  });

  it("实机反馈三轮 ②（行距 8px 裁决）：行内 mb 全部移除，行距由卡片根 gap-2 统一承担", () => {
    const { container } = render(<SessionCard session={mk({ title: "总结标题" })} />);
    const tail = titleRow(container);
    expect(tail.className).toContain("font-semibold");
    expect(tail.className).not.toMatch(/\bmb-\b/);
    const preview = tail.nextElementSibling!;
    expect(preview.className).not.toMatch(/\bmb-\d/);
    // 卡片根：gap-2 = 8px 统一行距（替代基类 gap-6=24px）
    const card = tail.closest("button") ?? tail.closest('[class*="gap-2"]');
    expect(card?.className).toContain("gap-2");
    expect(card?.className).not.toContain("gap-6");
  });

  it("实机反馈三轮 ②：空白标题不渲染标题行（trim 守卫），id 回退不受影响", () => {
    // 空白标题：标题行（.font-mono.truncate）不渲染（不再留空行占位；titleRow 助手在
    // 行缺席时会返回预览 <p>，此处直查标题行本身）
    const { container: blank } = render(<SessionCard session={mk({ title: "   " })} />);
    expect(blank.querySelector("div.font-mono.truncate")).toBeNull();
    // 无 title 时仍回退 id 前 8 位
    const { container: byId } = render(<SessionCard session={mk({ title: undefined })} />);
    expect(byId.querySelector("div.font-mono.truncate")?.textContent).toBe(
      mk({ title: undefined }).id.slice(0, 8)
    );
  });

  it("未读与状态灯合并为单一指示：无独立小绿点，未读以光环徽章表达", () => {
    const { container } = render(
      <SessionCard session={mk({ status: "idle", form: "app", unread: true })} />
    );
    // 旧独立绿点（bg-emerald-400 圆点）不再渲染
    expect(container.querySelector(".bg-emerald-400")).toBeNull();
    // 未读光环徽章存在（aria-label=Unread），且状态灯仍在
    const ring = screen.getByLabelText("Unread");
    expect(ring.className).toContain("ring-emerald-400/80");
    expect(container.querySelector(".bg-green-500")).not.toBeNull();
    // 未读指示唯一：全卡仅一个 Unread 标签
    expect(screen.getAllByLabelText("Unread")).toHaveLength(1);
  });

  it("已读卡不显示未读光环", () => {
    render(<SessionCard session={mk({ status: "idle", form: "app", unread: false })} />);
    expect(screen.queryByLabelText("Unread")).toBeNull();
  });

  it("未读卡点 X：标记已读且不触发卡片跳转（stopPropagation）", async () => {
    render(<SessionCard session={mk({ status: "idle", form: "app", unread: true })} />);
    fireEvent.click(screen.getByRole("button", { name: /Mark read/i }));
    await waitFor(() =>
      expect(invokeMock).toHaveBeenCalledWith("mark_session_read", {
        agentType: "workbuddy",
        sessionId: "s1",
      })
    );
    expect(invokeMock).not.toHaveBeenCalledWith("focus_session", expect.anything());
  });

  it("活跃 App 卡点 X：dismiss 且不触发卡片跳转（stopPropagation）", async () => {
    render(<SessionCard session={mk({ status: "waiting", form: "app", unread: false })} />);
    fireEvent.click(screen.getByRole("button", { name: /Hide for now/i }));
    await waitFor(() =>
      expect(invokeMock).toHaveBeenCalledWith("dismiss_session_card", {
        agentType: "workbuddy",
        sessionId: "s1",
        status: "waiting",
      })
    );
    expect(invokeMock).not.toHaveBeenCalledWith("focus_session", expect.anything());
  });

  it("CLI 卡：X 已接入（2026-09-14 全形态）、无未读光环，状态灯正常", () => {
    const { container } = render(
      <SessionCard session={mk({ status: "processing", form: "cli", unread: false })} />
    );
    expect(screen.getByRole("button", { name: /Hide for now/i })).not.toBeNull();
    expect(screen.queryByRole("button", { name: /Mark read/i })).toBeNull();
    expect(screen.queryByLabelText("Unread")).toBeNull();
    // 状态灯仍在（黄灯 = 运行中）
    expect(container.querySelector(".bg-yellow-500")).not.toBeNull();
  });

  it("CLI 卡未读语义保留：光环显示，X 走已读（未读为 APP 专用，防御性路径不变）", async () => {
    render(<SessionCard session={mk({ status: "idle", form: "cli", unread: true })} />);
    expect(screen.getByLabelText("Unread")).not.toBeNull();
    fireEvent.click(screen.getByRole("button", { name: /Mark read/i }));
    await waitFor(() =>
      expect(invokeMock).toHaveBeenCalledWith("mark_session_read", {
        agentType: "workbuddy",
        sessionId: "s1",
      })
    );
  });
});
