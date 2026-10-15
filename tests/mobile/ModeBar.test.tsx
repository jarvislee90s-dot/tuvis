import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import ModeBar from "@/mobile/ModeBar";
import type { MamMode, SessionModeView } from "@/mobile/api";

// 批次丙 T6 + 丁T4：模式栏。覆盖——
// ① 显示当前档（屏读回显成功时）；② 档未知时**必须**显示人工核对提示（红线 4）；
// ③ unsupported 工具不渲染；④ 切档回执 verified=false → 人工核对文案；
// 丁T4 新增：⑤ 二维家（codex/kimi）两组按钮与各自的当前档；⑥ 单轴家切换钮 + 回显；
// ⑦ 裁6 opencode「默认」（Build 已术语对齐）；⑧ 裁7 退役档不作可点按钮；
// ⑨ 旧后端（无 groups 字段）回落单轴渲染且 POST 不带 group；
// 2026-10-10 二版用户指令：⑩ codex 模式组两档 chips 即按钮（当前档高亮**禁用**、
// 另一档可点 = 发一次 shift+tab，无零投递闸）；⑪ 权限组四档 chips 常驻直选（生效档
// 高亮框），「切换权限」picker 入口/菜单面板与「已退役」说明行**已删**（chips 高亮
// 已承载全部状态信息）。picker 菜单端点（/session-mode/menu）的交互用例随之删除。

interface Routes {
  mode?: SessionModeView;
  modeStatus?: number;
  modeNetworkFail?: boolean;
  switchBody?: Record<string, unknown>;
  switchStatus?: number;
}

let routes: Routes;
let fetchMock: ReturnType<typeof vi.fn>;

beforeEach(() => {
  routes = {};
});
afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
});

function installFetch() {
  fetchMock = vi.fn(async (input: RequestInfo | URL) => {
    const url = String(input);
    if (url.includes("/session-mode/switch")) {
      if (routes.switchStatus) {
        return new Response(JSON.stringify(routes.switchBody ?? { error: "internal" }), {
          status: routes.switchStatus,
        });
      }
      return new Response(
        JSON.stringify(routes.switchBody ?? { status: "key_sent", verified: false, hint: null }),
        { status: 200 }
      );
    }
    if (url.includes("/session-mode")) {
      if (routes.modeNetworkFail) throw new TypeError("network down");
      if (routes.modeStatus) return new Response("no", { status: routes.modeStatus });
      return new Response(JSON.stringify(routes.mode ?? opencodePlan()), { status: 200 });
    }
    throw new Error(`unexpected fetch: ${url}`);
  });
  vi.stubGlobal("fetch", fetchMock);
}

/** 旧后端形态（无 groups/structure）——opencode 单轴 Plan 档 */
function opencodePlan(): SessionModeView {
  return {
    tool: "opencode",
    current: "plan",
    currentLabel: "计划",
    readback: true,
    switchKind: "shiftTab",
  };
}

/** 丁T4 新后端形态：单轴（opencode，默认档） */
function opencodeSingleAxis(): SessionModeView {
  return {
    tool: "opencode",
    current: "default",
    currentLabel: "默认",
    readback: true,
    switchKind: "shiftTab",
    structure: "singleAxis",
    groups: [
      {
        id: "mode",
        label: "模式",
        step: true,
        readback: true,
        current: "default",
        currentLabel: "默认",
        tiers: [
          { mode: "default", label: "默认", selectable: true },
          { mode: "plan", label: "计划", selectable: true },
        ],
        legacy: [],
      },
    ],
  };
}

/** 丁T4 新后端形态：二维（codex）。2026-10-10 二版用户指令改版后：
 *  模式组 = 两档 chips 即按钮（[计划][操作]——当前档高亮禁用，另一档可点 = 向终端
 *  发一次 shift+tab，无零投递闸）；权限组 = 四档 chips 常驻直选（「切换权限」picker
 *  面板与退役说明行已删）。
 *  `modeCurrent` 驱动 toggle chips 的高亮与禁用；`permissionCurrent` 用来测 chips
 *  高亮与「上次切换」记忆标注（null = 无记忆 → 模式未知）。
 *  终审 P1-2 mock 改写：权限组回读源 2026-10-09 起 readback=true（最新回执行），
 *  旧 mock 的 `readback: false` 过期；「上次切换」标注改由 `currentSource` 驱动
 *  （默认：有记忆 = "memory"、无 = "null"；可显式传 "screen" 测屏读来源不标注）。
 *  legacy 数组**保留** untrusted/on-failure 两条——二版起前端不消费但后端仍下发，
 *  用它锁「给了退役数据也不渲染」。 */
