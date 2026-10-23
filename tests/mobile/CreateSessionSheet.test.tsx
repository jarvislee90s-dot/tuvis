import { act, cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, afterAll, beforeAll, beforeEach, describe, expect, it, vi } from "vitest";
import Board from "@/mobile/Board";
import CreateSessionSheet from "@/mobile/CreateSessionSheet";
import { MockEventSource } from "./eventSourceMock";
import type { CreateProjectView } from "@/mobile/api";
import type { Session } from "@/types/session";

// 文案走 mobile.* 双语键后按 zh 断言（Board.test 同款：显式引入并固定中文）。
// 只 changeLanguage 不够：jsdom 默认 en-US 会在语言重评估时翻回 en——浏览器
// 语言一并钉到 zh（language-toggle 同款）
import { pinZhNavigator } from "./pinZhNavigator";

let unpinZh: () => void;

beforeAll(async () => {
  unpinZh = await pinZhNavigator();
});

afterAll(() => {
  unpinZh();
});

// C11：新建会话 UI（表单 + 进度 + 跳转）。fetch 全量 stub（盖过 setup.ts 的 msw），
// 按 URL 分路到 create-projects / session-create / session-create/status 三族端点
// （判序：/session-create/status 是 /session-create 的前缀扩展，长路径先判）。
// 轮询与等待上板用 fake timers 驱动（2s 轮询节奏、15s 上板时限）。

/** 目录候选夹具（C10 CreateProjectView camelCase 契约） */
function project(overrides: Partial<CreateProjectView> = {}): CreateProjectView {
  return {
    path: "E:\\proj\\alpha",
    lastActiveAt: "2026-10-01T10:00:00Z",
    tools: ["claude", "codex"],
    activeTools: [],
    ...overrides,
  };
}

/** 看板卡夹具（done 后「快照见新卡」用） */
function boardSession(overrides: Partial<Session> & Pick<Session, "id" | "agentType">): Session {
  return {
    projectName: "alpha",
    projectPath: "E:\\proj\\alpha",
    title: null,
    gitBranch: null,
    githubUrl: null,
    status: "processing",
    lastMessage: null,
    lastMessageRole: null,
    lastActivityAt: "2026-10-02T10:00:00Z",
    pid: 1,
    cpuUsage: 0,
    activeSubagentCount: 0,
    form: "cli",
    jumpSupported: false,
    unread: false,
    ...overrides,
  };
}

interface Routes {
  projects?: CreateProjectView[];
  projectsStatus?: number;
  /** POST /session-create 的 200 回执 */
  createAccepted?: { taskId: number; hasActiveSession: boolean };
  /** POST /session-create 非 2xx（400/409）+ 响应体 */
  createStatus?: number;
  createBody?: Record<string, unknown>;
  /** GET /session-create/status 的 200 快照 */
  status?: {
    phase: string;
    detail: string | null;
    sessionId: string | null;
    spawnedPid: number | null;
  };
  statusStatus?: number;
  /** status 端点网络层异常（单拍失败不终止轮询的回归锁） */
  statusReject?: boolean;
  /** POST /session-create 网络层异常（ApiError 分診测试用） */
  createReject?: boolean;
}

let routes: Routes;
let fetchMock: ReturnType<typeof vi.fn>;

function installFetch() {
  fetchMock = vi.fn(async (input: RequestInfo | URL) => {
    const url = String(input);
    if (url.includes("/session-create/status")) {
      if (routes.statusReject) throw new TypeError("network down");
      if (routes.statusStatus) {
        return new Response(JSON.stringify({ error: "no_task" }), { status: routes.statusStatus });
      }
      return new Response(JSON.stringify(routes.status), { status: 200 });
    }
    if (url.includes("/session-create")) {
      if (routes.createReject) throw new TypeError("network down");
      if (routes.createStatus) {
        return new Response(JSON.stringify(routes.createBody ?? {}), {
          status: routes.createStatus,
        });
      }
      return new Response(
        JSON.stringify(routes.createAccepted ?? { taskId: 1, hasActiveSession: false }),
        { status: 200 }
      );
    }
    if (url.includes("/create-projects")) {
      if (routes.projectsStatus) return new Response("", { status: routes.projectsStatus });
      return new Response(JSON.stringify({ projects: routes.projects ?? [] }), { status: 200 });
    }
    return new Response("{}", { status: 404 });
  });
  vi.stubGlobal("fetch", fetchMock);
}

