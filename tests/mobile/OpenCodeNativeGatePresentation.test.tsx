// H05 聊天页提示与可操作状态收口（RED→GREEN）：
// - echo 写门（H03）的 `OC_GATE_*` 受控码必须得到**专属**本地化文案：不上屏机器串、
//   不与既有 `OC_INSTANCE_*` 归属文案混用（不同门码可映射不同原因短语）；
// - 同一阻断事实同屏只解释一处（会话级绑定门在场 ⇒ 发送阻断行不重复解释）；
// - 「正在连接/读取中」只用于真实请求在途；常驻态（未接管）不得写成连接中；
// - 缺能力 / 已定案 / 结果未知三种呈现互不合并；
// - 上传口过门 = 提前失败：上传被归属门拒绝时呈现门拒绝原因（零本地落盘）；
// - stale 恢复后阻断消失、草稿与恢复锚保留（既有 staleRecovered 行为锁）；
// - 双宽度（竖屏 jsdom 缺省 / 横屏 matchMedia 替身）下关键结构一致：阻断单处、输入可用。
//
// 消费面与既有 OpenCodeNativePanel.test.tsx 同一契约；本文件只加 H05 的呈现锁。
import { act, cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterAll, afterEach, beforeAll, beforeEach, describe, expect, it, vi } from "vitest";
import OpenCodeNativePanel from "@/mobile/OpenCodeNativePanel";
import OpenCodeNativeQueue, { resetQueueControlMemoryForTests } from "@/mobile/OpenCodeNativeQueue";
import SessionDetail from "@/mobile/SessionDetail";
import {
  addActionAnchor,
  rememberActionBinding,
  resetActionAnchorMemoryForTests,
} from "@/mobile/opencodeActionAnchors";
import type { PublicNativeView } from "@/mobile/opencodeNativeApi";
import { isOwnershipRefusalCode } from "@/mobile/opencodeNativeApi";
import i18n from "@/i18n";
import { pinZhNavigator } from "./pinZhNavigator";
import { MockEventSource } from "./eventSourceMock";

const sessionId = "gate-presentation";

let unpin: () => void;

beforeAll(async () => {
  unpin = await pinZhNavigator();
});
afterAll(() => unpin());

beforeEach(() => {
  sessionStorage.clear();
  resetActionAnchorMemoryForTests();
  resetQueueControlMemoryForTests();
  MockEventSource.reset();
});

afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
  sessionStorage.clear();
  resetActionAnchorMemoryForTests();
  resetQueueControlMemoryForTests();
});

function capability(allowed: boolean, reasonCode: string | null = null) {
  return { allowed, evidence: "contract", reasonCode };
}

/** 权威视图夹具（与 OpenCodeNativePanel.test.tsx 的 nativeView 同形状）。 */
function baseView(over: Partial<PublicNativeView> = {}): PublicNativeView {
  return {
    sessionId,
    bindingToken: "bind-1",
    routeVersion: 1,
    revision: 1,
    complete: true,
    route: "opencode_native",
    routeReasonCode: null,
    capabilities: {
      send: capability(true),
      retract: capability(true),
      stop_session: capability(true),
      set_agent: capability(true),
    },
    messages: [],
    pending: [],
    inbox: [],
    runtimeState: "ready",
    files: {},
    ...over,
  };
}

const t = (key: string) => i18n.t(key);

/** 门码 → 专属文案键（与 nativeErrorKey 的 GREEN 映射一一对应）。 */
const GATE_COPY: Array<[string, string]> = [
  ["OC_GATE_NO_ACK", "mobile.openCodeNative.errors.gateNoAck"],
  ["OC_GATE_LATE_ACK", "mobile.openCodeNative.errors.gateNoAck"],
  ["OC_GATE_BUSY", "mobile.openCodeNative.errors.gateBusy"],
  ["OC_GATE_UNREACHABLE", "mobile.openCodeNative.errors.gateUnreachable"],
  ["OC_GATE_PID_UNVERIFIED", "mobile.openCodeNative.errors.gatePidUnverified"],
  ["OC_GATE_PID_MISMATCH", "mobile.openCodeNative.errors.gateEvidenceConflict"],
  ["OC_GATE_NONCE_MISMATCH", "mobile.openCodeNative.errors.gateEvidenceConflict"],
  ["OC_GATE_TITLE_INVALID", "mobile.openCodeNative.errors.gateEvidenceConflict"],
  ["OC_GATE_PROBE_UNWRITABLE", "mobile.openCodeNative.errors.gateEvidenceConflict"],
  ["OC_GATE_PROBE_TARGET", "mobile.openCodeNative.errors.gateProbeTarget"],
];