function codexTwoAxis(opts?: {
  modeCurrent?: MamMode;
  permissionCurrent?: MamMode | null;
  permissionCurrentSource?: "screen" | "memory" | "null";
}): SessionModeView {
  const modeCurrent = opts?.modeCurrent ?? "plan";
  const permissionCurrent = opts?.permissionCurrent ?? null;
  const permissionCurrentSource =
    opts?.permissionCurrentSource ?? (permissionCurrent === null ? "null" : "memory");
  return {
    tool: "codex",
    current: modeCurrent,
    currentLabel: modeCurrent === "plan" ? "计划" : "操作",
    readback: true,
    switchKind: "slashCommand",
    structure: "twoAxis",
    groups: [
      {
        id: "mode",
        label: "模式",
        step: false,
        readback: true,
        layout: "toggle",
        current: modeCurrent,
        currentLabel: modeCurrent === "plan" ? "计划" : "操作",
        currentSource: "screen",
        tiers: [
          { mode: "default", label: "操作", selectable: true },
          { mode: "plan", label: "计划", selectable: true },
        ],
        legacy: [],
      },
      {
        id: "permission",
        label: "权限",
        step: false,
        readback: true,
        layout: "picker",
        current: permissionCurrent,
        currentLabel: permissionCurrent === null ? null : "只读",
        currentSource: permissionCurrentSource,
        tiers: [
          { mode: "readOnly", label: "只读", selectable: true },
          { mode: "default", label: "默认", selectable: true },
          { mode: "acceptEdits", label: "自动审批", selectable: true },
          { mode: "bypass", label: "完全信任", selectable: true },
        ],
        legacy: [
          { label: "untrusted", note: "官方 0.149.0 起退役（配置即拒启）——不作为可选档" },
          { label: "on-failure", note: "官方已弃用（deprecated）——不作为可选档" },
        ],
      },
    ],
  };
}

async function flushAsync() {
  for (let i = 0; i < 6; i += 1) {
    await Promise.resolve();
  }
}

describe("ModeBar：模式显示与切档（批次丙 T6）", () => {
  it("显示当前档（opencode 屏读回显成功）：档名可见 + 切换模式按钮", async () => {
    installFetch();
    render(<ModeBar session={{ id: "s1" }} />);
    expect(await screen.findByTestId("mode-bar")).toBeTruthy();
    expect(screen.getByTestId("mode-bar").getAttribute("data-mode")).toBe("plan");
    expect(screen.getByTestId("mode-current-mode").textContent).toBe("计划");
    // 回显成功 → 无「人工核对」提示
    expect(screen.queryByTestId("mode-unknown-hint-mode")).toBeNull();
    // shiftTab 档 → 单钮「切换模式」（循环切一档，不提供直达）
    expect(screen.getByTestId("mode-switch-next-mode")).toBeTruthy();
  });

  it("档未知（屏读失败/不支持回显）：显示「模式未知」+ 必须给人工核对提示（红线 4）", async () => {
    installFetch();
    routes.mode = {
      tool: "claude",
      current: null,
      currentLabel: null,
      readback: false,
      switchKind: "shiftTab",
    };
    render(<ModeBar session={{ id: "s2" }} />);
    expect(await screen.findByTestId("mode-bar")).toBeTruthy();
    expect(screen.getByTestId("mode-bar").getAttribute("data-mode")).toBe("unknown");
    expect(screen.getByTestId("mode-current-mode").textContent).toBe("模式未知");
    // 红线 4：不假装成功——必须有核对提示
    expect(screen.getByTestId("mode-unknown-hint-mode").textContent).toContain("人工核对");
    // 切换入口仍在（盲切是允许的，只是如实标注不可验证）
    expect(screen.getByTestId("mode-switch-next-mode")).toBeTruthy();
  });

  it("切档 verified=false：回执为人工核对提示，不声称已切到目标档", async () => {
    installFetch();
    routes.mode = {
      tool: "claude",
      current: null,
      currentLabel: null,
      readback: false,
      switchKind: "shiftTab",
    };
    routes.switchBody = {
      status: "key_sent",
      verified: false,
      hint: "该工具的模式回显未实测，请人工核对终端当前模式",
    };
    render(<ModeBar session={{ id: "s3" }} />);
    fireEvent.click(await screen.findByTestId("mode-switch-next-mode"));
    const receipt = await screen.findByTestId("mode-receipt");
    expect(receipt.textContent).toContain("人工核对");
    expect(receipt.textContent).not.toContain("已切换到");
    // POST 体正确
    const call = fetchMock.mock.calls.find((c: unknown[]) =>
      String(c[0]).includes("/session-mode/switch")
    );
    const body = JSON.parse(String((call![1] as RequestInit).body));
    expect(body.sessionId).toBe("s3");
    expect(body.target).toBe("default");
  });

  it("unsupported 工具：不渲染（无实测切换机制）", async () => {
    installFetch();
    routes.mode = {
      tool: "zcode",
      current: null,
      currentLabel: null,
      readback: false,
      switchKind: "unsupported",
    };
    const { container } = render(<ModeBar session={{ id: "s5" }} />);
    await flushAsync();
    expect(container.firstChild).toBeNull();
    expect(screen.queryByTestId("mode-bar")).toBeNull();
  });

  it("拉取失败：静默自隐（不渲染、不报错）", async () => {
    installFetch();
    routes.modeNetworkFail = true;
    const { container } = render(<ModeBar session={{ id: "s6" }} />);
    await flushAsync();
    expect(container.firstChild).toBeNull();
  });

  it("409 no_mechanism：显示中文文案「该工具的模式切换未实测」", async () => {
    installFetch();
    routes.mode = codexTwoAxis();
    routes.switchStatus = 409;
    routes.switchBody = { error: "no_mechanism" };
    render(<ModeBar session={{ id: "s7" }} />);
    // 二版 chips：current=plan → 点另一档「操作」chip 发切换
    fireEvent.click(await screen.findByTestId("mode-switch-mode-default"));
    expect((await screen.findByTestId("mode-error")).textContent).toContain("未实测");
  });
});