// fake timers 下推进定时器并让微任务落地（act 包裹消状态更新警告）
async function advance(ms: number) {
  await act(async () => {
    await vi.advanceTimersByTimeAsync(ms);
  });
}

/** 取 POST /session-create 的请求体（camelCase 形态断言用；多次提交时取第 n 次，缺省最后一次） */
function createBody(n = -1): Record<string, unknown> {
  const posts = fetchMock.mock.calls.filter(([u]) => String(u) === "/m/api/v1/session-create");
  const call = posts.at(n);
  expect(call).toBeTruthy();
  const init = call[1] as RequestInit;
  return JSON.parse(String(init.body)) as Record<string, unknown>;
}

/** status 端点被轮询的次数 */
function statusCalls(): number {
  return fetchMock.mock.calls.filter(([u]) => String(u).includes("/session-create/status")).length;
}

/** 挂载 → 选工具 claude → 手填路径 → 提交（进度态/错误分診测试的公共前导） */
async function submitWithManualPath(first?: string) {
  render(
    <CreateSessionSheet
      enabledTools={new Set(["claude", "codex", "kimi", "opencode"])}
      installedTools={new Set(["claude", "codex", "kimi", "opencode"])}
      boardSessions={[]}
      onClose={vi.fn()}
    />
  );
  await advance(0);
  fireEvent.click(screen.getByTestId("create-tool-claude"));
  fireEvent.click(screen.getByTestId("create-manual-toggle"));
  fireEvent.change(screen.getByTestId("create-manual-input"), {
    target: { value: "E:\\proj\\alpha" },
  });
  if (first !== undefined) {
    fireEvent.change(screen.getByTestId("create-first-message"), { target: { value: first } });
  }
  fireEvent.click(screen.getByTestId("create-submit"));
  await advance(0); // 提交落地 + 首拍轮询（挂入即拍）
}

beforeEach(() => {
  vi.useFakeTimers();
  // 固定时钟：目录候选的相对时长断言（1 天前）可确定
  vi.setSystemTime(new Date("2026-10-02T10:00:00Z"));
  routes = {};
});
afterEach(() => {
  cleanup();
  vi.useRealTimers();
  vi.unstubAllGlobals();
});