describe("isOwnershipRefusalCode 直测（H05 小补丁②）：归属未证呈现族单点判定", () => {
  it("OC_INSTANCE_* / OC_DESKTOP_BINDING_REQUIRED / 全部 OC_GATE_* 归入呈现族", () => {
    expect(isOwnershipRefusalCode("OC_INSTANCE_OWNERSHIP_UNVERIFIED")).toBe(true);
    expect(isOwnershipRefusalCode("OC_INSTANCE_AMBIGUOUS")).toBe(true);
    expect(isOwnershipRefusalCode("OC_DESKTOP_BINDING_REQUIRED")).toBe(true);
    for (const [code] of GATE_COPY) expect(isOwnershipRefusalCode(code)).toBe(true);
  });

  it("大小写与首尾空白归一；非家族码（含 OC_GATE 相邻形态）不误判", () => {
    expect(isOwnershipRefusalCode(" oc_gate_busy ")).toBe(true);
    expect(isOwnershipRefusalCode("oc_instance_ambiguous")).toBe(true);
    expect(isOwnershipRefusalCode(null)).toBe(false);
    expect(isOwnershipRefusalCode(undefined)).toBe(false);
    expect(isOwnershipRefusalCode("")).toBe(false);
    expect(isOwnershipRefusalCode("   ")).toBe(false);
    expect(isOwnershipRefusalCode("OC_BINDING_STALE")).toBe(false);
    expect(isOwnershipRefusalCode("OC_RESPONSE_INVALID")).toBe(false);
    expect(isOwnershipRefusalCode("OC_GATEFOO")).toBe(false);
  });
});

describe("门码 × 绑定门在场：单处阻断互斥组合（H05 小补丁②，现只锁过 OC_INSTANCE 变体）", () => {
  /** 组合夹具：owned 路由 + 绑定门在场（bindingRequired=true、服务/目录可判定）
   *  + send 能力带门码——阻断事实相同 ⇒ 全屏只允许绑定门一处说明。 */
  function installGateViewWithBindingGate(code: string) {
    vi.stubGlobal(
      "fetch",
      vi.fn(async (input: RequestInfo | URL) => {
        const url = String(input);
        if (url.includes("/opencode/view"))
          return Response.json(
            baseView({
              bindingRequired: true,
              desktopBound: false,
              serviceUrl: "http://127.0.0.1:4096",
              directory: "/proj",
              capabilities: {
                send: capability(false, code),
                stop_session: capability(true),
              },
            })
          );
        if (url.includes("/opencode/outbox"))
          return Response.json({ sessionId, revision: 1, paused: false, items: [] });
        if (url.includes("/opencode/agents")) return Response.json({ current: null, options: [] });
        throw new Error(`unexpected fetch: ${url}`);
      })
    );
  }

  it.each(GATE_COPY)(
    "%s × 绑定门在场：绑定门恰一块，发送阻断行不重复解释，机器串不上屏",
    async (code) => {
      installGateViewWithBindingGate(code);
      render(<OpenCodeNativePanel sessionId={sessionId} />);
      const gates = await screen.findAllByTestId("native-binding-gate");
      expect(gates).toHaveLength(1);
      // 互斥：同一阻断事实的发送阻断行被门吸收
      expect(screen.queryByTestId("native-send-blocked")).toBeNull();
      // 门原因走本地化（门码非身份拒绝 ⇒ 重连指引分支），机器串不上屏
      expect(gates[0].textContent).toContain(t("mobile.openCodeNative.route.bindingGate"));
      expect(screen.queryByText(/OC_GATE_/)).toBeNull();
    }
  );
});