// ==== 丁T3 §2.7：对话框在场红线（裁8/9，问题 5）====
describe("乙T3 对话框在场拒绝对接（blocked_by_dialog）", () => {
  it("409 blocked_by_dialog：显示后端 reason 原文「终端有待决对话框，请先处理」", async () => {
    installFetch();
    routes.mode = {
      tool: "claude",
      current: null,
      currentLabel: null,
      readback: false,
      switchKind: "shiftTab",
    };
    routes.switchStatus = 409;
    routes.switchBody = {
      error: "blocked_by_dialog",
      reason: "终端有待决对话框，请先处理",
    };
    render(<ModeBar session={{ id: "s8" }} />);
    fireEvent.click(await screen.findByTestId("mode-switch-next-mode"));
    const err = await screen.findByTestId("mode-error");
    expect(err.textContent).toBe("终端有待决对话框，请先处理");
    // 不得误报为「已发送切换」/「未实测」——拒绝语义必须与另两态可分
    expect(screen.queryByTestId("mode-receipt")).toBeNull();
  });

  it("blocked_by_dialog 无 reason 字段（旧后端/异体）→ 前端兜底同义文案，不显示空串", async () => {
    installFetch();
    routes.mode = {
      tool: "claude",
      current: null,
      currentLabel: null,
      readback: false,
      switchKind: "shiftTab",
    };
    routes.switchStatus = 409;
    routes.switchBody = { error: "blocked_by_dialog" };
    render(<ModeBar session={{ id: "s9" }} />);
    fireEvent.click(await screen.findByTestId("mode-switch-next-mode"));
    expect((await screen.findByTestId("mode-error")).textContent).toBe(
      "终端有待决对话框，请先处理"
    );
  });
});

