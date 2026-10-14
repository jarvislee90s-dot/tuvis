import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import ModeBarApp from "@/mobile/ModeBarApp";
import type { SessionModeView } from "@/mobile/api";

// Task 8（P1 codex 轨）：ModeBarApp——APP 形态（surface=app）的模式栏。契约锚点
//（后端 GET /session-mode APP 面，api.rs `session_mode` 的 codex_app_form 分支）：
// - 顶层 `surface:"app"` + `degraded`（探测正常 JSON null，降级时原因串）；
// - 权限组 = 协议三档（never/on-request/untrusted，标签 全自动/默认/安全，全
//   selectable），current = rollout 读回（currentSource="rollout"）；读不到 →
//   current=null（T0.5 回填：旧会话 rollout 无 turn_context.approval_policy）；
// - 模式组 = 整组灰置（tiers selectable=false + reason「APP 会话暂无模式切换通道」
//   ——T0.2 只定案了 approvalPolicy 载体，形态级暂缺按裁决 31b 灰置 + 一句原因）。
// POST 回执（api.rs `codex_app_switch_arm`）：`{status:"app_switch", verify:
// hit|mismatch|ineffective, carrier, hint, …}`；活跃线程拒绝走 200
// `{status:"failed", code:"active_thread_write_unavailable", error:<钉句>}`。
// 覆盖（任务书测试清单）——
// ① 协议读回档位渲染；② current=null →「模式未知，请人工核对 APP」；
// ③ mismatch → hint + 自动重拉 GET、不拿回执改本地状态；④ degraded → 只读
// chips + 原因；⑤ 活跃线程拒绝 → 灰置 + 钉句；⑥（附）hit → hint 透传 + 重拉。

interface Routes {
  mode?: SessionModeView;
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
  fetchMock = vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
    const url = String(input);
    if (url.includes("/session-mode/switch")) {
      if (routes.switchStatus) {
        return new Response(JSON.stringify(routes.switchBody ?? { error: "internal" }), {
          status: routes.switchStatus,
        });
      }
      return new Response(JSON.stringify(routes.switchBody ?? appSwitchHit("never")), {
        status: 200,
      });
    }
    if (url.includes("/session-mode")) {
      return new Response(JSON.stringify(routes.mode ?? appSurface()), { status: 200 });
    }
    throw new Error(`unexpected fetch: ${url} ${init?.method ?? ""}`);
  });
  vi.stubGlobal("fetch", fetchMock);
}

/** GET /session-mode APP 面（codex）：权限组协议三档 + 模式组灰置。
 *  `permissionCurrent` = rollout 读回串（null = 旧会话无 approval_policy 记录）。 */