describe("发送被 echo 写门拒绝：单处专属文案 + 确定零写出 + 可重试", () => {
  /** 可写视图 + POST /actions 按给定受控码 409。 */
  function installSendReject(code: string) {
    vi.stubGlobal(
      "fetch",
      vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
        const url = String(input);
        if (url.endsWith("/opencode/actions") && init?.method === "POST")
          return new Response(JSON.stringify({ error: code, reasonCode: code }), { status: 409 });
        if (url.includes("/opencode/view")) return Response.json(baseView());
        if (url.includes("/opencode/outbox"))
          return Response.json({ sessionId, revision: 1, paused: false, items: [] });
        if (url.includes("/opencode/agents")) return Response.json({ current: null, options: [] });
        throw new Error(`unexpected fetch: ${url}`);
      })
    );
  }

  it.each(GATE_COPY)(
    "%s 拒绝显示专属短语（非机器串、非归属泛句），草稿保留、锚清除、发送可重试",
    async (code, key) => {
      installSendReject(code);
      render(<OpenCodeNativePanel sessionId={sessionId} />);
      const input = await screen.findByTestId("native-input");
      fireEvent.change(input, { target: { value: "归属门草稿" } });
      fireEvent.click(screen.getByTestId("native-send"));

      const receipt = await screen.findByTestId("native-receipt-rejected");
      expect(receipt.textContent).toBe(t(key));
      // 专属文案：不上屏机器串，也不落回「归属未证」泛句
      expect(screen.queryByText(/OC_GATE_/)).toBeNull();
      expect(
        screen.queryByText(t("mobile.openCodeNative.errors.instanceOwnershipUnverified"))
      ).toBeNull();
      // 确定零写出：锚清除、锁句消失、草稿保留、发送键恢复（重试 = 新 intent）
      expect(sessionStorage.getItem(`tuvis.opencodeNative.action.${sessionId}`)).toBeNull();
      expect(screen.queryByTestId("native-send-locked")).toBeNull();
      expect(screen.queryByTestId("native-action-anchor")).toBeNull();
      expect(input).toHaveValue("归属门草稿");
      expect(screen.getByTestId("native-send")).not.toBeDisabled();
    }
  );

  it("拒绝后同屏只有一处阻断说明（回执即原因，无第二块会话级阻断卡）", async () => {
    installSendReject("OC_GATE_NO_ACK");
    render(<OpenCodeNativePanel sessionId={sessionId} />);
    const input = await screen.findByTestId("native-input");
    fireEvent.change(input, { target: { value: "一处说明" } });
    fireEvent.click(screen.getByTestId("native-send"));
    await screen.findByTestId("native-receipt-rejected");
    expect(screen.queryByTestId("native-binding-gate")).toBeNull();
    expect(screen.queryByTestId("native-send-blocked")).toBeNull();
    expect(screen.queryByTestId("native-load-error")).toBeNull();
  });
});