// ==== 丁T4 §2.6：二维结构 / 单轴 / 裁6 / 裁7 ====
describe("丁T4 模式二维与回读（§2.6 规格表）", () => {
  it("二维家（codex）：渲染模式组 + 权限组两组；模式组当前档 chip data-current，权限组 current=null（屏上无回执行）→ 人工核对", async () => {
    installFetch();
    routes.mode = codexTwoAxis();
    render(<ModeBar session={{ id: "t1" }} />);
    expect(await screen.findByTestId("mode-bar")).toBeTruthy();
    // 结构标记（前端渲染分支的判据）
    expect(screen.getByTestId("mode-bar").getAttribute("data-structure")).toBe("twoAxis");
    // 两组都在
    expect(screen.getByTestId("mode-group-mode")).toBeTruthy();
    expect(screen.getByTestId("mode-group-permission")).toBeTruthy();
    // 模式组：回读命中「计划」（chips 高亮承载，2026-10-10 二版移除文本回显）；
    // 权限组：屏上无回执行（current=null）→ 模式未知 + 人工核对
    expect(screen.getByTestId("mode-switch-mode-plan").getAttribute("data-current")).toBe("true");
    expect(screen.getByTestId("mode-switch-mode-default").getAttribute("data-current")).toBe(
      "false"
    );
    expect(screen.getByTestId("mode-unknown-hint-permission").textContent).toContain("人工核对");
    // 组标题可见（两组才显示，帮助用户区分两个轴）
    expect(screen.getByTestId("mode-group-permission").textContent).toContain("权限");
  });

  it("二维家：权限组 = 四档 chips 常驻直选；「切换权限」picker 入口已删（2026-10-10 二版）", async () => {
    installFetch();
    routes.mode = codexTwoAxis();
    render(<ModeBar session={{ id: "t2" }} />);
    await screen.findByTestId("mode-bar");
    // 四档 chips 常驻（来自 tiers 里 selectable 档）
    expect(screen.getByTestId("mode-switch-permission-readOnly")).toBeTruthy();
    expect(screen.getByTestId("mode-switch-permission-default")).toBeTruthy();
    expect(screen.getByTestId("mode-switch-permission-acceptEdits")).toBeTruthy();
    expect(screen.getByTestId("mode-switch-permission-bypass")).toBeTruthy();
    // 二版指令：picker 入口按钮与菜单面板不再渲染（chips 高亮已承载全部状态信息）
    expect(screen.queryByTestId("mode-picker-permission-open")).toBeNull();
    expect(screen.queryByTestId("mode-menu-panel")).toBeNull();
  });

  it("裁7 + 二版：codex 退役旧档（untrusted/on-failure）**不渲染**——既无可点按钮也无「已退役」说明行（legacy 数据仍下发）", async () => {
    installFetch();
    routes.mode = codexTwoAxis(); // mock 的 legacy 数组仍带 untrusted/on-failure 两条
    render(<ModeBar session={{ id: "t3" }} />);
    await screen.findByTestId("mode-bar");
    // 没有任何元素的 testid 含 untrusted / on-failure
    const ids = Array.from(document.querySelectorAll("[data-testid]")).map((el) =>
      el.getAttribute("data-testid")
    );
    expect(ids.some((id) => id !== null && /untrusted|on-failure/i.test(id))).toBe(false);
    // 二版指令：说明行 mode-legacy-permission 已删——后端仍下发 legacy，前端不消费
    expect(screen.queryByTestId("mode-legacy-permission")).toBeNull();
  });

  it("2026-10-10 二版：codex 模式组 = 两档 chips 即按钮（[计划][操作]），三段 [◀▶] 形态已删", async () => {
    installFetch();
    routes.mode = codexTwoAxis();
    render(<ModeBar session={{ id: "t4" }} />);
    await screen.findByTestId("mode-bar");
    // 两颗 chip 按钮（标签来自后端 tiers 的屏显标签）
    expect(screen.getByTestId("mode-switch-mode-plan").textContent).toBe("计划");
    expect(screen.getByTestId("mode-switch-mode-default").textContent).toBe("操作");
    // 旧三段形态的元素全部退役：「切换模式」钮 + 双标签指示器 + 文本回显 + 「不可用」档
    expect(screen.queryByTestId("mode-switch-mode-toggle")).toBeNull();
    expect(screen.queryByTestId("mode-toggle-label-mode-plan")).toBeNull();
    expect(screen.queryByTestId("mode-toggle-label-mode-default")).toBeNull();
    expect(screen.queryByTestId("mode-current-mode")).toBeNull();
    expect(screen.queryByTestId("mode-tier-disabled-mode-default")).toBeNull();
  });

  it("二版：模式组当前档 chip 高亮且禁用（data-current=true + disabled），另一档 enabled", async () => {
    installFetch();
    routes.mode = codexTwoAxis(); // mode 组 current = plan
    render(<ModeBar session={{ id: "t4-hl" }} />);
    await screen.findByTestId("mode-bar");
    const plan = screen.getByTestId("mode-switch-mode-plan") as HTMLButtonElement;
    const op = screen.getByTestId("mode-switch-mode-default") as HTMLButtonElement;
    expect(plan.getAttribute("data-current")).toBe("true");
    expect(plan.disabled).toBe(true, "当前档 chip 不可点（点另一档=切过去）");
    expect(op.getAttribute("data-current")).toBe("false");
    expect(op.disabled).toBe(false);
    // 样式统一（chipClass）：当前档高亮框 + 加粗——toggle/picker 两组同款
    expect(plan.className).toContain("font-semibold");
    expect(op.className).not.toContain("font-semibold");
    // 评审 P2-1：disabled 降透明只压**非当前档**——当前档高亮不被 40% 压暗
    expect(plan.className).not.toContain("disabled:opacity-40");
    expect(op.className).toContain("disabled:opacity-40");
  });

  it("裁6 + 单轴：opencode 两档渲染「默认」/「计划」+ 切换钮 + 当前档回显", async () => {
    installFetch();
    routes.mode = opencodeSingleAxis();
    render(<ModeBar session={{ id: "t5" }} />);
    await screen.findByTestId("mode-bar");
    expect(screen.getByTestId("mode-bar").getAttribute("data-structure")).toBe("singleAxis");
    // 当前档回显：默认（**不是** Build——裁6 术语对齐）
    expect(screen.getByTestId("mode-current-mode").textContent).toBe("默认");
    expect(screen.queryByText(/Build/)).toBeNull();
    // 单轴家仍渲染唯一的「切换模式」钮（不渲染逐档直达钮——shift+tab 一步一档）
    expect(screen.getByTestId("mode-switch-next-mode")).toBeTruthy();
    expect(screen.queryByTestId("mode-switch-mode-plan")).toBeNull();
  });

  it("单轴家只有一组 → 不渲染组标题（避免噪音）；二维家渲染", async () => {
    installFetch();
    routes.mode = opencodeSingleAxis();
    const { unmount } = render(<ModeBar session={{ id: "t6" }} />);
    await screen.findByTestId("mode-bar");
    expect(screen.getByTestId("mode-group-mode").textContent).not.toContain("模式模式");
    unmount();
  });

  it("旧后端（无 groups/structure）回落单轴渲染，且 POST **不带 group**（前向兼容）", async () => {
    installFetch();
    routes.mode = opencodePlan(); // 旧形态
    routes.switchBody = { status: "key_sent", verified: false, hint: "请人工核对" };
    render(<ModeBar session={{ id: "t7" }} />);
    await screen.findByTestId("mode-bar");
    expect(screen.getByTestId("mode-bar").getAttribute("data-structure")).toBe("legacy");
    expect(screen.getByTestId("mode-current-mode").textContent).toBe("计划");
    fireEvent.click(screen.getByTestId("mode-switch-next-mode"));
    await flushAsync();
    const call = fetchMock.mock.calls.find((c: unknown[]) =>
      String(c[0]).includes("/session-mode/switch")
    );
    const body = JSON.parse(String((call![1] as RequestInit).body));
    expect(body.group).toBeUndefined();
    expect(body.target).toBe("default");
  });

  it("回读命中（verified=true）：回执「已切换」+ 重拉 GET 刷新显示", async () => {
    installFetch();
    routes.mode = opencodeSingleAxis();
    routes.switchBody = {
      status: "key_sent",
      verified: true,
      hint: null,
      current: "plan",
      currentLabel: "计划",
    };
    render(<ModeBar session={{ id: "t8" }} />);
    fireEvent.click(await screen.findByTestId("mode-switch-next-mode"));
    expect((await screen.findByTestId("mode-receipt")).textContent).toBe("已切换");
    // 重拉了一次 GET（切换后确认新档）
    const gets = fetchMock.mock.calls.filter(
      (c: unknown[]) => String(c[0]).includes("/session-mode?") && !String(c[0]).includes("switch")
    );
    expect(gets.length).toBeGreaterThanOrEqual(2);
  });

  it("回读与预期不符（verified=false 带预期/实际）：回执原样透出后端 hint", async () => {
    installFetch();
    routes.mode = opencodeSingleAxis();
    routes.switchBody = {
      status: "key_sent",
      verified: false,
      hint: "回读到的档与预期不符（预期「计划」、实际「默认」）——请人工核对终端",
    };
    render(<ModeBar session={{ id: "t9" }} />);
    fireEvent.click(await screen.findByTestId("mode-switch-next-mode"));
    const receipt = await screen.findByTestId("mode-receipt");
    expect(receipt.textContent).toContain("预期「计划」");
    expect(receipt.textContent).toContain("实际「默认」");
    expect(receipt.textContent).not.toBe("已切换");
  });

  it("mode_switch_busy（codex 运行中）：failed 回执进 error 区，不进 receipt", async () => {
    installFetch();
    routes.mode = codexTwoAxis();
    routes.switchBody = {
      status: "failed",
      error: "codex 运行中不接受模式切换（shift+tab），请等回合结束后重试",
    };
    render(<ModeBar session={{ id: "t10" }} />);
    // 二版 chips：current=plan → 点另一档「操作」chip 发切换
    fireEvent.click(await screen.findByTestId("mode-switch-mode-default"));
    const err = await screen.findByTestId("mode-error");
    expect(err.textContent).toContain("运行中不接受模式切换");
    expect(screen.queryByTestId("mode-receipt")).toBeNull();
  });
});