// ==== 表单（工具四选 / 目录候选 / 手填 / 黄字）====
describe("CreateSessionSheet 表单", () => {
  it("工具四选：enabledTools 之外置灰并标「未启用」", async () => {
    installFetch();
    render(
      <CreateSessionSheet
        enabledTools={new Set(["claude"])}
        installedTools={new Set(["claude", "codex", "kimi", "opencode"])}
        boardSessions={[]}
        onClose={vi.fn()}
      />
    );
    await advance(0);
    expect(screen.getByTestId("create-tool-claude")).toBeEnabled();
    expect(screen.getByTestId("create-tool-codex")).toBeDisabled();
    expect(screen.getByTestId("create-tool-codex").textContent).toContain("未启用");
    expect(screen.getByTestId("create-tool-kimi")).toBeDisabled();
    expect(screen.getByTestId("create-tool-opencode")).toBeDisabled();
  });

  it("enabledTools 未到（null）时不猜：四工具全可点（对齐 Board chips 竞态口径）", async () => {
    installFetch();
    render(
      <CreateSessionSheet
        enabledTools={null}
        installedTools={null}
        boardSessions={[]}
        onClose={vi.fn()}
      />
    );
    await advance(0);
    for (const t of ["claude", "codex", "kimi", "opencode"]) {
      expect(screen.getByTestId(`create-tool-${t}`)).toBeEnabled();
    }
  });

  it("installedTools 之外置灰并标「未安装」（P1-9：spec §2 置灰数据源 = enabledTools ∩ 安装探测）", async () => {
    installFetch();
    render(
      <CreateSessionSheet
        enabledTools={new Set(["claude", "codex", "kimi", "opencode"])}
        installedTools={new Set(["claude"])}
        boardSessions={[]}
        onClose={vi.fn()}
      />
    );
    await advance(0);
    expect(screen.getByTestId("create-tool-claude")).toBeEnabled();
    expect(screen.getByTestId("create-tool-codex")).toBeDisabled();
    expect(screen.getByTestId("create-tool-codex").textContent).toContain("未安装");
    expect(screen.getByTestId("create-tool-kimi")).toBeDisabled();
    expect(screen.getByTestId("create-tool-kimi").textContent).toContain("未安装");
    expect(screen.getByTestId("create-tool-opencode")).toBeDisabled();
    expect(screen.getByTestId("create-tool-opencode").textContent).toContain("未安装");
  });

  it("目录候选：条目展示 path + 相对时间 + 工具名；点选后回显已选目录", async () => {
    routes.projects = [
      project({
        path: "E:\\proj\\alpha",
        lastActiveAt: "2026-10-01T10:00:00Z", // 距固定时钟恰好 1 天
        tools: ["claude", "codex"],
      }),
    ];
    installFetch();
    render(
      <CreateSessionSheet
        enabledTools={new Set(["claude", "codex"])}
        installedTools={new Set(["claude", "codex", "kimi", "opencode"])}
        boardSessions={[]}
        onClose={vi.fn()}
      />
    );
    await advance(0);
    const item = screen.getByTestId("create-project-0");
    expect(item.textContent).toContain("E:\\proj\\alpha");
    expect(item.textContent).toContain("1 天前");
    expect(item.textContent).toContain("Claude");
    expect(item.textContent).toContain("Codex");

    fireEvent.click(item);
    expect(screen.getByTestId("create-selected-path").textContent).toContain("E:\\proj\\alpha");
  });

  it("手填切换：文本输入；Windows 盘符非 X:\\ 形态时前端提示（服务端权威校验）", async () => {
    installFetch();
    render(
      <CreateSessionSheet
        enabledTools={new Set(["claude"])}
        installedTools={new Set(["claude", "codex", "kimi", "opencode"])}
        boardSessions={[]}
        onClose={vi.fn()}
      />
    );
    await advance(0);
    fireEvent.click(screen.getByTestId("create-manual-toggle"));
    fireEvent.change(screen.getByTestId("create-manual-input"), {
      target: { value: "E:/proj/beta" },
    });
    expect(screen.getByTestId("create-win-hint").textContent).toContain(
      "Windows 路径须为 X:\\ 形态"
    );
    fireEvent.change(screen.getByTestId("create-manual-input"), {
      target: { value: "E:\\proj\\beta" },
    });
    expect(screen.queryByTestId("create-win-hint")).not.toBeInTheDocument();
  });

  it("黄字按 activeTools 触发（≥1 语义）：选中工具在 activeTools → 显示；不在 → 消失；手填路径无数据 → 不显示", async () => {
    routes.projects = [project({ activeTools: ["claude"] })];
    installFetch();
    render(
      <CreateSessionSheet
        enabledTools={new Set(["claude", "codex"])}
        installedTools={new Set(["claude", "codex", "kimi", "opencode"])}
        boardSessions={[]}
        onClose={vi.fn()}
      />
    );
    await advance(0);
    fireEvent.click(screen.getByTestId("create-tool-claude"));
    fireEvent.click(screen.getByTestId("create-project-0"));
    expect(screen.getByTestId("create-active-hint").textContent).toContain(
      "该项目已有该工具的活跃会话"
    );
    expect(screen.getByTestId("create-active-hint").textContent).toContain(
      "多实例下后续消息路由可能混淆"
    );

    // 切到不在 activeTools 的工具：黄字消失
    fireEvent.click(screen.getByTestId("create-tool-codex"));
    expect(screen.queryByTestId("create-active-hint")).not.toBeInTheDocument();

    // 手填路径：无 activeTools 数据，即使切回 claude 也不显示（与回执黄字分层）
    fireEvent.click(screen.getByTestId("create-tool-claude"));
    fireEvent.click(screen.getByTestId("create-manual-toggle"));
    fireEvent.change(screen.getByTestId("create-manual-input"), {
      target: { value: "E:\\proj\\beta" },
    });
    expect(screen.queryByTestId("create-active-hint")).not.toBeInTheDocument();
  });

  it("提交请求体：camelCase 逐键；首句为空省 firstMessage 键（后端缺省探针 hi），有值透传", async () => {
    installFetch();
    await submitWithManualPath("帮我跑一遍测试");
    expect(createBody()).toEqual({
      tool: "claude",
      projectPath: "E:\\proj\\alpha",
      firstMessage: "帮我跑一遍测试",
    });

    // 取消回表单 → 清空首句重提：firstMessage 键省略（undefined 不序列化）
    fireEvent.click(screen.getByTestId("create-cancel"));
    fireEvent.change(screen.getByTestId("create-first-message"), { target: { value: "" } });
    fireEvent.click(screen.getByTestId("create-submit"));
    await advance(0);
    expect(createBody()).toEqual({ tool: "claude", projectPath: "E:\\proj\\alpha" });
  });
});