function appSurface(opts?: {
  permissionCurrent?: string | null;
  degraded?: string | null;
}): SessionModeView {
  const permissionCurrent = opts?.permissionCurrent ?? null;
  const labels: Record<string, string> = {
    never: "全自动",
    "on-request": "默认",
    untrusted: "安全",
  };
  return {
    tool: "codex",
    current: null,
    currentLabel: null,
    readback: false,
    switchKind: "slashCommand",
    structure: "twoAxis",
    // Task 6a/7：交互面单点下发；degraded = 探测降级原因（None → JSON null）
    surface: "app",
    degraded: opts?.degraded ?? null,
    groups: [
      {
        id: "mode",
        label: "模式",
        step: false,
        readback: false,
        layout: "toggle",
        // 模式组：无协议载体 → current=null + tiers 全灰置（原因单点来自后端）
        current: null,
        currentLabel: null,
        currentSource: "null",
        tiers: [
          { mode: "plan", label: "计划", selectable: false, reason: "APP 会话暂无模式切换通道" },
          { mode: "default", label: "操作", selectable: false, reason: "APP 会话暂无模式切换通道" },
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
        currentLabel:
          permissionCurrent === null ? null : (labels[permissionCurrent] ?? permissionCurrent),
        currentSource: permissionCurrent === null ? "null" : "rollout",
        tiers: [
          { mode: "never", label: "全自动", selectable: true, reason: null },
          { mode: "on-request", label: "默认", selectable: true, reason: null },
          { mode: "untrusted", label: "安全", selectable: true, reason: null },
        ],
        legacy: [],
      },
    ],
  };
}

/** POST 回执（app_switch，hit 形态——后端 hint 原样透传，前端不重拼） */
function appSwitchHit(policy: string): Record<string, unknown> {
  return {
    status: "app_switch",
    interrupted: false,
    verified: true,
    verify: "hit",
    policy,
    readback: null,
    carrier: "turnStart",
    turnObserved: true,
    hint: `已切换至 默认（协议读回核对一致）`,
  };
}

describe("ModeBarApp：APP 形态模式栏（Task 8）", () => {
  it("mode_bar_app_renders_readback_current：rollout 读回档位高亮、三档 chips 在场", async () => {
    installFetch();
    routes.mode = appSurface({ permissionCurrent: "on-request" });
    render(<ModeBarApp session={{ id: "s1" }} />);
    // 协议读回档（on-request/默认）高亮；其余两档可点但不高亮
    const current = await screen.findByTestId("mode-app-switch-permission-on-request");
    expect(current.getAttribute("data-current")).toBe("true");
    expect(
      screen.getByTestId("mode-app-switch-permission-never").getAttribute("data-current")
    ).toBe("false");
    expect(
      screen.getByTestId("mode-app-switch-permission-untrusted").getAttribute("data-current")
    ).toBe("false");
    // currentSource="rollout" 是实时协议读回——不得带 CLI 的「（上次切换）」标注
    expect(screen.queryByTestId("mode-current-source-permission")).toBeNull();
    // 模式组（无协议载体）：灰置 chips 在场，不带可点按钮语义
    expect(screen.getByTestId("mode-app-tier-disabled-mode-plan").textContent).toContain("计划");
  });

  it("mode_bar_app_unknown_current_shows_manual_check_hint：current=null → 模式未知，请人工核对 APP", async () => {
    installFetch();
    routes.mode = appSurface({ permissionCurrent: null });
    render(<ModeBarApp session={{ id: "s1" }} />);
    // T0.5 回填定案文案：rollout 无 approval_policy 记录（旧会话）→ 保守降级不猜档
    expect((await screen.findByTestId("mode-app-unknown-hint-permission")).textContent).toContain(
      "模式未知，请人工核对 APP"
    );
  });

  it("mode_bar_app_switch_mismatch_triggers_refetch_not_local_state：不符 → hint + 自动重拉、不采信回执", async () => {
    installFetch();
    routes.mode = appSurface({ permissionCurrent: "on-request" });
    routes.switchBody = {
      status: "app_switch",
      interrupted: false,
      verified: false,
      verify: "mismatch",
      policy: "never",
      readback: "untrusted",
      carrier: "turnStart",
      turnObserved: true,
      hint: "档位读回为「untrusted」，与目标 全自动 不符——请以 APP 实况为准后重试",
    };
    render(<ModeBarApp session={{ id: "s1" }} />);
    fireEvent.click(await screen.findByTestId("mode-app-switch-permission-never"));
    // hint 原样透传（含读回串与目标档——后端单点文案，前端不重拼）
    expect((await screen.findByTestId("mode-app-notice")).textContent).toContain("不符");
    // **不拿回执改本地状态**：目标档不高亮，原档保持（重拉 GET 的权威面才算数）
    expect(
      screen.getByTestId("mode-app-switch-permission-never").getAttribute("data-current")
    ).toBe("false");
    expect(
      screen.getByTestId("mode-app-switch-permission-on-request").getAttribute("data-current")
    ).toBe("true");
    // 自动重拉 GET：初始 1 次 + 回执后 1 次（GET 带 session_id 查询串）
    const gets = fetchMock.mock.calls.filter(([u]) => String(u).includes("/session-mode?"));
    expect(gets.length).toBe(2);
  });

  it("mode_bar_app_degraded_renders_readonly_chips_with_reason：探测降级 → 只读 chips + 原因", async () => {
    installFetch();
    routes.mode = appSurface({
      permissionCurrent: "on-request",
      degraded: "codex app-server 探测失败：spawn 不可达",
    });
    render(<ModeBarApp session={{ id: "s1" }} />);
    await screen.findByTestId("mode-bar-app");
    // 只读：三档 chips 全部禁用（Task 7 降级档 → 前端消费 degraded 字段）
    for (const mode of ["never", "on-request", "untrusted"]) {
      expect(
        (screen.getByTestId(`mode-app-switch-permission-${mode}`) as HTMLButtonElement).disabled
      ).toBe(true);
    }
    // 原因原样透出（不做第二次文案加工）
    expect(screen.getByTestId("mode-app-degraded").textContent).toContain("spawn 不可达");
  });

  it("mode_bar_app_active_thread_refusal_shows_grayed_reason：活跃线程拒绝 → 灰置 + 钉句", async () => {
    installFetch();
    routes.mode = appSurface({ permissionCurrent: "on-request" });
    routes.switchBody = {
      status: "failed",
      error: "会话正被 APP 使用，请关闭 APP 后重试或在 APP 内操作",
      code: "active_thread_write_unavailable",
    };
    render(<ModeBarApp session={{ id: "s1" }} />);
    fireEvent.click(await screen.findByTestId("mode-app-switch-permission-never"));
    // 钉句单点透出（api.rs ACTIVE_THREAD_WRITE_REASON 的 Display 串）
    expect((await screen.findByTestId("mode-app-refusal")).textContent).toContain(
      "会话正被 APP 使用"
    );
    // 灰置：活跃线程持有写者，切档入口全部禁用（T0.1 定案：APP 活跃会话无写通道）
    for (const mode of ["never", "on-request", "untrusted"]) {
      expect(
        (screen.getByTestId(`mode-app-switch-permission-${mode}`) as HTMLButtonElement).disabled
      ).toBe(true);
    }
  });

  it("mode_bar_app_unknown_tier_word_reports_instead_of_silent：selectable 档词漂移 → 如实提示、零 POST（m-3）", async () => {
    installFetch();
    const base = appSurface({ permissionCurrent: "on-request" });
    if (base.groups === undefined) throw new Error("fixture groups missing");
    // 词表漂移夹具：权限组 selectable 档混入 MamMode 词（bypass——CLI 词表的词，
    // 不在协议三档词表内；isAppPolicy 收窄应拒发并如实提示）
    routes.mode = {
      ...base,
      groups: base.groups.map((g) =>
        g.id === "permission"
          ? { ...g, tiers: [{ mode: "bypass", label: "漂移档", selectable: true, reason: null }] }
          : g
      ),
    };
    render(<ModeBarApp session={{ id: "s1" }} />);
    fireEvent.click(await screen.findByTestId("mode-app-switch-permission-bypass"));
    // 如实提示（不静默吞点击）；无 switch POST（词表外不发请求）
    expect((await screen.findByTestId("mode-app-notice")).textContent).toContain(
      "未知档位词 bypass"
    );
    const posts = fetchMock.mock.calls.filter(([u]) => String(u).includes("/session-mode/switch"));
    expect(posts.length).toBe(0);
  });

  it("mode_bar_app_switch_hit_shows_receipt_and_refetches：命中 → hint 透传 + 重拉 GET 刷新（附加用例）", async () => {
    installFetch();
    routes.mode = appSurface({ permissionCurrent: "on-request" });
    routes.switchBody = appSwitchHit("never");
    render(<ModeBarApp session={{ id: "s1" }} />);
    fireEvent.click(await screen.findByTestId("mode-app-switch-permission-never"));
    // hit：后端 hint 原样透传（不硬编码「已切换」盖掉后端口径——CLI 套 T5 同款纪律）
    expect((await screen.findByTestId("mode-app-receipt")).textContent).toContain(
      "协议读回核对一致"
    );
    // hit 同样重拉 GET 刷新显示（卡面以 GET 权威面为准，不拿回执字段改本地）
    await waitFor(() => {
      const gets = fetchMock.mock.calls.filter(([u]) => String(u).includes("/session-mode?"));
      expect(gets.length).toBe(2);
    });
  });

  // ===== 收口 I-2：通用 failed 臂落 rose error（对齐 CLI 套 mode-error 语义）=====
  // CLI 套（ModeBarCli）的 `failed{error}` / 网络异常走 rose `mode-error`，amber
  // notice 只承载「未确认」语义（mismatch/ineffective）；APP 套此前把通用 failed
  // 也落 amber notice——「失败」与「注意」两种语义漂移。refusal（活跃线程钉句）
  // 单列不变（sticky 灰置是它专属的形态）。
  it("mode_bar_app_generic_failed_renders_rose_error：failed{error} → rose error（非 amber notice）", async () => {
    installFetch();
    routes.mode = appSurface({ permissionCurrent: "on-request" });
    routes.switchBody = { status: "failed", error: "codex 协议通道不可达" };
    render(<ModeBarApp session={{ id: "s1" }} />);
    fireEvent.click(await screen.findByTestId("mode-app-switch-permission-never"));
    const err = await screen.findByTestId("mode-app-error");
    expect(err.textContent).toContain("codex 协议通道不可达");
    // 失败不落 amber notice（「未确认」语义与「失败」语义分列）
    expect(screen.queryByTestId("mode-app-notice")).toBeNull();
  });

  it("mode_bar_app_network_error_renders_rose_error：非 2xx/网络异常 → rose error（对齐 CLI catch 臂）", async () => {
    installFetch();
    routes.mode = appSurface({ permissionCurrent: "on-request" });
    routes.switchStatus = 500;
    routes.switchBody = { error: "internal" };
    render(<ModeBarApp session={{ id: "s1" }} />);
    fireEvent.click(await screen.findByTestId("mode-app-switch-permission-never"));
    expect(await screen.findByTestId("mode-app-error")).toBeTruthy();
    expect(screen.queryByTestId("mode-app-notice")).toBeNull();
    expect(screen.queryByTestId("mode-app-refusal")).toBeNull();
  });
});