// ==== 批次戊 E3④：当前档高亮 + 问答待决置灰 ====
describe("ModeBar：E3④ 当前档高亮与待决置灰", () => {
  it("当前档高亮：toggle 当前 chip data-current=true；权限组生效档 chips 高亮框选中（memory 来源仍标「上次切换」）", async () => {
    installFetch();
    // 终审 P1-2：标注判据 = currentSource === "memory"（显式传入驱动本用例）
    routes.mode = codexTwoAxis({
      permissionCurrent: "readOnly",
      permissionCurrentSource: "memory",
    });
    render(<ModeBar session={{ id: "s-e3-hl" }} />);
    await screen.findByTestId("mode-switch-permission-readOnly");
    // 模式组：当前档（plan）chip 高亮，另一档不高亮（chips 即指示器，二版）
    expect(screen.getByTestId("mode-switch-mode-plan").getAttribute("data-current")).toBe("true");
    expect(screen.getByTestId("mode-switch-mode-default").getAttribute("data-current")).toBe(
      "false"
    );
    // 权限组 chips：生效档（只读）高亮框选中（data-current）；其余不选中
    expect(screen.getByTestId("mode-switch-permission-readOnly").getAttribute("data-current")).toBe(
      "true"
    );
    expect(screen.getByTestId("mode-switch-permission-bypass").getAttribute("data-current")).toBe(
      "false"
    );
    // memory 来源仍保留「上次切换」标注（chips 高亮不取代口径声明；档名文本
    // 回显已按 2026-10-10 用户指令移除——高亮即状态）
    expect(screen.getByTestId("mode-current-source-permission").textContent).toBe("（上次切换）");
  });

  it("E3④ 问答待决：questionPending=true → 按钮全部禁用 + 原因文案 + data-question-pending", async () => {
    installFetch();
    routes.mode = { ...codexTwoAxis(), questionPending: true };
    render(<ModeBar session={{ id: "s-e3-pending" }} />);
    await screen.findByTestId("mode-question-pending-hint");
    expect(screen.getByTestId("mode-bar").getAttribute("data-question-pending")).toBe("true");
    // 模式组：另一档「操作」（平时可点）也禁用；权限组 chips 全禁用
    expect((screen.getByTestId("mode-switch-mode-default") as HTMLButtonElement).disabled).toBe(
      true
    );
    expect(
      (screen.getByTestId("mode-switch-permission-readOnly") as HTMLButtonElement).disabled
    ).toBe(true);
  });

  it("非待决（questionPending 缺省/ false）不置灰不显文案", async () => {
    installFetch();
    routes.mode = codexTwoAxis();
    render(<ModeBar session={{ id: "s-e3-normal" }} />);
    // 用**非当前档** chip 验不置灰（当前档 plan 的禁用来自 isCurrent，与此判据无关）
    await screen.findByTestId("mode-switch-mode-default");
    expect(screen.queryByTestId("mode-question-pending-hint")).toBeNull();
    expect((screen.getByTestId("mode-switch-mode-default") as HTMLButtonElement).disabled).toBe(
      false
    );
  });
});