// ==== 最近项目快捷 chips（置顶加速器行）====
describe("CreateSessionSheet 最近项目快捷 chips", () => {
  it("有候选 → chips 置顶渲染：文案 = 路径末段，title = 完整路径", async () => {
    routes.projects = [
      project({ path: "E:\\proj\\alpha" }),
      project({ path: "E:\\proj\\beta", lastActiveAt: "2026-09-30T10:00:00Z" }),
    ];
    installFetch();
    render(
      <CreateSessionSheet
        enabledTools={new Set(["claude", "codex"])}
        installedTools={new Set(["claude", "codex", "kimi", "opencode"])}
        boardSessions={[]}
        onClose={vi.fn()}
      />
    );
    await advance(0);
    expect(screen.getByTestId("create-recent-chips")).toBeInTheDocument();
    expect(screen.getByTestId("create-recent-chip-0").textContent).toBe("alpha");
    expect(screen.getByTestId("create-recent-chip-0")).toHaveAttribute("title", "E:\\proj\\alpha");
    expect(screen.getByTestId("create-recent-chip-1").textContent).toBe("beta");
  });

  it("非手填态点 chip → 直接选中该候选（回显 + aria-pressed，与候选列表同口径）", async () => {
    routes.projects = [project()];
    installFetch();
    render(
      <CreateSessionSheet
        enabledTools={new Set(["claude", "codex"])}
        installedTools={new Set(["claude", "codex", "kimi", "opencode"])}
        boardSessions={[]}
        onClose={vi.fn()}
      />
    );
    await advance(0);
    fireEvent.click(screen.getByTestId("create-recent-chip-0"));
    expect(screen.getByTestId("create-selected-path").textContent).toContain("E:\\proj\\alpha");
    expect(screen.getByTestId("create-recent-chip-0")).toHaveAttribute("aria-pressed", "true");
    // 与候选列表选中态同步
    expect(screen.getByTestId("create-project-0")).toHaveAttribute("aria-pressed", "true");
  });

  it("手填态点 chip → 只灌入手填框、不退出手填态（以候选为起点可再微调）", async () => {
    routes.projects = [project({ path: "E:\\proj\\alpha" })];
    installFetch();
    render(
      <CreateSessionSheet
        enabledTools={new Set(["claude", "codex"])}
        installedTools={new Set(["claude", "codex", "kimi", "opencode"])}
        boardSessions={[]}
        onClose={vi.fn()}
      />
    );
    await advance(0);
    fireEvent.click(screen.getByTestId("create-manual-toggle"));
    fireEvent.click(screen.getByTestId("create-recent-chip-0"));
    expect((screen.getByTestId("create-manual-input") as HTMLInputElement).value).toBe(
      "E:\\proj\\alpha"
    );
    expect(screen.getByTestId("create-manual-toggle")).toHaveAttribute("aria-pressed", "true");
    // 手填态不亮 chip 选中（选中权威在输入框，不双高亮）
    expect(screen.getByTestId("create-recent-chip-0")).toHaveAttribute("aria-pressed", "false");
    expect(screen.queryByTestId("create-selected-path")).not.toBeInTheDocument();
  });

  it("候选为空 → chips 整节不渲染（空态不占位，候选区已有文案）", async () => {
    routes.projects = [];
    installFetch();
    render(
      <CreateSessionSheet
        enabledTools={new Set(["claude", "codex"])}
        installedTools={new Set(["claude", "codex", "kimi", "opencode"])}
        boardSessions={[]}
        onClose={vi.fn()}
      />
    );
    await advance(0);
    expect(screen.queryByTestId("create-recent-chips")).not.toBeInTheDocument();
  });
});