describe("视图层写能力带门码：本地化呈现 + 只读能力不受影响", () => {
  it("send 能力 reasonCode=OC_GATE_PID_UNVERIFIED 时显示专属短语，绝不显示机器串", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn(async (input: RequestInfo | URL) => {
        const url = String(input);
        if (url.includes("/opencode/view"))
          return Response.json(
            baseView({
              runtimeState: "running",
              capabilities: {
                send: capability(false, "OC_GATE_PID_UNVERIFIED"),
                stop_session: capability(true),
              },
            })
          );
        if (url.includes("/opencode/outbox"))
          return Response.json({ sessionId, revision: 1, paused: false, items: [] });
        if (url.includes("/opencode/agents"))
          return Response.json({
            current: "build",
            options: [{ id: "build", name: "Build" }],
          });
        throw new Error(`unexpected fetch: ${url}`);
      })
    );
    render(<OpenCodeNativePanel sessionId={sessionId} />);
    const blocked = await screen.findByTestId("native-send-blocked");
    expect(blocked.textContent).toBe(t("mobile.openCodeNative.errors.gatePidUnverified"));
    expect(screen.queryByText(/OC_GATE_/)).toBeNull();
    expect(screen.queryByTestId("native-binding-gate")).toBeNull();
    // 正文仍可编辑；running 时，停止按钮跟随自身能力，不受发送能力门影响。
    const input = screen.getByTestId("native-input");
    fireEvent.change(input, { target: { value: "仍可输入" } });
    expect(input).toHaveValue("仍可输入");
    expect(screen.getByTestId("native-stop")).not.toBeDisabled();
  });

  it("能力未放行（缺能力）、已定案、结果未知三态同屏各自呈现，不合并成一种失败", async () => {
    addActionAnchor(sessionId, {
      actionId: "act-decide-other",
      kind: "decide",
      targetId: "perm-other",
    });
    rememberActionBinding("act-decide-other", "bind-1");
    vi.stubGlobal("EventSource", MockEventSource as unknown as typeof EventSource);
    vi.stubGlobal(
      "fetch",
      vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
        const url = String(input);
        if (url.endsWith("/opencode/actions") && init?.method === "POST") {
          // 桌面端先决：原生侧已定案 → 409 OC_REQUEST_SETTLED（不重试、不归因）
          return Response.json({ error: "OC_REQUEST_SETTLED" }, { status: 409 });
        }
        if (url.includes("/opencode/actions/"))
          return Response.json({ actionId: "act-decide-other", state: "unconfirmed" });
        if (url.includes("/opencode/view"))
          return Response.json(
            baseView({
              capabilities: {
                send: capability(true),
                permission_once: capability(true),
                permission_reject: capability(true),
                // answer_group 缺键 = 未判定 = 不可用（缺能力态）
              },
              pending: [
                {
                  kind: "form",
                  id: "form-1",
                  title: "问题表单",
                  context_token: "ctx",
                  state: "unknown",
                  fields: [],
                },
                {
                  kind: "permission",
                  id: "perm-1",
                  title: "审批请求",
                  decisions: ["once", "reject"],
                  context_token: "ctx",
                },
              ],
            })
          );
        if (url.includes("/opencode/outbox"))
          return Response.json({ sessionId, revision: 1, paused: false, items: [] });
        if (url.includes("/opencode/agents")) return Response.json({ current: null, options: [] });
        throw new Error(`unexpected fetch: ${url}`);
      })
    );
    render(<OpenCodeNativePanel sessionId={sessionId} />);
    // 缺能力：表单走只读行并说明能力未放行
    const formRow = await screen.findByTestId("native-pending-form");
    expect(formRow.textContent).toContain(
      t("mobile.openCodeNative.questions.capabilityBlockedGeneric")
    );
    // 已定案：点击 decide（once）→ 原生侧已定案（独立文案，不冒充失败）
    fireEvent.click(
      screen
        .getAllByTestId("native-decision")
        .find((el) => el.getAttribute("data-decision") === "once") as HTMLElement
    );
    const note = await screen.findByTestId("native-decision-receipt");
    expect(note).toHaveAttribute("data-kind", "settled");
    expect(note.textContent).toContain(t("mobile.openCodeNative.pending.decisionSettled"));
    // 结果未知：恢复锚行保留，只给只读核对（独立入口）
    const anchor = screen.getByTestId("native-action-anchor");
    expect(anchor).toHaveAttribute("data-action-id", "act-decide-other");
    expect(screen.getByTestId("native-action-verify")).not.toBeDisabled();
    // 三态文案互不相同
    expect(t("mobile.openCodeNative.questions.capabilityBlockedGeneric")).not.toContain("已经定案");
    expect(t("mobile.openCodeNative.pending.decisionSettled")).not.toContain("能力未放行");
  });
});