// ==== 2026-09-23 codex 模式切换改造 / 2026-10-10 二版 chips 化：toggle 直选 / bypass 二次确认 ====
describe("ModeBar：codex toggle chips 与完全信任二次确认（2026-09-23 / 2026-10-10 二版）", () => {
  it("toggle 点另一档 chip = shift+tab 语义：current=plan 时点「操作」**恰好一次** {group:'mode', target:'default'}", async () => {
    installFetch();
    routes.mode = codexTwoAxis(); // mode 组 current = plan → 「操作」chip 是可点档
    routes.switchBody = { status: "key_sent", verified: true, hint: null };
    render(<ModeBar session={{ id: "mc-t1" }} />);
    fireEvent.click(await screen.findByTestId("mode-switch-mode-default"));
    await flushAsync();
    const calls = fetchMock.mock.calls.filter((c: unknown[]) =>
      String(c[0]).includes("/session-mode/switch")
    );
    expect(calls.length).toBe(1, "点一颗 chip 只发一次 switch（无零投递闸，也不多发）");
    const body = JSON.parse(String((calls[0]![1] as RequestInit).body));
    expect(body.sessionId).toBe("mc-t1");
    expect(body.group).toBe("mode");
    expect(body.target).toBe("default", "当前是计划 → 点操作 chip 切到操作");
  });

  it("current=default 时可点档对调：「计划」chip 可点 → 发 target=plan；当前档「操作」禁用", async () => {
    installFetch();
    // mode 组 current = 操作（default）→ 「计划」chip 是可点档
    routes.mode = codexTwoAxis({ modeCurrent: "default" });
    routes.switchBody = { status: "key_sent", verified: true, hint: null };
    render(<ModeBar session={{ id: "mc-t2" }} />);
    const op = await screen.findByTestId("mode-switch-mode-default");
    const plan = screen.getByTestId("mode-switch-mode-plan");
    expect(op.getAttribute("data-current")).toBe("true");
    expect((op as HTMLButtonElement).disabled).toBe(true, "当前档（操作）chip 不可点");
    expect(plan.getAttribute("data-current")).toBe("false");
    fireEvent.click(plan);
    await flushAsync();
    const call = fetchMock.mock.calls.find((c: unknown[]) =>
      String(c[0]).includes("/session-mode/switch")
    );
    const body = JSON.parse(String((call![1] as RequestInit).body));
    expect(body.target).toBe("plan");
  });

  it("当前档未知（纵深防御）：两档 chip 都可点、都不高亮，点击照发（target 仅作核验预期兜底）", async () => {
    installFetch();
    // 权限组记忆不影响模式组——构造 mode 组 current=null 的 toggle 形态
    routes.mode = {
      tool: "codex",
      current: null,
      currentLabel: null,
      readback: false,
      switchKind: "shiftTab",
      structure: "twoAxis",
      groups: [
        {
          id: "mode",
          label: "模式",
          step: false,
          readback: false,
          layout: "toggle",
          current: null,
          currentLabel: null,
          tiers: [
            { mode: "default", label: "操作", selectable: true },
            { mode: "plan", label: "计划", selectable: true },
          ],
          legacy: [],
        },
      ],
    };
    routes.switchBody = { status: "key_sent", verified: true, hint: "已切换（屏读核验命中）" };
    render(<ModeBar session={{ id: "mc-t2b" }} />);
    const plan = await screen.findByTestId("mode-switch-mode-plan");
    const op = screen.getByTestId("mode-switch-mode-default");
    // 用户裁决：屏读必有解，未知态实际不存在——这里只作纵深防御：两档都可点，
    // 落点由后端屏读核验（前端 target 兜底），卡面跟随屏读结果。
    expect(plan.getAttribute("data-current")).toBe("false");
    expect(op.getAttribute("data-current")).toBe("false");
    expect((plan as HTMLButtonElement).disabled).toBe(false);
    expect((op as HTMLButtonElement).disabled).toBe(false);
    // 未知态仍如实给「人工核对」提示（红线 4 不因 chips 化放松）
    expect(screen.getByTestId("mode-unknown-hint-mode").textContent).toContain("人工核对");
    fireEvent.click(plan);
    await flushAsync();
    const call = fetchMock.mock.calls.find((c: unknown[]) =>
      String(c[0]).includes("/session-mode/switch")
    );
    const body = JSON.parse(String((call![1] as RequestInit).body));
    expect(body.group).toBe("mode");
    expect(body.target).toBe("plan", "当前档未知 → 兜底 target=plan（仅核验预期用）");
  });

  it("权限组记忆标注：currentSource=memory → 「（上次切换）」+ 生效档 chip 高亮；screen 来源不标注", async () => {
    installFetch();
    routes.mode = codexTwoAxis({
      permissionCurrent: "readOnly",
      permissionCurrentSource: "memory",
    });
    const { unmount } = render(<ModeBar session={{ id: "mc-t6" }} />);
    await screen.findByTestId("mode-switch-permission-readOnly");
    expect(screen.getByTestId("mode-current-source-permission").textContent).toBe("（上次切换）");
    expect(screen.queryByTestId("mode-unknown-hint-permission")).toBeNull();
    // 生效档 chip 高亮（chips 常驻直选，2026-10-10）
    expect(screen.getByTestId("mode-switch-permission-readOnly").getAttribute("data-current")).toBe(
      "true"
    );
    unmount();

    // 终审 P1-2 反向面：屏读来源（screen）= 实时权威 → **不标注**「上次切换」
    // （旧判据 readback:false 在 readback 全开后不可达——此用例同时锁「不误标」）
    installFetch();
    routes.mode = codexTwoAxis({
      permissionCurrent: "readOnly",
      permissionCurrentSource: "screen",
    });
    render(<ModeBar session={{ id: "mc-t6-screen" }} />);
    await screen.findByTestId("mode-switch-permission-readOnly");
    expect(screen.queryByTestId("mode-current-source-permission")).toBeNull();
  });

  it("权限 chips 直选：点「只读」→ POST {group:'permission', target:'readOnly'}；点「自动审批」→ target:'acceptEdits'", async () => {
    installFetch();
    routes.mode = codexTwoAxis();
    routes.switchBody = { status: "key_sent", verified: true, hint: "已切换（屏读核验命中）" };
    render(<ModeBar session={{ id: "mc-chips" }} />);
    fireEvent.click(await screen.findByTestId("mode-switch-permission-readOnly"));
    await flushAsync();
    const call = fetchMock.mock.calls.find((c: unknown[]) =>
      String(c[0]).includes("/session-mode/switch")
    );
    const body = JSON.parse(String((call![1] as RequestInit).body));
    expect(body.group).toBe("permission");
    expect(body.target).toBe("readOnly");
    // 第二颗 chip：acceptEdits（走同一 switch 端点 Menu 编排，前端不猜编号）。
    // 第一次切换 verified 后组件重拉过 GET（DOM 重渲染）——用 findBy 重取当前节点。
    fireEvent.click(await screen.findByTestId("mode-switch-permission-acceptEdits"));
    await flushAsync();
    const calls = fetchMock.mock.calls.filter((c: unknown[]) =>
      String(c[0]).includes("/session-mode/switch")
    );
    expect(calls.length).toBe(2, "两颗 chip 各发一次 switch");
    const body2 = JSON.parse(String((calls[calls.length - 1]![1] as RequestInit).body));
    expect(body2.target).toBe("acceptEdits");
  });

  it("权限 chips 的「完全信任」仍守二次确认防线：先出确认条，确认后才发 bypass", async () => {
    installFetch();
    routes.mode = codexTwoAxis();
    routes.switchBody = { status: "key_sent", verified: true, hint: null };
    render(<ModeBar session={{ id: "mc-chips-bypass" }} />);
    fireEvent.click(await screen.findByTestId("mode-switch-permission-bypass"));
    await flushAsync();
    // 未确认前零请求，确认条先出
    expect(
      fetchMock.mock.calls.filter((c: unknown[]) => String(c[0]).includes("/session-mode/switch"))
    ).toHaveLength(0);
    expect(screen.getByTestId("mode-bypass-confirm")).toBeTruthy();
    fireEvent.click(screen.getByTestId("mode-bypass-confirm-yes"));
    await flushAsync();
    const call = fetchMock.mock.calls.find((c: unknown[]) =>
      String(c[0]).includes("/session-mode/switch")
    );
    const body = JSON.parse(String((call![1] as RequestInit).body));
    expect(body.group).toBe("permission");
    expect(body.target).toBe("bypass");
  });

  it("权限组当前档 chip **可点**（与 toggle 组的有意不对称，评审 P2-3）：点生效档「只读」恰好发一次 {group:'permission', target:'readOnly'}", async () => {
    installFetch();
    routes.mode = codexTwoAxis({
      permissionCurrent: "readOnly",
      permissionCurrentSource: "screen",
    });
    routes.switchBody = { status: "key_sent", verified: true, hint: null };
    render(<ModeBar session={{ id: "mc-chips-current" }} />);
    const chip = await screen.findByTestId("mode-switch-permission-readOnly");
    // 权限组 chip 只有 disabled={busy}（无 isCurrent 禁用）——生效档也 enabled；
    // toggle 组才是「当前档禁用」。防止将来被无声统一成同款。
    expect(chip.getAttribute("data-current")).toBe("true");
    expect((chip as HTMLButtonElement).disabled).toBe(false, "权限组当前档不禁用（toggle 组才禁）");
    fireEvent.click(chip);
    await flushAsync();
    const calls = fetchMock.mock.calls.filter((c: unknown[]) =>
      String(c[0]).includes("/session-mode/switch")
    );
    expect(calls.length).toBe(1, "点当前档 chip 恰好发一次 switch（重申/核验语义）");
    const body = JSON.parse(String((calls[0]![1] as RequestInit).body));
    expect(body.group).toBe("permission");
    expect(body.target).toBe("readOnly");
  });

  it("当前档是完全信任且可点：点它先武装确认条（不直接发）——零请求 + mode-bypass-confirm 在场", async () => {
    installFetch();
    routes.mode = codexTwoAxis({
      permissionCurrent: "bypass",
      permissionCurrentSource: "screen",
    });
    routes.switchBody = { status: "key_sent", verified: true, hint: null };
    render(<ModeBar session={{ id: "mc-chips-bypass-current" }} />);
    const chip = await screen.findByTestId("mode-switch-permission-bypass");
    expect(chip.getAttribute("data-current")).toBe("true");
    expect((chip as HTMLButtonElement).disabled).toBe(false);
    fireEvent.click(chip);
    await flushAsync();
    // 当前档也要走二次确认防线——未确认前零请求，确认条先出
    expect(
      fetchMock.mock.calls.filter((c: unknown[]) => String(c[0]).includes("/session-mode/switch"))
    ).toHaveLength(0);
    expect(screen.getByTestId("mode-bypass-confirm")).toBeTruthy();
  });
});