// ==== 提交错误分診（400 reasonCode / 409 / 网络异常）====
describe("CreateSessionSheet 提交错误分診", () => {
  it("409 conflict →「已有创建任务进行中」", async () => {
    routes.createStatus = 409;
    installFetch();
    await submitWithManualPath();
    expect(screen.getByTestId("create-error").textContent).toBe("已有创建任务进行中");
    // 仍停留表单态（未进入进度轮询）
    expect(screen.getByTestId("create-submit")).toBeInTheDocument();
    expect(statusCalls()).toBe(0);
  });

  it("400 + reasonCode=blacklisted →「路径命中危险目录黑名单」", async () => {
    routes.createStatus = 400;
    routes.createBody = { reasonCode: "blacklisted" };
    installFetch();
    await submitWithManualPath();
    expect(screen.getByTestId("create-error").textContent).toBe("路径命中危险目录黑名单");
  });

  it("400 + reason 定位详情 → 后端 reason 优先于本地码表（修复批 I1）", async () => {
    routes.createStatus = 400;
    routes.createBody = {
      reasonCode: "blacklisted",
      reason: "路径命中危险目录黑名单（命中段「.ssh」，凭据表）",
    };
    installFetch();
    await submitWithManualPath();
    expect(screen.getByTestId("create-error").textContent).toBe(
      "路径命中危险目录黑名单（命中段「.ssh」，凭据表）"
    );
  });

  it("400 + reasonCode=non_ascii_path →「v1 路径限纯 ASCII」", async () => {
    routes.createStatus = 400;
    routes.createBody = { reasonCode: "non_ascii_path" };
    installFetch();
    await submitWithManualPath();
    expect(screen.getByTestId("create-error").textContent).toBe("v1 路径限纯 ASCII");
  });

  it("400 + reasonCode=path_too_long → 码表映射「路径超长」（评审 P1-7：端点三码全集补齐）", async () => {
    routes.createStatus = 400;
    routes.createBody = { reasonCode: "path_too_long" };
    installFetch();
    await submitWithManualPath();
    expect(screen.getByTestId("create-error").textContent).toBe("路径超长（上限 10000 字符）");
  });

  it("网络/服务异常（ApiError）→ 通用失败文案，仍留表单", async () => {
    routes.createReject = true;
    installFetch();
    await submitWithManualPath();
    expect(screen.getByTestId("create-error").textContent).toContain("创建请求失败");
    expect(screen.getByTestId("create-submit")).toBeInTheDocument();
  });

  it("提交 403（设备失效）→「设备已失效，请重新配对」，留表单（评审修复④）", async () => {
    routes.createStatus = 403;
    installFetch();
    await submitWithManualPath();
    expect(screen.getByTestId("create-error").textContent).toBe("设备已失效，请重新配对");
    expect(screen.getByTestId("create-submit")).toBeInTheDocument();
  });

  it("首句超长（>10000）→ 前端预检提示，不发起请求（评审修复⑥）", async () => {
    installFetch();
    render(
      <CreateSessionSheet
        enabledTools={new Set(["claude", "codex", "kimi", "opencode"])}
        installedTools={new Set(["claude", "codex", "kimi", "opencode"])}
        boardSessions={[]}
        onClose={vi.fn()}
      />
    );
    await advance(0);
    fireEvent.click(screen.getByTestId("create-tool-claude"));
    fireEvent.click(screen.getByTestId("create-manual-toggle"));
    fireEvent.change(screen.getByTestId("create-manual-input"), {
      target: { value: "E:\\proj\\alpha" },
    });
    fireEvent.change(screen.getByTestId("create-first-message"), {
      target: { value: "x".repeat(10001) },
    });
    fireEvent.click(screen.getByTestId("create-submit"));
    await advance(0);
    expect(screen.getByTestId("create-error").textContent).toBe(
      "首句超长（上限 10000 字符），请精简后重试"
    );
    expect(
      fetchMock.mock.calls.filter(([u]) => String(u) === "/m/api/v1/session-create").length
    ).toBe(0);
  });
});