describe("「正在连接/读取中」只用于真实请求在途", () => {
  it("首读在途显示读取中；失败落到确定错误；重读成功后错误消失", async () => {
    let calls = 0;
    let release: ((ok: boolean) => void) | null = null;
    vi.stubGlobal(
      "fetch",
      vi.fn(async (input: RequestInfo | URL) => {
        const url = String(input);
        if (url.includes("/opencode/view")) {
          calls += 1;
          if (calls === 1)
            return new Promise<Response>((resolve) => {
              release = (ok: boolean) =>
                resolve(ok ? Response.json(baseView()) : Response.json({}, { status: 500 }));
            });
          return Response.json(baseView());
        }
        if (url.includes("/opencode/outbox"))
          return Response.json({ sessionId, revision: 1, paused: false, items: [] });
        if (url.includes("/opencode/agents")) return Response.json({ current: null, options: [] });
        throw new Error(`unexpected fetch: ${url}`);
      })
    );
    render(<OpenCodeNativePanel sessionId={sessionId} />);
    // 在途：读取中可见
    expect(await screen.findByTestId("native-loading")).toBeInTheDocument();
    // 失败：确定状态（读取中消失，错误块出现）
    release?.(false);
    await screen.findByTestId("native-load-error");
    expect(screen.queryByTestId("native-loading")).toBeNull();
    // 恢复：错误块消失
    fireEvent.click(screen.getByTestId("native-retry"));
    await waitFor(() => expect(screen.queryByTestId("native-load-error")).toBeNull());
    expect(screen.queryByTestId("native-loading")).toBeNull();
  });

  it("未接管是常驻确定态：文案不再写成「正在连接」", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn(async (input: RequestInfo | URL) => {
        const url = String(input);
        if (url.includes("/opencode/view"))
          return Response.json(
            baseView({
              route: "legacy",
              routeReasonCode: "OC_INSTANCE_OWNERSHIP_UNVERIFIED",
              bindingRequired: true,
              desktopBound: true,
              capabilities: { send: capability(false, "OC_INSTANCE_OWNERSHIP_UNVERIFIED") },
            })
          );
        if (url.includes("/opencode/outbox"))
          return Response.json({ sessionId, revision: 1, paused: false, items: [] });
        if (url.includes("/opencode/agents")) return Response.json({ current: null, options: [] });
        throw new Error(`unexpected fetch: ${url}`);
      })
    );
    render(<OpenCodeNativePanel sessionId={sessionId} />);
    await screen.findByTestId("native-input");
    fireEvent.click(screen.getByTestId("native-connection-details-toggle"));
    const notice = screen.getByTestId("native-route-notice");
    expect(notice.textContent).toBe(t("mobile.openCodeNative.route.unclaimed"));
    expect(notice.textContent).not.toContain("正在连接");
  });
});

describe("上传口过门 = 提前失败：门拒绝原因如实呈现", () => {
  /** XHR 替身：上传直接以 409 + 门受控码拒绝（后端在落盘前就拒）。 */
  class GateRefusedXHR {
    upload = { onprogress: null as ((e: ProgressEvent) => void) | null };
    onload: ((e: ProgressEvent) => void) | null = null;
    onerror: (() => void) | null = null;
    onabort: (() => void) | null = null;
    status = 0;
    responseText = "";
    open(_method: string, _url: string) {}
    setRequestHeader() {}
    send() {
      this.respond(409, JSON.stringify({ error: "OC_GATE_NO_ACK", reasonCode: "OC_GATE_NO_ACK" }));
    }
    abort() {
      this.onabort?.();
    }
    respond(status: number, body: string) {
      this.status = status;
      this.responseText = body;
      this.onload?.(new ProgressEvent("load"));
    }
  }

  it("上传被门阻断：chip 显示门拒绝原因（不落盘、发送保持禁用、原因只此一处）", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn(async (input: RequestInfo | URL) => {
        const url = String(input);
        if (url.includes("/opencode/view")) return Response.json(baseView());
        if (url.includes("/opencode/outbox"))
          return Response.json({ sessionId, revision: 1, paused: false, items: [] });
        if (url.includes("/opencode/agents")) return Response.json({ current: null, options: [] });
        throw new Error(`unexpected fetch: ${url}`);
      })
    );
    vi.stubGlobal("XMLHttpRequest", GateRefusedXHR as unknown as typeof XMLHttpRequest);
    render(<OpenCodeNativePanel sessionId={sessionId} />);
    await screen.findByTestId("native-input");
    fireEvent.change(screen.getByTestId("native-file-input"), {
      target: { files: [new File(["x"], "gate.txt")] },
    });
    const chip = await screen.findByTestId("native-attachment");
    expect(chip).toHaveAttribute("data-status", "failed");
    const error = screen.getByTestId("native-attachment-error");
    expect(error.textContent).toContain(t("mobile.openCodeNative.errors.gateNoAck"));
    // 上传被阻 = 提前失败：整次发送保持阻断，机器串不上屏，门原因只出现一次
    expect(screen.queryByText(/OC_GATE_/)).toBeNull();
    expect(screen.getAllByText(t("mobile.openCodeNative.errors.gateNoAck"))).toHaveLength(1);
    expect(screen.getByTestId("native-send")).toBeDisabled();
  });
});