// ==== T5（spec §3.1/§3.2/§6-T5）：codex 模式/权限两链的回执消费 ====
// （2026-10-10 用户指令：「已在目标档（零投递）」态删除——点切换必然发键，
// 后端 hint 只剩命中/不符/未生效三态；原零投递回执用例随之删除。
// 同日二版：picker 菜单链（confirm-cancelled 自动重开）随面板删除，用例一并移除。）
describe("ModeBar：T5 codex 回执消费（observed）", () => {
  it("codex 模式切换 verified=false 带 observed → 回执原样透出「屏已切换至 X（预期 Y）」，不重复拼接 observed 档名", async () => {
    installFetch();
    routes.mode = codexTwoAxis();
    routes.switchBody = {
      status: "key_sent",
      verified: false,
      hint: "屏已切换至 操作（预期 计划）——请人工核对终端",
      observed: "default",
      current: "default",
      currentLabel: "操作",
    };
    render(<ModeBar session={{ id: "t5-obs" }} />);
    // 二版 chips：current=plan → 点另一档「操作」chip 发切换
    fireEvent.click(await screen.findByTestId("mode-switch-mode-default"));
    const receipt = await screen.findByTestId("mode-receipt");
    // 整串等值 = 前端只透传未拼接（observed 档名已并入后端 hint，前端不再叠一次）
    expect(receipt.textContent).toBe("屏已切换至 操作（预期 计划）——请人工核对终端");
  });
});