// ==== 进度轮询与终态 ====
describe("CreateSessionSheet 进度轮询", () => {
  it.each([
    ["opening_terminal", "开终端"],
    ["dialog_handling", "处置弹窗"],
    ["injecting_first", "注入首句"],
    ["waiting_materialize", "等待上板"],
    ["done", "完成"],
    ["failed", "失败"],
  ])("phase %s → 中文映射「%s」（逐值经 UI 渲染断言）", async (phase, label) => {
    routes.status = { phase, detail: null, sessionId: null, spawnedPid: null };
    installFetch();
    await submitWithManualPath();
    expect(screen.getByTestId("create-phase").textContent).toBe(label);
  });

  it("2s 节奏轮询：相变实时跟上（opening_terminal → dialog_handling）", async () => {
    routes.status = { phase: "opening_terminal", detail: null, sessionId: null, spawnedPid: null };
    installFetch();
    await submitWithManualPath();
    expect(screen.getByTestId("create-phase").textContent).toBe("开终端");
    routes.status = { phase: "dialog_handling", detail: null, sessionId: null, spawnedPid: null };
    await advance(2000);
    expect(screen.getByTestId("create-phase").textContent).toBe("处置弹窗");
  });

  it("detail 有值即展示——done 相也展示（codex hooks 信任提示挂在 done.detail，C10 交接）", async () => {
    routes.status = {
      phase: "done",
      detail: "codex 需在 TUI 内 /hooks 审阅并信任 兔维斯 钩子一次，事件才会触发",
      sessionId: null,
      spawnedPid: 4242,
    };
    installFetch();
    await submitWithManualPath();
    expect(screen.getByTestId("create-phase").textContent).toBe("完成");
    expect(screen.getByTestId("create-detail").textContent).toContain(
      "codex 需在 TUI 内 /hooks 审阅并信任 兔维斯 钩子一次"
    );
  });

  it("回执黄字（hasActiveSession=true）在进度态展示，不拦截", async () => {
    routes.createAccepted = { taskId: 3, hasActiveSession: true };
    routes.status = { phase: "opening_terminal", detail: null, sessionId: null, spawnedPid: null };
    installFetch();
    await submitWithManualPath();
    expect(screen.getByTestId("create-active-hint").textContent).toContain(
      "该项目已有该工具的活跃会话"
    );
  });

  it("轮询单拍网络失败不终止轮询（任务仍在主机侧跑，下一拍恢复）", async () => {
    routes.status = { phase: "opening_terminal", detail: null, sessionId: null, spawnedPid: null };
    routes.statusReject = true;
    installFetch();
    await submitWithManualPath();
    // 单拍失败：相未知 → 如实显示占位，不假装有进度
    expect(screen.getByTestId("create-phase").textContent).toBe("正在获取进度…");
    routes.statusReject = false;
    await advance(2000);
    expect(screen.getByTestId("create-phase").textContent).toBe("开终端");
  });

  it("跟踪满 120s 无终态 → 停滞提示亮起且轮询不停（评审 P1-8：主机进程死亡收不到 failed 的场景说破）", async () => {
    routes.status = {
      phase: "waiting_materialize",
      detail: null,
      sessionId: null,
      spawnedPid: null,
    };
    installFetch();
    await submitWithManualPath();
    // 119s：预算+裕量内，不提示
    await advance(119_000);
    expect(screen.queryByTestId("create-stalled-hint")).not.toBeInTheDocument();
    // 推过 120s 阈值（下个 2s 拍 >120s）：提示亮起
    await advance(5_000);
    expect(screen.getByTestId("create-stalled-hint").textContent).toContain("创建耗时已超 2 分钟");
    // 提示不等于停拍：轮询照旧 2s 推进
    const calls = statusCalls();
    await advance(4_000);
    expect(statusCalls()).toBeGreaterThan(calls);
  });

  it("终态先到 → 停滞提示永不亮（阈值只作用于非终态跟踪）", async () => {
    routes.status = { phase: "failed", detail: "现场", sessionId: null, spawnedPid: null };
    installFetch();
    await submitWithManualPath();
    await advance(130_000);
    expect(screen.queryByTestId("create-stalled-hint")).not.toBeInTheDocument();
  });

  it("404 no_task →「任务已失效（主机可能重启），请重试」，停轮询，返回重试保留已填项", async () => {
    routes.statusStatus = 404;
    installFetch();
    await submitWithManualPath("第一句");
    expect(screen.getByTestId("create-error").textContent).toBe(
      "任务已失效（主机可能重启），请重试"
    );
    const calls = statusCalls();
    await advance(6000);
    expect(statusCalls()).toBe(calls); // 停轮询

    fireEvent.click(screen.getByTestId("create-retry"));
    expect(screen.getByTestId("create-submit")).toBeInTheDocument();
    expect((screen.getByTestId("create-first-message") as HTMLInputElement).value).toBe("第一句");
    expect((screen.getByTestId("create-manual-input") as HTMLInputElement).value).toBe(
      "E:\\proj\\alpha"
    );
  });

  it("轮询 403（设备失效）→「设备已失效，请重新配对」，停拍（评审修复④）", async () => {
    routes.statusStatus = 403;
    installFetch();
    await submitWithManualPath("第一句");
    expect(screen.getByTestId("create-error").textContent).toBe("设备已失效，请重新配对");
    const calls = statusCalls();
    await advance(6000);
    expect(statusCalls()).toBe(calls); // 停拍：403 空转无意义
  });
});