describe("stale 恢复：阻断消失，草稿与恢复锚保留", () => {
  it("旧绑定失效自动恢复后阻断提示消失，草稿与未确认锚保留，写能力回归", async () => {
    addActionAnchor(sessionId, {
      actionId: "act-keep",
      kind: "decide",
      targetId: "perm-keep",
    });
    rememberActionBinding("act-keep", "bind-1");
    vi.stubGlobal("EventSource", MockEventSource as unknown as typeof EventSource);
    let stale = false;
    const target = { directory: "/proj", serviceId: "svc-1", servicePid: 1 };
    vi.stubGlobal(
      "fetch",
      vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
        const url = String(input);
        if (url.includes("/opencode/view")) {
          const binding = new Headers(init?.headers).get("X-OpenCode-Expected-Binding");
          if (stale && binding === "bind-1")
            return Response.json({ error: "OC_BINDING_STALE" }, { status: 409 });
          const fresh = binding === null || binding === "bind-2";
          if (!fresh) return Response.json({ error: "OC_BINDING_STALE" }, { status: 409 });
          return Response.json(
            baseView({
              bindingToken: "bind-2",
              revision: stale ? 3 : 1,
              directory: target.directory,
              serviceId: target.serviceId,
              servicePid: target.servicePid,
            })
          );
        }
        if (url.includes("/opencode/actions/"))
          return Response.json({ actionId: "act-keep", state: "unconfirmed" });
        if (url.includes("/opencode/outbox"))
          return Response.json({ sessionId, revision: 1, paused: false, items: [] });
        if (url.includes("/opencode/agents")) return Response.json({ current: null, options: [] });
        throw new Error(`unexpected fetch: ${url}`);
      })
    );
    render(<OpenCodeNativePanel sessionId={sessionId} />);
    const input = await screen.findByTestId("native-input");
    fireEvent.change(input, { target: { value: "恢复保留草稿" } });
    expect(await screen.findByTestId("native-action-anchor")).toBeInTheDocument();
    expect(MockEventSource.instances.length).toBeGreaterThan(0);

    stale = true;
    (MockEventSource.latest() as unknown as { emit: (t: string, d: string) => void }).emit(
      "revision",
      "2"
    );
    await waitFor(() => expect(screen.queryByTestId("native-load-error")).toBeNull());
    // 阻断消失 + 草稿/锚保留 + 写能力回归（decide 锚只锁该行核对，不冒充会话级写锁）
    expect(input).toHaveValue("恢复保留草稿");
    expect(screen.queryByTestId("native-binding-gate")).toBeNull();
    expect(screen.getByTestId("native-action-anchor")).toBeInTheDocument();
    expect(screen.getByTestId("native-action-verify")).not.toBeDisabled();
    expect(screen.getByTestId("native-send")).not.toBeDisabled();
  });
});