// ==== Task 15 止血门（2026-10-12）前端分诊：codex APP 会话的写动作 =====
// 后端对 form=App ∧ tool=codex 的模式/权限/问答写入口 409 `app_form_pending` +
// 中文 reason。**前端必须消费这条码**：不消费则 ModeBar 的 409 兜底分支把
// 「session-mode/switch 409」这种对用户无意义的串显示出来（本仓库既有教训，
// 见 api.ts 的 questionAnswerErrorCopy 注释），止血门的用户价值只落地一半。
// 文案遵循既有惯例：后端 `reason` 在场用 reason（单一来源），缺省用同义兜底常量
// （`APP_FORM_PENDING_FALLBACK`，与问答侧同一常量——两入口不漂移）。
describe("Task 15 止血门：app_form_pending 分诊", () => {
  /** 后端下发的引导原文（与 api.rs 的 APP_FORM_PENDING_REASON 逐字同义） */
  const REASON = "APP 会话该操作走协议通道，开发中——请在 APP 内操作";

  it("409 app_form_pending + reason：渲染后端中文引导（不是「session-mode/switch 409」）", async () => {
    installFetch();
    routes.mode = codexTwoAxis();
    routes.switchStatus = 409;
    routes.switchBody = { error: "app_form_pending", reason: REASON };
    render(<ModeBar session={{ id: "app-sw-1" }} />);
    // 二版 chips：current=plan → 点另一档「操作」chip 发切换
    fireEvent.click(await screen.findByTestId("mode-switch-mode-default"));
    const err = await screen.findByTestId("mode-error");
    expect(err.textContent).toBe(REASON);
    expect(err.textContent).not.toContain("409");
  });

  it("app_form_pending 无 reason（旧后端/异体）→ 同义兜底，仍不显示「... 409」", async () => {
    installFetch();
    routes.mode = codexTwoAxis();
    routes.switchStatus = 409;
    routes.switchBody = { error: "app_form_pending" };
    render(<ModeBar session={{ id: "app-sw-2" }} />);
    fireEvent.click(await screen.findByTestId("mode-switch-mode-default"));
    const err = await screen.findByTestId("mode-error");
    expect(err.textContent).toBe(REASON);
    expect(err.textContent).not.toContain("409");
  });
});