// ==== 终态分支（failed 重试 / done 跳转 / 取消）====
describe("CreateSessionSheet 终态与跳转", () => {
  it("failed：展示后端中文 detail（分阶段原因），重试回表单且已填项保留，停轮询", async () => {
    routes.status = {
      phase: "failed",
      detail: "终端启动失败：terminal not found（终端窗口保留供查看现场）",
      sessionId: null,
      spawnedPid: null,
    };
    installFetch();
    await submitWithManualPath("第一句");
    expect(screen.getByTestId("create-phase").textContent).toBe("失败");
    expect(screen.getByTestId("create-detail").textContent).toContain("终端启动失败");
    const calls = statusCalls();
    await advance(6000);
    expect(statusCalls()).toBe(calls); // 终态停轮询

    fireEvent.click(screen.getByTestId("create-retry"));
    expect(screen.getByTestId("create-submit")).toBeInTheDocument();
    expect(screen.getByTestId("create-tool-claude")).toHaveAttribute("aria-pressed", "true");
    expect((screen.getByTestId("create-first-message") as HTMLInputElement).value).toBe("第一句");
    expect((screen.getByTestId("create-manual-input") as HTMLInputElement).value).toBe(
      "E:\\proj\\alpha"
    );
  });

  it("done + sessionId：看板快照见新卡 → 以真实 Session 对象触发跳转回调（复用既有导航）并关闭", async () => {
    routes.status = {
      phase: "waiting_materialize",
      detail: null,
      sessionId: null,
      spawnedPid: null,
    };
    installFetch();
    const onOpenSession = vi.fn();
    const onClose = vi.fn();
    const view = render(
      <CreateSessionSheet
        enabledTools={new Set(["claude"])}
        installedTools={new Set(["claude", "codex", "kimi", "opencode"])}
        boardSessions={[]}
        onOpenSession={onOpenSession}
        onClose={onClose}
      />
    );
    await advance(0);
    fireEvent.click(screen.getByTestId("create-tool-claude"));
    fireEvent.click(screen.getByTestId("create-manual-toggle"));
    fireEvent.change(screen.getByTestId("create-manual-input"), {
      target: { value: "E:\\proj\\alpha" },
    });
    fireEvent.click(screen.getByTestId("create-submit"));
    await advance(0);
    expect(screen.getByTestId("create-phase").textContent).toBe("等待上板");

    // 下一拍 done + sessionId：快照尚无新卡 → 不跳转，等待
    routes.status = { phase: "done", detail: null, sessionId: "s9", spawnedPid: 1 };
    await advance(2000);
    expect(screen.getByTestId("create-phase").textContent).toBe("完成");
    expect(onOpenSession).not.toHaveBeenCalled();

    // 看板快照出现新卡（boardSessions prop 更新）→ 跳转 + 关闭
    view.rerender(
      <CreateSessionSheet
        enabledTools={new Set(["claude"])}
        installedTools={new Set(["claude", "codex", "kimi", "opencode"])}
        boardSessions={[boardSession({ id: "s9", agentType: "claude" })]}
        onOpenSession={onOpenSession}
        onClose={onClose}
      />
    );
    await advance(0);
    expect(onOpenSession).toHaveBeenCalledTimes(1);
    expect(onOpenSession.mock.calls[0][0]).toMatchObject({ id: "s9", agentType: "claude" });
    expect(onClose).toHaveBeenCalledTimes(1);
  });

  it("done 后 15s 仍未上板：不伪造跳转，如实提示去看板查看；上板后仍自动跳", async () => {
    routes.status = { phase: "done", detail: null, sessionId: "s9", spawnedPid: 1 };
    installFetch();
    const onOpenSession = vi.fn();
    const view = render(
      <CreateSessionSheet
        enabledTools={new Set(["claude"])}
        installedTools={new Set(["claude", "codex", "kimi", "opencode"])}
        boardSessions={[]}
        onOpenSession={onOpenSession}
        onClose={vi.fn()}
      />
    );
    await advance(0);
    fireEvent.click(screen.getByTestId("create-tool-claude"));
    fireEvent.click(screen.getByTestId("create-manual-toggle"));
    fireEvent.change(screen.getByTestId("create-manual-input"), {
      target: { value: "E:\\proj\\alpha" },
    });
    fireEvent.click(screen.getByTestId("create-submit"));
    await advance(0);
    expect(screen.getByTestId("create-phase").textContent).toBe("完成");

    await advance(15_000 + 100);
    expect(screen.getByTestId("create-wait-hint").textContent).toContain("尚未在看板出现");
    expect(onOpenSession).not.toHaveBeenCalled();

    // 迟到的上板仍触发跳转（提示不阻断既有导航路径）
    view.rerender(
      <CreateSessionSheet
        enabledTools={new Set(["claude"])}
        installedTools={new Set(["claude", "codex", "kimi", "opencode"])}
        boardSessions={[boardSession({ id: "s9", agentType: "claude" })]}
        onOpenSession={onOpenSession}
        onClose={vi.fn()}
      />
    );
    await advance(0);
    expect(onOpenSession).toHaveBeenCalledTimes(1);
  });

  it("轮询中取消：回表单 + 如实提示「主机侧创建仍将继续」，轮询停止", async () => {
    routes.status = { phase: "dialog_handling", detail: null, sessionId: null, spawnedPid: null };
    installFetch();
    await submitWithManualPath();
    expect(screen.getByTestId("create-phase").textContent).toBe("处置弹窗");
    const calls = statusCalls();
    fireEvent.click(screen.getByTestId("create-cancel"));
    expect(screen.getByTestId("create-submit")).toBeInTheDocument();
    expect(screen.getByTestId("create-notice").textContent).toContain("主机侧创建仍将继续");
    await advance(6000);
    expect(statusCalls()).toBe(calls); // 停轮询
  });
});