describe("双宽度：竖屏与横屏下关键结构一致（阻断单处、输入可用）", () => {
  /** SessionDetail 的最小端点集合（同 OpenCodeNativePanel.test.tsx 的 shell 形状）。 */
  function installShellFetch() {
    vi.stubGlobal(
      "fetch",
      vi.fn(async (input: RequestInfo | URL) => {
        const url = String(input);
        if (url.includes("/session-messages"))
          return Response.json({ messages: [], truncated: false });
        if (url.includes("/session-files")) return Response.json({ files: [], truncated: false });
        if (url.includes("/session-subagents")) return Response.json({ subagents: [] });
        if (url.includes("/session-send-info"))
          return Response.json({
            injectable: false,
            reasonCode: "opencode_native",
            interactionRoute: "opencode_native",
            interactionRouteReason: "已归原生",
          });
        if (url.includes("/opencode/view"))
          return Response.json(
            baseView({
              route: "legacy",
              routeReasonCode: "OC_INSTANCE_OWNERSHIP_UNVERIFIED",
              bindingRequired: true,
              desktopBound: true,
              capabilities: { send: capability(false, "OC_INSTANCE_OWNERSHIP_UNVERIFIED") },
            })
          );
        if (url.includes("/opencode/agents")) return Response.json({ current: null, options: [] });
        if (url.includes("/opencode/outbox"))
          return Response.json({ sessionId, revision: 1, paused: false, items: [] });
        if (url.includes("/opencode/events")) throw new Error("EventSource only");
        if (url.includes("/channel")) return Response.json({ via: "lan", limited: false });
        if (url.includes("/host"))
          return Response.json({
            host: { name: "n", platform: "darwin", version: "0", bootId: "b" },
          });
        return Response.json({});
      })
    );
    vi.stubGlobal("EventSource", MockEventSource as unknown as typeof EventSource);
  }

  function shellSession() {
    return {
      id: sessionId,
      agentType: "opencode",
      projectName: "proj",
      projectPath: "/tmp/proj",
      title: "t",
      gitBranch: null,
      githubUrl: null,
      status: "processing" as const,
      lastMessage: null,
      lastMessageRole: null,
      lastActivityAt: "2026-09-15T00:00:00Z",
      pid: 1,
      cpuUsage: 0,
      activeSubagentCount: 0,
      form: "cli" as const,
      jumpSupported: false,
      unread: false,
    };
  }

  function stubLandscapeMatchMedia() {
    window.matchMedia = ((query: string) => ({
      matches: query === "(min-aspect-ratio: 6/5)",
      media: query,
      onchange: null,
      addListener: () => {},
      removeListener: () => {},
      addEventListener: () => {},
      removeEventListener: () => {},
      dispatchEvent: () => false,
    })) as unknown as typeof window.matchMedia;
  }

  async function assertInvariants() {
    const input = await screen.findByTestId("native-input");
    // 阻断单处：绑定门恰一块，发送阻断行不再重复同一事实
    const gates = screen.getAllByTestId("native-binding-gate");
    expect(gates).toHaveLength(1);
    expect(screen.queryByTestId("native-send-blocked")).toBeNull();
    // 输入可用
    fireEvent.change(input, { target: { value: "双宽度输入" } });
    expect(input).toHaveValue("双宽度输入");
  }

  afterEach(() => {
    Reflect.deleteProperty(window, "matchMedia");
  });

  it("竖屏（jsdom 缺省 = 窄屏）", async () => {
    installShellFetch();
    render(<SessionDetail session={shellSession()} onBack={() => {}} />);
    await assertInvariants();
  });

  it("横屏（宽 > 高 × 1.2）", async () => {
    stubLandscapeMatchMedia();
    installShellFetch();
    render(<SessionDetail session={shellSession()} onBack={() => {}} />);
    await assertInvariants();
  });
});

describe("队列的门码呈现与按钮跟随能力", () => {
  const outboxItem = {
    actionId: "local-1",
    owner: "tuvis" as const,
    reasonCode: "OC_BINDING_STALE",
    state: "queued" as const,
    textSummary: "待发一条",
    attachmentCount: 0,
  };

  it("重绑被 OC_GATE_BUSY 拒绝：显示等待重试短语（非机器串），锚清除、控制仍可用", async () => {
    const posts: Record<string, unknown>[] = [];
    vi.stubGlobal(
      "fetch",
      vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
        const url = String(input);
        if (url.endsWith("/opencode/actions") && init?.method === "POST") {
          posts.push(JSON.parse(String(init.body)) as Record<string, unknown>);
          return Response.json(
            { error: "OC_GATE_BUSY", reasonCode: "OC_GATE_BUSY" },
            { status: 409 }
          );
        }
        if (url.includes("/opencode/outbox"))
          return Response.json({ sessionId: "q", revision: 1, paused: false, items: [outboxItem] });
        throw new Error(`unexpected fetch: ${url}`);
      })
    );
    const confirm = vi.spyOn(window, "confirm").mockReturnValue(true);
    render(<OpenCodeNativeQueue sessionId="q" bindingToken="binding" />);
    fireEvent.click(await screen.findByTestId("outbox-rebind-local-1"));
    await waitFor(() => expect(posts).toHaveLength(1));
    expect(confirm).toHaveBeenCalled();
    expect(await screen.findByTestId("native-outbox")).toHaveTextContent(
      t("mobile.openCodeNative.errors.gateBusy")
    );
    expect(screen.queryByText(/OC_GATE_/)).toBeNull();
    expect(
      screen.queryByText(t("mobile.openCodeNative.errors.instanceOwnershipUnverified"))
    ).toBeNull();
    // 确定零写出：锚清除、控制不锁死
    expect(sessionStorage.getItem("tuvis.opencodeNative.outboxControl.q")).toBeNull();
    expect(screen.getByTestId("outbox-rebind-local-1")).not.toBeDisabled();
  });

  it("视图层写阻断码为门码时：重绑禁用、本地撤回不受影响、队列不重复解释原因", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn(async (input: RequestInfo | URL) => {
        const url = String(input);
        if (url.includes("/opencode/outbox"))
          return Response.json({
            sessionId: url.includes("q3") ? "q3" : "q2",
            revision: 1,
            paused: false,
            items: url.includes("q3") ? [] : [outboxItem],
          });
        throw new Error(`unexpected fetch: ${url}`);
      })
    );
    const { rerender } = render(
      <OpenCodeNativeQueue
        sessionId="q2"
        bindingToken="binding"
        writeBlockedCode="OC_GATE_NO_ACK"
      />
    );
    await screen.findByTestId("outbox-local-local-1");
    expect(screen.getByTestId("outbox-rebind-local-1")).toBeDisabled();
    // 有待发项 ⇒ 队列区在场，其内只有一个「暂停派发」文字链（无独立框、无说明段）。
    expect(screen.getByTestId("outbox-pause")).toBeInTheDocument();
    expect(screen.getByTestId("outbox-retract-local-1")).not.toBeDisabled();
    // 队列只显示本地控制结果：归属/门的原因由会话级单处解释
    expect(
      screen.queryByText(t("mobile.openCodeNative.errors.instanceOwnershipUnverified"))
    ).toBeNull();
    expect(screen.queryByText(t("mobile.openCodeNative.errors.gateNoAck"))).toBeNull();

    // 2026-11-22 用户口径：空队列 + 未暂停 + 无未决 ⇒ 队列区整块不渲染，
    // 也不再有任何预暂停入口（暂停只在发送键上体现）。
    rerender(
      <OpenCodeNativeQueue
        sessionId="q3"
        bindingToken="binding"
        writeBlockedCode="OC_GATE_NO_ACK"
      />
    );
    await act(async () => {
      await Promise.resolve();
    });
    expect(screen.queryByTestId("native-outbox")).toBeNull();
    expect(screen.queryByTestId("outbox-pause")).toBeNull();
  });
});