// ==== Board 头部入口（C11 集成面）====
describe("Board 新建会话入口", () => {
  it("头部「新建」打开创建面板（受管名单透传置灰）；关闭回看板", async () => {
    class ScriptedEventSource extends MockEventSource {
      constructor(url: string) {
        super(url);
        setTimeout(
          () => this.emit("snapshot", { sessions: [], totalCount: 0, waitingCount: 0 }),
          0
        );
      }
    }
    vi.stubGlobal("EventSource", ScriptedEventSource);
    const f = vi.fn(async (input: RequestInfo | URL) => {
      const url = String(input);
      if (url === "/m/api/v1/host") {
        return new Response(
          JSON.stringify({
            host: { name: "JARVIS-Win", platform: "windows", version: "0.5.0" },
            enabledTools: ["claude"],
            installedTools: ["claude", "codex", "kimi", "opencode"],
          }),
          { status: 200 }
        );
      }
      if (url.includes("/create-projects")) {
        return new Response(JSON.stringify({ projects: [] }), { status: 200 });
      }
      return new Response(JSON.stringify({ sessions: [], totalCount: 0, waitingCount: 0 }), {
        status: 200,
      });
    });
    vi.stubGlobal("fetch", f);

    render(<Board onPaired={vi.fn()} onUnpaired={vi.fn()} onOpenHistory={() => {}} />);
    await advance(0);
    expect(screen.queryByTestId("create-sheet")).not.toBeInTheDocument();

    fireEvent.click(screen.getByTestId("create-open"));
    expect(screen.getByTestId("create-sheet")).toBeInTheDocument();
    await advance(0); // 候选拉取落地
    expect(f.mock.calls.some(([u]) => String(u).includes("/create-projects"))).toBe(true);
    // 受管名单自 host 透传：claude 可点，codex 置灰标「未启用」
    expect(screen.getByTestId("create-tool-claude")).toBeEnabled();
    expect(screen.getByTestId("create-tool-codex")).toBeDisabled();

    fireEvent.click(screen.getByTestId("create-close"));
    expect(screen.queryByTestId("create-sheet")).not.toBeInTheDocument();
  });
});
