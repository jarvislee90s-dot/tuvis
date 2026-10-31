// S5 红测：Codex 发送流的产品界面（30 秒截止 / 前台五分类 / 重试关联 / 人工继续 /
// 队列暂停选择 / 报告入口安全引用）。
//
// 消费面 = S2/S3/S4 已登记接口（只消费不复刻）：
// - send-info `queuePaused`（S2；暂停不置灰——恢复 / 本条先发送 二选一）；
// - session-send `bypassPausedQueue` 一次性越队（S2）；NotSent 理由码枚举
//   `queue_paused` / `grant_consumed` / `native_pending` / `capability_unverified` /
//   `capability_unavailable`（S2 单源）；
// - `GET /codex/action/verify`（S4 四值 outcome）+ `POST /codex/action/manual-resume`
//   （S4 resumed/converged/already_resumed）+ force-continue（S4 accepted-unbound 解除）；
// - 前台 30 秒 = 前端本地单调计时（S3 统一预算的前台面），到期按服务端 stage 呈现。
//
// 文案断言钉中文（pinZhNavigator 同款）；fetch 全量 stub 按 URL 分路。
import { act, cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, afterAll, beforeAll, beforeEach, describe, expect, it, vi } from "vitest";
import MessageComposer from "@/mobile/MessageComposer";
import CodexQueuePanel from "@/mobile/CodexQueuePanel";
import type { SendInfo } from "@/mobile/api";

import i18n from "@/i18n";
import { pinZhNavigator } from "./pinZhNavigator";

let unpinZh: () => void;

beforeAll(async () => {
  unpinZh = await pinZhNavigator();
});

afterAll(() => {
  unpinZh();
});

afterEach(() => {
  cleanup();
  vi.useRealTimers();
  vi.unstubAllGlobals();
});

/** 可注入的 Codex APP 会话（channels 带 headless_codex_ 前缀 → 需要 actionId） */
function codexSendInfo(over: Partial<SendInfo> = {}): SendInfo {
  return {
    injectable: true,
    channels: ["headless_codex_queue"],
    visibility: "realtime",
    ...over,
  };
}

/** desktop_ipc 回执夹具（ActionReceipt 形状） */
function ipcReceipt(over: Record<string, unknown> = {}) {
  return {
    actionId: "act-1",
    delivery: "accepted",
    route: "desktop_ipc",
    turnId: null,
    envelopeId: null,
    reasonCode: null,
    ...over,
  };
}

interface Routes {
  info?: SendInfo;
  /** POST /session-send 回执序列（每次调用按序弹出；缺省 codex_queued） */
  sendQueue?: Record<string, unknown>[];
  /** POST /session-send 挂起（手动放行；F6 起逐请求独立控制器 hangingSends） */
  sendHang?: boolean;
  /** POST /session-send 网络层异常（fetch reject） */
  sendReject?: boolean;
  /** F6：/session-approve-options 探测挂起（前置卡片探测吃预算场景；releaseApproveOptions 放行） */
  approveOptionsHang?: boolean;
  /** R7：/session-approve-options 载荷序列（按次弹出）——A 前探迟到返回用
   *  真实端点可返回的 plan_pending 形状（api.rs:2921 available=true），非违约 mock。 */
  approveOptionsSequence?: Array<{
    available: boolean;
    planPending?: boolean;
    options?: unknown[];
  }>;
  /** GET /codex/action/verify 载荷序列（按次弹出） */
  verifyQueue?: Record<string, unknown>[];
  /** POST /codex/action/manual-resume 载荷 */
  resume?: Record<string, unknown>;
  resumeStatus?: number;
  resumeBody?: Record<string, unknown>;
  /** Q2：manual-resume 挂起（人工继续响应迟到场景；releaseResumeWith 放行） */
  resumeHang?: boolean;
  /** POST /codex/queue/force-continue */
  forceStatus?: number;
  forceBody?: Record<string, unknown>;
  /** POST /codex/queue/resume 是否已调用（断言） */
  queueResumeCalls?: string[];
  /** GET /codex/queue 队列视图（CodexQueuePanel 用） */
  queueView?: Record<string, unknown>;
  bypassFlag?: boolean;
}

let routes: Routes;
let fetchMock: ReturnType<typeof vi.fn>;
let releaseSend: ((r: Response) => void) | null = null;
/** F6 定序：挂起的 session-send 逐请求控制器（A/B 各自独立放行/拒绝） */
interface HangingSend {
  resolve: (r: Response) => void;
  reject: (e: unknown) => void;
}
let hangingSends: HangingSend[] = [];
/** F6：挂起的 approve-options 探测放行器 */
let releaseApproveOptions: ((r: Response) => void) | null = null;
/** Q2：挂起的 manual-resume 逐请求控制器（A 的人工继续响应迟到场景） */
let hangingResumes: HangingSend[] = [];
const sendBodies: string[] = [];
const verifyUrls: string[] = [];
const resumeBodies: string[] = [];
const forceBodies: string[] = [];
const queueResumeBodies: string[] = [];

beforeEach(() => {
  routes = {};
  releaseSend = null;
  hangingSends = [];
  hangingResumes = [];
  releaseApproveOptions = null;
  sendBodies.length = 0;
  verifyUrls.length = 0;
  resumeBodies.length = 0;
  forceBodies.length = 0;
  queueResumeBodies.length = 0;
});

function json(body: unknown, status = 200) {
  return new Response(JSON.stringify(body), { status });
}

function installFetch() {
  fetchMock = vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
    const url = String(input);
    if (url.includes("/m/api/v1/channel")) {
      return json({ via: "lan", limited: false, est_mbps_down: 0, est_mbps_up: 0 });
    }
    // 长路径先判（前缀包含关系）
    if (url.includes("/codex/action/verify")) {
      verifyUrls.push(url);
      const next = routes.verifyQueue?.shift();
      return json(next ?? { outcome: "unresolved", stage: "dispatching" });
    }
    if (url.includes("/codex/action/manual-resume")) {
      resumeBodies.push(String(init?.body ?? ""));
      if (routes.resumeHang) {
        return new Promise<Response>((resolve, reject) => {
          hangingResumes.push({ resolve, reject });
        });
      }
      if (routes.resumeStatus)
        return json(routes.resumeBody ?? { error: "conflict" }, routes.resumeStatus);
      return json(routes.resume ?? { status: "resumed", successorActionId: "act-succ", send: {} });
    }
    if (url.includes("/codex/queue/resume")) {
      queueResumeBodies.push(String(init?.body ?? ""));
      return json({ ok: true, paused: false });
    }
    if (url.includes("/codex/queue/force-continue")) {
      forceBodies.push(String(init?.body ?? ""));
      if (routes.forceStatus)
        return json(routes.forceBody ?? { error: "target_not_inputtable" }, routes.forceStatus);
      return json({ actionId: "act-1", state: "accepted", position: null, receipt: null });
    }
    if (url.includes("/codex/queue")) {
      return json(routes.queueView ?? { actions: [], blockedReasonCode: null });
    }
    if (url.includes("/codex/action")) {
      return json({ actionId: "act-1", state: "queued", position: null, receipt: null });
    }
    if (url.includes("/session-send-info")) {
      return json(routes.info ?? codexSendInfo());
    }
    if (url.includes("/session-approve-options")) {
      if (routes.approveOptionsHang) {
        return new Promise<Response>((resolve) => {
          releaseApproveOptions = resolve;
        });
      }
      if (routes.approveOptionsSequence && routes.approveOptionsSequence.length > 0) {
        return json(routes.approveOptionsSequence.shift());
      }
      return json({
        available: false,
        options: [],
        verifiedWith: "t",
        currentVersion: null,
        drift: false,
      });
    }
    if (url.includes("/session-question")) {
      return json({ available: false, questions: [] });
    }
    if (url.includes("/session-send")) {
      if (routes.sendHang) {
        return new Promise<Response>((resolve, reject) => {
          hangingSends.push({ resolve, reject });
          releaseSend = resolve; // 兼容单挂起用例：指向最新一个
        });
      }
      if (routes.sendReject) throw new TypeError("session-send 网络断开（模拟运输层失败）");
      sendBodies.push(String(init?.body ?? ""));
      return json(
        routes.sendQueue?.shift() ?? { status: "codex_queued", actionId: "act-q", position: 1 }
      );
    }
    if (url.includes("/session-queue")) {
      return json({ items: [] });
    }
    return json({});
  });
  vi.stubGlobal("fetch", fetchMock);
}

async function flushAsync() {
  for (let i = 0; i < 8; i += 1) {
    await act(async () => {
      await Promise.resolve();
    });
  }
}

/** 从待确认面板读原动作短 ID（前 8 位；挂起的发送请求体不可得，面板是唯一展示面） */
function panelActionShort(): string {
  const text = screen.getByTestId("codex-recovery-action-id").textContent ?? "";
  const prefix = i18n.t("mobile.codex.composer.recoveryActionRef", { short: "" });
  return text.replace(prefix, "").trim();
}

async function sendText(text: string) {
  const input = (await screen.findByTestId("composer-input")) as HTMLTextAreaElement;
  await act(async () => {
    fireEvent.change(input, { target: { value: text } });
  });
  await flushAsync();
  fireEvent.click(screen.getByTestId("composer-send"));
  await flushAsync();
}

/** Q2：放行最早挂起的 manual-resume（以 routes.resume 为迟到响应载荷） */
function releaseResumeWith() {
  const h = hangingResumes.shift();
  if (h && routes.resume) h.resolve(json(routes.resume));
}

// ============================================================
// 30 秒截止与待确认面板
// ============================================================
describe("CodexSendFlow：30 秒截止与待确认（S5）", () => {
  it("deadline_30s：发送挂起 30 秒 → 前端本地强制结束转圈；待确认面板出「检查是否送达」「报告问题」，不出普通重试；此后迟到回执不覆盖面板", async () => {
    vi.useFakeTimers();
    installFetch();
    routes.sendHang = true;
    render(<MessageComposer session={{ id: "sess-1" }} />);
    await flushAsync();
    await act(async () => {
      fireEvent.change(screen.getByTestId("composer-input"), { target: { value: "排查一下构建" } });
    });
    await flushAsync();
    fireEvent.click(screen.getByTestId("composer-send"));
    await flushAsync();
    // 转圈在场
    expect(screen.getByTestId("send-receipt-delivering")).toBeTruthy();

    // 本地单调计时 30 秒到期
    await act(async () => {
      await vi.advanceTimersByTimeAsync(30_000);
    });
    // 转圈结束
    expect(screen.queryByTestId("send-receipt-delivering")).toBeNull();
    // 待确认面板在场：可核对、可报告；没有普通重试
    expect(screen.getByTestId("codex-recovery-panel")).toBeTruthy();
    expect(screen.getByTestId("codex-recovery-check")).toBeTruthy();
    expect(screen.getByTestId("codex-recovery-report")).toBeTruthy();
    expect(screen.queryByTestId("codex-recovery-resend")).toBeNull();
    // 迟到的原始回执（unknown_delivery）不覆盖面板（面板接管呈现，核对走账本）
    await act(async () => {
      releaseSend!(
        json({ status: "desktop_ipc", receipt: ipcReceipt({ delivery: "unknown_delivery" }) })
      );
    });
    await flushAsync();
    expect(screen.getByTestId("codex-recovery-panel")).toBeTruthy();
    expect(screen.queryByTestId("send-receipt-codex-unknown")).toBeNull();
  });

  it("verify_delivered：核对返回 delivered → 「已确认送达」（正式回显口径）", async () => {
    vi.useFakeTimers();
    installFetch();
    routes.sendHang = true;
    render(<MessageComposer session={{ id: "sess-1" }} />);
    await flushAsync();
    await act(async () => {
      fireEvent.change(screen.getByTestId("composer-input"), { target: { value: "排查一下构建" } });
    });
    await flushAsync();
    fireEvent.click(screen.getByTestId("composer-send"));
    await flushAsync();
    await act(async () => {
      await vi.advanceTimersByTimeAsync(30_000);
    });
    routes.verifyQueue = [
      {
        actionId: "act-1",
        stage: "settled",
        outcome: "delivered",
        turnId: "t1",
        runLimitReleased: false,
        reasonCode: null,
        verifiedAt: 1,
      },
    ];
    fireEvent.click(screen.getByTestId("codex-recovery-check"));
    await flushAsync();
    expect(screen.getByTestId("codex-recovery-result")).toBeTruthy();
    expect(screen.getByTestId("codex-recovery-result").textContent).toContain("已确认送达");
  });

  it("accepted_unbound：核对 accepted_unbound → 只给「解除运行限制」，不给重发；解除走 force-continue 且不重发原消息", async () => {
    vi.useFakeTimers();
    installFetch();
    routes.sendHang = true;
    render(<MessageComposer session={{ id: "sess-1" }} />);
    await flushAsync();
    await act(async () => {
      fireEvent.change(screen.getByTestId("composer-input"), { target: { value: "继续跑" } });
    });
    await flushAsync();
    fireEvent.click(screen.getByTestId("composer-send"));
    await flushAsync();
    await act(async () => {
      await vi.advanceTimersByTimeAsync(30_000);
    });
    routes.verifyQueue = [
      {
        actionId: "act-1",
        stage: "settled",
        outcome: "accepted_unbound",
        turnId: null,
        runLimitReleased: false,
        reasonCode: null,
        verifiedAt: 1,
      },
    ];
    fireEvent.click(screen.getByTestId("codex-recovery-check"));
    await flushAsync();
    expect(screen.getByTestId("codex-recovery-result").textContent).toContain("已受理");
    // 无重发出口（已受理的消息不重发）
    expect(screen.queryByTestId("codex-recovery-resend")).toBeNull();
    // 只解除运行限制
    fireEvent.click(screen.getByTestId("codex-recovery-release"));
    await flushAsync();
    expect(forceBodies).toHaveLength(1);
    // 靶向原动作（发送挂起 → 请求体不可得；面板展示的动作短 ID 即其前 8 位）
    const forced = JSON.parse(forceBodies[0]) as { actionId: string };
    expect(forced.actionId).toBeTruthy();
    expect(forced.actionId.startsWith(panelActionShort())).toBe(true);
    // 解除成功反馈在 note 位（结果行保留「已受理」事实）
    expect(screen.getByTestId("codex-recovery-note").textContent).toContain("已解除运行限制");
    expect(screen.getByTestId("codex-recovery-result").textContent).toContain("已受理");
  });

  it("manual_resume：核对 unresolved → 「仍要重新发送（可能重复）」先展示三件风险再确认；确认后建新动作（newActionId ≠ 原 ID）且正文同原消息；converged 显示最新结果不重发", async () => {
    vi.useFakeTimers();
    installFetch();
    routes.sendHang = true;
    render(<MessageComposer session={{ id: "sess-1" }} />);
    await flushAsync();
    await act(async () => {
      fireEvent.change(screen.getByTestId("composer-input"), { target: { value: "重启服务" } });
    });
    await flushAsync();
    fireEvent.click(screen.getByTestId("composer-send"));
    await flushAsync();
    await act(async () => {
      await vi.advanceTimersByTimeAsync(30_000);
    });
    routes.verifyQueue = [
      {
        actionId: "act-1",
        stage: "dispatching",
        outcome: "unresolved",
        turnId: null,
        runLimitReleased: false,
        reasonCode: null,
        verifiedAt: 1,
      },
    ];
    fireEvent.click(screen.getByTestId("codex-recovery-check"));
    await flushAsync();
    expect(screen.getByTestId("codex-recovery-result").textContent).toContain("没有找到足够证据");
    // 第一步：只展开风险确认，不发请求
    fireEvent.click(screen.getByTestId("codex-recovery-resend"));
    await flushAsync();
    expect(resumeBodies).toHaveLength(0);
    const risks = screen.getByTestId("codex-recovery-resend-risks").textContent;
    expect(risks).toContain("仍可能迟到");
    expect(risks).toContain("新消息");
    expect(risks).toContain("不是证明原消息未送达");
    // 第二步：确认 → manual-resume（新 actionId + 原文）
    routes.resume = {
      status: "resumed",
      predecessor: {},
      successorActionId: "act-succ-9",
      send: { receipt: ipcReceipt({ actionId: "act-succ-9", delivery: "accepted" }) },
    };
    fireEvent.click(screen.getByTestId("codex-recovery-resend-confirm"));
    await flushAsync();
    expect(resumeBodies).toHaveLength(1);
    const body = JSON.parse(resumeBodies[0]) as Record<string, string>;
    expect(body.actionId).toBeTruthy();
    expect(body.newActionId).toBeTruthy();
    expect(body.newActionId).not.toBe(body.actionId);
    expect(body.text).toBe("重启服务");
    expect(screen.getByTestId("codex-recovery-result").textContent).toContain("已重新发送");
    // converged：确认期间已收敛 → 显示最新结果，不建后继不重发
    //（复审 W2：converged 段前移——already_resumed 后核对锚已切后继，原动作不再提供重发）
    routes.verifyQueue = [
      {
        actionId: "act-1",
        stage: "settled",
        outcome: "unresolved",
        turnId: null,
        runLimitReleased: false,
        reasonCode: null,
        verifiedAt: 2,
      },
    ];
    routes.resume = {
      status: "converged",
      action: { actionId: "act-1", state: "accepted", position: null, receipt: ipcReceipt({}) },
    };
    fireEvent.click(screen.getByTestId("codex-recovery-check"));
    await flushAsync();
    fireEvent.click(screen.getByTestId("codex-recovery-resend"));
    await flushAsync();
    fireEvent.click(screen.getByTestId("codex-recovery-resend-confirm"));
    await flushAsync();
    expect(resumeBodies).toHaveLength(2);
    expect(screen.getByTestId("codex-recovery-result").textContent).toContain("已有结果");
    // 重放幂等回执（already_resumed，W2）：**不凭存在关系宣称投递成功**——如实显示
    // 「此前已创建后继…状态待查」，不显示「已重新发送」
    routes.verifyQueue = [
      {
        actionId: "act-1",
        stage: "settled",
        outcome: "unresolved",
        turnId: null,
        runLimitReleased: false,
        reasonCode: null,
        verifiedAt: 3,
      },
    ];
    routes.resume = { status: "already_resumed", predecessor: {}, successorActionId: "act-succ-9" };
    fireEvent.click(screen.getByTestId("codex-recovery-check"));
    await flushAsync();
    fireEvent.click(screen.getByTestId("codex-recovery-resend"));
    await flushAsync();
    expect(resumeBodies).toHaveLength(2);
    fireEvent.click(screen.getByTestId("codex-recovery-resend-confirm"));
    await flushAsync();
    expect(resumeBodies).toHaveLength(3);
    const alreadyText = screen.getByTestId("codex-recovery-result").textContent ?? "";
    expect(alreadyText).toContain("此前已创建后继");
    expect(alreadyText).toContain("act-succ");
    expect(alreadyText).not.toContain("已重新发送");
  });

  it("write_right_in_flight（复审 round 4 V2）：在途未收尾的明确状态保持待确认；converged+unknown 防御性按待确认——绝不渲染「已有结果」假收敛", async () => {
    vi.useFakeTimers();
    installFetch();
    routes.sendHang = true;
    render(<MessageComposer session={{ id: "sess-1" }} />);
    await flushAsync();
    await act(async () => {
      fireEvent.change(screen.getByTestId("composer-input"), { target: { value: "在途确认" } });
    });
    await flushAsync();
    fireEvent.click(screen.getByTestId("composer-send"));
    await flushAsync();
    await act(async () => {
      await vi.advanceTimersByTimeAsync(30_000);
    });
    routes.verifyQueue = [
      {
        actionId: "act-1",
        stage: "settled",
        outcome: "unresolved",
        turnId: null,
        runLimitReleased: false,
        reasonCode: null,
        verifiedAt: 1,
      },
    ];
    routes.resume = {
      status: "write_right_in_flight",
      action: { actionId: "act-1", state: "reserved", position: null, receipt: ipcReceipt({}) },
    };
    fireEvent.click(screen.getByTestId("codex-recovery-check"));
    await flushAsync();
    fireEvent.click(screen.getByTestId("codex-recovery-resend"));
    await flushAsync();
    fireEvent.click(screen.getByTestId("codex-recovery-resend-confirm"));
    await flushAsync();
    // 在途明确状态：note 提示等待回执；result 不渲染「已有结果」假收敛
    expect(screen.getByTestId("codex-recovery-note").textContent).toContain("正在发送等待回执");
    expect(screen.getByTestId("codex-recovery-result").textContent).not.toContain("已有结果");

    // converged + unknown（后端修复后不再出现，防御性）：不渲染「已有结果」
    routes.verifyQueue = [
      {
        actionId: "act-1",
        stage: "settled",
        outcome: "unresolved",
        turnId: null,
        runLimitReleased: false,
        reasonCode: null,
        verifiedAt: 2,
      },
    ];
    routes.resume = {
      status: "converged",
      action: { actionId: "act-1", state: "unknown", position: null, receipt: ipcReceipt({}) },
    };
    fireEvent.click(screen.getByTestId("codex-recovery-check"));
    await flushAsync();
    fireEvent.click(screen.getByTestId("codex-recovery-resend"));
    await flushAsync();
    fireEvent.click(screen.getByTestId("codex-recovery-resend-confirm"));
    await flushAsync();
    expect(screen.getByTestId("codex-recovery-note").textContent).toContain("仍在等待桌面回执");
    expect(screen.getByTestId("codex-recovery-result").textContent).not.toContain("已有结果");
  });

  it("verify_not_delivered：核对 not_delivered → 明确未送达 + 重试（新动作）+ 报告", async () => {
    vi.useFakeTimers();
    installFetch();
    routes.sendHang = true;
    render(<MessageComposer session={{ id: "sess-1" }} />);
    await flushAsync();
    await act(async () => {
      fireEvent.change(screen.getByTestId("composer-input"), { target: { value: "再试一次" } });
    });
    await flushAsync();
    fireEvent.click(screen.getByTestId("composer-send"));
    await flushAsync();
    await act(async () => {
      await vi.advanceTimersByTimeAsync(30_000);
    });
    routes.verifyQueue = [
      {
        actionId: "act-1",
        stage: "settled",
        outcome: "not_delivered",
        turnId: null,
        runLimitReleased: false,
        reasonCode: "native_pending",
        verifiedAt: 1,
      },
    ];
    fireEvent.click(screen.getByTestId("codex-recovery-check"));
    await flushAsync();
    expect(screen.getByTestId("codex-recovery-result").textContent).toContain("未送达");
    // 重试（新动作）：先放行 session-send（第一发仍挂起）。
    // 假定时器下不用 waitFor（其内部计时器被劫持会挂起）——微任务冲刷即可
    routes.sendHang = false;
    routes.sendQueue = [
      { status: "desktop_ipc", receipt: ipcReceipt({ actionId: "act-2", delivery: "accepted" }) },
    ];
    const originalShort = panelActionShort(); // 点击后面板被新回执替换，先捕获
    fireEvent.click(screen.getByTestId("codex-recovery-retry"));
    await flushAsync();
    expect(sendBodies).toHaveLength(1);
    const body = JSON.parse(sendBodies[0]) as Record<string, string>;
    expect(body.actionId).toBeTruthy();
    // 新动作 ≠ 原动作（与面板展示的原动作短 ID 前缀对不上 = 换了新 ID）
    expect(body.actionId.startsWith(originalShort)).toBe(false);
    expect(body.text).toBe("再试一次");
    // 关联展示：新回执注明原动作（安全引用，前 8 位）
    expect(screen.getByTestId("codex-retried-from").textContent).toContain(originalShort);
  });
});

// ============================================================
// 明确未送达（NotSent）重试关联与运输重试同动作
// ============================================================
describe("CodexSendFlow：NotSent 重试关联与运输重试（S5）", () => {
  it("not_sent_retry_links_new_action：明确 NotSent（queue_paused）→ 保留草稿 + 理由文案 + 重试创建关联的新动作（不重放原失败）", async () => {
    installFetch();
    routes.sendQueue = [
      {
        status: "desktop_ipc",
        receipt: ipcReceipt({
          actionId: "act-old",
          delivery: "not_sent",
          reasonCode: "queue_paused",
        }),
      },
    ];
    render(<MessageComposer session={{ id: "sess-1" }} />);
    await sendText("排队期间的意见");
    // 明确未送达：草稿保留 + 理由码专属文案（非 raw 码）
    expect((screen.getByTestId("composer-input") as HTMLTextAreaElement).value).toBe(
      "排队期间的意见"
    );
    const chip = screen.getByTestId("send-receipt-codex-failed");
    expect(chip.textContent).toContain("队列已暂停");
    expect(screen.getByTestId("codex-retry-notsent")).toBeTruthy();
    expect(screen.getByTestId("codex-report-notsent")).toBeTruthy();
    // 重试 = 新动作（不同 actionId），成功后显示关联
    routes.sendQueue = [
      { status: "desktop_ipc", receipt: ipcReceipt({ actionId: "act-new", delivery: "accepted" }) },
    ];
    fireEvent.click(screen.getByTestId("codex-retry-notsent"));
    await flushAsync();
    expect(sendBodies).toHaveLength(2);
    const second = JSON.parse(sendBodies[1]) as Record<string, string>;
    expect(second.actionId).not.toBe("act-old");
    // 关联展示：新回执注明原动作
    expect(screen.getByTestId("send-receipt-codex-accepted").textContent).toContain("已受理");
    expect(screen.getByTestId("codex-retried-from").textContent).toContain("act-old".slice(0, 8));
  });

  it("transport_retry_same_action：网络层失败（结果未知）→ 待确认面板；同草稿再按发送沿同一 actionId（运输重试不换动作）", async () => {
    installFetch();
    routes.sendReject = true;
    render(<MessageComposer session={{ id: "sess-1" }} />);
    await sendText("同一条消息");
    expect(screen.getByTestId("codex-recovery-panel")).toBeTruthy();
    // 再按发送：同一 actionId（运输重试不换动作；服务端账本去重防双投）。
    // 第一次发送在网络层失败、请求体不入 capture——重发的 actionId 与面板展示的
    // 原动作短 ID 同前缀即同一动作。
    routes.sendReject = false;
    routes.sendQueue = [{ status: "desktop_ipc", receipt: ipcReceipt({ delivery: "accepted" }) }];
    const originalShort = panelActionShort();
    fireEvent.click(screen.getByTestId("composer-send"));
    await waitFor(() => expect(sendBodies).toHaveLength(1));
    const retried = JSON.parse(sendBodies[0]) as Record<string, string>;
    expect(retried.actionId.startsWith(originalShort)).toBe(true);
    expect(retried.text).toBe("同一条消息");
  });

  it("accepted_not_completed：已受理回执显示「已受理/正在执行」，不写「已完成」；无重试入口", async () => {
    installFetch();
    routes.sendQueue = [
      { status: "desktop_ipc", receipt: ipcReceipt({ delivery: "accepted", turnId: "t-1" }) },
    ];
    render(<MessageComposer session={{ id: "sess-1" }} />);
    await sendText("开始吧");
    const chip = screen.getByTestId("send-receipt-codex-accepted");
    expect(chip.textContent).toContain("已受理");
    expect(chip.textContent).not.toContain("已完成");
    expect(chip.textContent).not.toContain("已送达");
    expect(screen.queryByTestId("codex-retry-notsent")).toBeNull();
  });
});

// ============================================================
// 队列暂停展示收敛（C04：解释/恢复/先发送只在队列区；composer 只留入队回执）
// ============================================================
describe("CodexSendFlow：队列暂停展示收敛（C04）", () => {
  it("c04_queue_paused_composer_clean：send-info queuePaused=true → composer 不渲染暂停二选一；正常发送只出简短入队回执", async () => {
    installFetch();
    routes.info = codexSendInfo({ queuePaused: true });
    render(<MessageComposer session={{ id: "sess-1" }} />);
    // 暂停解释/恢复/先发送三件全部不在 composer（归队列区 CodexQueuePanel）
    await flushAsync();
    expect(screen.queryByTestId("send-queue-paused")).toBeNull();
    expect(screen.queryByTestId("queue-resume")).toBeNull();
    expect(screen.queryByTestId("queue-bypass-arm")).toBeNull();
    // 暂停不置灰：正常输入发送 → 服务端留队 → 简短入队回执
    await sendText("排队意见");
    await waitFor(() => expect(screen.getByTestId("send-receipt-codex-queued")).toBeTruthy());
    // 回执不在 composer 触发任何越队参数（载荷无 bypassPausedQueue）
    const queuedBody = sendBodies.map((b) => JSON.parse(b) as Record<string, unknown>);
    expect(queuedBody.some((body) => body.bypassPausedQueue === true)).toBe(false);
  });

  it("c04_composer_has_no_resume：暂停下 composer 也无恢复按钮（唯一入口在队列区，见面板用例）", async () => {
    installFetch();
    routes.info = codexSendInfo({ queuePaused: true });
    render(<MessageComposer session={{ id: "sess-1" }} />);
    await flushAsync();
    expect(screen.queryByTestId("queue-resume")).toBeNull();
  });

  it("queue_panel_empty_pause：无排队条目也无屏障 → 只有陈旧暂停行时面板自隐", async () => {
    installFetch();
    routes.queueView = {
      actions: [],
      blockedReasonCode: null,
      queuePaused: true,
      queuePauseReason: "interrupt_uncertain",
    };
    render(<CodexQueuePanel sessionId="sess-1" />);
    await flushAsync();
    expect(screen.queryByTestId("codex-queue-panel")).toBeNull();
    expect(screen.queryByTestId("codex-queue-paused")).toBeNull();
  });

  it("queue_panel_paused：队列视图 queuePaused=true → 展开后出暂停说明与唯一恢复队列入口（同一解除出口）", async () => {
    installFetch();
    routes.queueView = {
      actions: [
        { actionId: "act-q1", state: "queued", position: 2, receipt: null, stage: "queued" },
      ],
      blockedReasonCode: null,
      queuePaused: true,
      queuePauseReason: "interrupt_uncertain",
    };
    render(<CodexQueuePanel sessionId="sess-1" />);
    await waitFor(() => expect(screen.getByTestId("codex-queue-panel")).toBeTruthy());
    // C04：默认折叠（摘要行）；展开后才出暂停说明与恢复入口
    fireEvent.click(screen.getByTestId("codex-queue-toggle"));
    expect(screen.getByTestId("codex-queue-paused")).toBeTruthy();
    fireEvent.click(screen.getByTestId("codex-queue-resume"));
    await flushAsync();
    expect(queueResumeBodies).toHaveLength(1);
  });
});

// ============================================================
// 报告入口（R5 前身）：统一可接线事件，payload 只含安全引用
// ============================================================
describe("CodexSendFlow：报告入口安全引用（S5）", () => {
  it("report_entry_safe_refs：诊断服务未部署 → 明确「诊断服务未开启」+ 复制摘要；摘要不含正文/日志/路径", async () => {
    installFetch();
    routes.sendQueue = [
      {
        status: "desktop_ipc",
        receipt: ipcReceipt({
          actionId: "act-rpt",
          delivery: "not_sent",
          reasonCode: "capability_unavailable",
        }),
      },
    ];
    render(<MessageComposer session={{ id: "sess-1" }} />);
    await sendText("这是不应进入报告的正文内容 abc/secret/path");
    fireEvent.click(screen.getByTestId("codex-report-notsent"));
    await flushAsync();
    // 未部署 → 明确不可用 + 复制出口（不伪造成功）
    expect(screen.getByTestId("codex-report-unavailable").textContent).toContain("诊断服务未开启");
    expect(screen.getByTestId("codex-report-copy")).toBeTruthy();
    const writeText = vi.fn(async () => {});
    Object.defineProperty(window.navigator, "clipboard", {
      value: { writeText },
      configurable: true,
    });
    fireEvent.click(screen.getByTestId("codex-report-copy"));
    await flushAsync();
    expect(writeText).toHaveBeenCalledTimes(1);
    const summary = String(writeText.mock.calls[0][0]);
    expect(summary).toContain("tool=codex");
    expect(summary).toContain("act-rpt");
    expect(summary).toContain("capability_unavailable");
    // 安全引用纪律：正文、路径形状一律不进摘要
    expect(summary).not.toContain("不应进入报告的正文内容");
    expect(summary).not.toContain("secret/path");
  });
});

// ============================================================
// 终审 F6：30 秒前台预算必须覆盖前置卡片探测段，且旧发送的迟到收尾
// 不得破坏新动作的计时器 / 转圈 / 回执（连续场景定序：A 超时 → B 开始 → A 迟到）。
// F7：待确认面板的状态必须按 actionId 隔离——新动作不继承旧动作的核对结论、
// 确认臂与 busy 态（连续两次 Unknown：A 核对 not_delivered → 重试建 B → B Unknown）。
// ============================================================
describe("CodexSendFlow：F6 前台预算覆盖探测段 + 迟到响应归属守卫", () => {
  async function typeAndSend(text: string) {
    const input = screen.getByTestId("composer-input") as HTMLTextAreaElement;
    await act(async () => {
      fireEvent.change(input, { target: { value: text } });
    });
    await flushAsync();
    fireEvent.click(screen.getByTestId("composer-send"));
    await flushAsync();
  }

  it("f6_probe_consumes_front_budget：前置卡片探测挂起也吃 30 秒预算——转圈准时结束、待确认面板出现；此后迟到回执不覆盖面板", async () => {
    vi.useFakeTimers();
    installFetch();
    routes.approveOptionsHang = true;
    routes.sendHang = true;
    render(<MessageComposer session={{ id: "sess-1" }} />);
    await flushAsync();
    await typeAndSend("探测挂起期间的消息");
    // 探测（/session-approve-options）挂起：session-send 尚未发出，转圈在场
    expect(sendBodies).toHaveLength(0);
    expect(hangingSends).toHaveLength(0);
    expect(screen.getByTestId("send-receipt-delivering")).toBeTruthy();
    // 预算从点击起算——探测段消费同一窗，30 秒到期强制结束转圈
    await act(async () => {
      await vi.advanceTimersByTimeAsync(30_000);
    });
    expect(screen.queryByTestId("send-receipt-delivering")).toBeNull();
    expect(screen.getByTestId("codex-recovery-panel")).toBeTruthy();
    // 探测此后才返回 → 发送照常发出 → 回执迟到：不覆盖已接管的面板
    await act(async () => {
      releaseApproveOptions!(
        json({
          available: false,
          options: [],
          verifiedWith: "t",
          currentVersion: null,
          drift: false,
        })
      );
    });
    await flushAsync();
    expect(hangingSends).toHaveLength(1);
    await act(async () => {
      hangingSends[0].resolve(
        json({ status: "desktop_ipc", receipt: ipcReceipt({ delivery: "accepted" }) })
      );
    });
    await flushAsync();
    expect(screen.getByTestId("codex-recovery-panel")).toBeTruthy();
    expect(screen.queryByTestId("send-receipt-codex-accepted")).toBeNull();
  });

  it("f6a_stale_success_cannot_touch_new_send：A 超时→B 开始→A 迟到成功——B 转圈不被关、30 秒准时报到、面板/回执不被 A 改写", async () => {
    vi.useFakeTimers();
    installFetch();
    routes.sendHang = true;
    render(<MessageComposer session={{ id: "sess-1" }} />);
    await flushAsync();
    // A：挂起 → 30 秒 → 待确认面板
    await typeAndSend("A 的消息");
    expect(hangingSends).toHaveLength(1);
    await act(async () => {
      await vi.advanceTimersByTimeAsync(30_000);
    });
    expect(screen.getByTestId("codex-recovery-panel")).toBeTruthy();
    const aShort = panelActionShort();
    // B：新逻辑发送（正文不同 → 必须是新动作）
    await typeAndSend("B 的消息");
    expect(hangingSends).toHaveLength(2);
    expect(screen.getByTestId("send-receipt-delivering")).toBeTruthy();
    // A 迟到成功：不得清 B 的计时器、不得关 B 的转圈、不得写 A 的回执
    await act(async () => {
      hangingSends[0].resolve(
        json({ status: "desktop_ipc", receipt: ipcReceipt({ delivery: "accepted" }) })
      );
    });
    await flushAsync();
    expect(screen.getByTestId("send-receipt-delivering")).toBeTruthy();
    expect(screen.queryByTestId("send-receipt-codex-accepted")).toBeNull();
    // B 的 30 秒准时报到（A 的迟到响应清不掉 B 的计时器）
    await act(async () => {
      await vi.advanceTimersByTimeAsync(30_000);
    });
    expect(screen.queryByTestId("send-receipt-delivering")).toBeNull();
    expect(screen.getByTestId("codex-recovery-panel")).toBeTruthy();
    expect(panelActionShort()).not.toBe(aShort); // 面板是 B 自己的动作
  });

  it("f6b_stale_rejection_cannot_touch_new_send：A 超时→B 开始→A 迟到失败（网络层）——B 状态不被 A 改写、B 准时移交面板", async () => {
    vi.useFakeTimers();
    installFetch();
    routes.sendHang = true;
    render(<MessageComposer session={{ id: "sess-1" }} />);
    await flushAsync();
    await typeAndSend("A 的消息");
    await act(async () => {
      await vi.advanceTimersByTimeAsync(30_000);
    });
    expect(screen.getByTestId("codex-recovery-panel")).toBeTruthy();
    await typeAndSend("B 的消息");
    expect(screen.getByTestId("send-receipt-delivering")).toBeTruthy();
    // A 迟到失败（catch 臂）：同样不得清 B 的计时器 / 关 B 的转圈
    await act(async () => {
      hangingSends[0].reject(new TypeError("A 的请求在网络层断开"));
    });
    await flushAsync();
    expect(screen.getByTestId("send-receipt-delivering")).toBeTruthy();
    await act(async () => {
      await vi.advanceTimersByTimeAsync(30_000);
    });
    expect(screen.queryByTestId("send-receipt-delivering")).toBeNull();
    expect(screen.getByTestId("codex-recovery-panel")).toBeTruthy();
  });
});

describe("CodexSendFlow：F7 待确认面板按动作隔离（连续两次 Unknown）", () => {
  it("f7_recovery_state_isolated_per_action：A Unknown→核对 not_delivered→重试建 B→B Unknown——面板重置，不继承 A 的结论/按钮，核对靶向 B", async () => {
    vi.useFakeTimers();
    installFetch();
    routes.sendHang = true;
    render(<MessageComposer session={{ id: "sess-1" }} />);
    await flushAsync();
    const input = screen.getByTestId("composer-input") as HTMLTextAreaElement;
    // A：挂起 → 30 秒 → 面板 → 核对 not_delivered（终态 + 重试按钮）
    await act(async () => {
      fireEvent.change(input, { target: { value: "第一条消息" } });
    });
    await flushAsync();
    fireEvent.click(screen.getByTestId("composer-send"));
    await flushAsync();
    await act(async () => {
      await vi.advanceTimersByTimeAsync(30_000);
    });
    const aShort = panelActionShort();
    routes.verifyQueue = [
      {
        actionId: "a-full",
        stage: "settled",
        outcome: "not_delivered",
        turnId: null,
        runLimitReleased: false,
        reasonCode: "native_pending",
        verifiedAt: 1,
      },
    ];
    fireEvent.click(screen.getByTestId("codex-recovery-check"));
    await flushAsync();
    expect(screen.getByTestId("codex-recovery-result").textContent).toContain("未送达");
    expect(screen.getByTestId("codex-recovery-retry")).toBeTruthy();
    expect(screen.queryByTestId("codex-recovery-check")).toBeNull(); // A 已终态
    // 重试 → B（新动作）→ 挂起 → 30 秒 → 面板（新 actionId）
    routes.verifyQueue = [
      {
        actionId: "b-full",
        stage: "dispatching",
        outcome: "unresolved",
        turnId: null,
        runLimitReleased: false,
        reasonCode: null,
        verifiedAt: 2,
      },
    ];
    fireEvent.click(screen.getByTestId("codex-recovery-retry"));
    await flushAsync();
    expect(hangingSends).toHaveLength(2);
    await act(async () => {
      await vi.advanceTimersByTimeAsync(30_000);
    });
    expect(screen.getByTestId("codex-recovery-panel")).toBeTruthy();
    const bShort = panelActionShort();
    expect(bShort).not.toBe(aShort);
    // F7：面板不得保留 A 的任何状态——核对入口在场、无 A 的未送达结论、无 A 的普通重试
    expect(screen.getByTestId("codex-recovery-check")).toBeTruthy();
    expect(screen.queryByTestId("codex-recovery-retry")).toBeNull();
    expect(screen.queryByTestId("codex-recovery-result")).toBeNull();
    // B 的核对靶向 B（不是 A）
    fireEvent.click(screen.getByTestId("codex-recovery-check"));
    await flushAsync();
    expect(verifyUrls[verifyUrls.length - 1]).toContain(bShort);
    expect(screen.getByTestId("codex-recovery-result").textContent).toContain("没有找到足够证据");
  });
});

// ============================================================
// 复审 R7（F6 前端半边）：前探（probeCardPresence）与分流路径补发送归属检查。
// 定序：A 前探挂起（approve-options 用**真实端点可返回的 plan_pending 形状**
// available=true，api.rs:2589 审批扫描不排除 App、rollout 尾部 proposed_plan 经
// plan_pending 在 :2921 返回——不需要违约 mock）→ A 30s 到期（面板接管）→
// B 开始发送（在途）→ A 前探迟到返回 plan_pending=approve → 断言：
// B 的 cardPresence / receipt / 分流不被 A 的迟到结果驱动。
// ============================================================
describe("CodexSendFlow：R7 前探迟到结果不驱动新发送", () => {
  it("r7_probe_owned_by_its_send：A 前探挂起→A 到期→B 在途→A 探测迟到 plan_pending——B 正常送达、拦截回执不落 B 的槽", async () => {
    vi.useFakeTimers();
    installFetch();
    routes.approveOptionsHang = true;
    routes.sendHang = true;
    render(<MessageComposer session={{ id: "sess-1" }} />);
    await flushAsync();
    const input = screen.getByTestId("composer-input") as HTMLTextAreaElement;
    // A：点击发送（挂载探针 + 发送复探都打 approve-options；第一发挂起探测器）
    await act(async () => {
      fireEvent.change(input, { target: { value: "A 的消息" } });
    });
    await flushAsync();
    fireEvent.click(screen.getByTestId("composer-send"));
    await flushAsync();
    expect(screen.getByTestId("send-receipt-delivering")).toBeTruthy();
    // A 的 30 秒预算到期（前探段消费同一窗）→ 面板接管
    await act(async () => {
      await vi.advanceTimersByTimeAsync(30_000);
    });
    expect(screen.getByTestId("codex-recovery-panel")).toBeTruthy();
    const aShort = panelActionShort();
    // B：新逻辑发送（正文不同 → 新动作）。B 的**自己的前探**正常返回不在场
    //（真实端点形态 available=false）→ B 正常走到 session-send 在途；
    // A 的**前探**仍在挂起（此刻才迟到返回 plan_pending approve）
    routes.approveOptionsHang = false;
    routes.approveOptionsSequence = [
      { available: false, options: [], verifiedWith: "t", currentVersion: null, drift: false },
    ];
    await act(async () => {
      fireEvent.change(input, { target: { value: "B 的消息" } });
    });
    await flushAsync();
    fireEvent.click(screen.getByTestId("composer-send"));
    await flushAsync();
    expect(hangingSends).toHaveLength(1); // B 的 session-send 在途（A 的从未发出）
    expect(screen.getByTestId("send-receipt-delivering")).toBeTruthy();
    // A 前探迟到返回（plan_pending approve——api.rs:2921 真实载荷）：不得驱动 B 的
    // 分流——B 不被拦截、不落拦截回执、cardPresence 不被 A 的迟到结果覆盖
    await act(async () => {
      releaseApproveOptions!(json({ available: true, planPending: true }));
    });
    await flushAsync();
    expect(screen.queryByTestId("send-receipt-blocked")).toBeNull();
    expect(screen.getByTestId("send-receipt-delivering")).toBeTruthy();
    // B 的 session-send 正常到达：accepted 回执上槽（不是 blocked）
    await act(async () => {
      hangingSends[0].resolve(
        json({ status: "desktop_ipc", receipt: ipcReceipt({ delivery: "accepted" }) })
      );
    });
    await flushAsync();
    expect(screen.getByTestId("send-receipt-codex-accepted")).toBeTruthy();
    expect(screen.queryByTestId("send-receipt-blocked")).toBeNull();
    // 面板在此前仍是 A 自己的动作（B 的 accepted 回执按单槽语义替换呈现）
    expect(aShort.length).toBe(8);
  });
});

// ============================================================
// 复审 R7 非阻塞附注（预算口径）：前探明确拦截 = 「本次尝试未发送」的事实展示。
// 复审原文：无新 B 时，前探明确拦截可以作为本次尝试尚未发送的事实展示；应准确
// 区分『本次尝试未发』与复用 actionId 的历史投递事实。钉两面：
// ① 拦截 chip 的文案/状态只陈述当前事实（终端等待审批 + 零投递尝试），不携带
//    「未送达/已送达」等投递结论措辞、不产生待确认面板（Unknown 未发生）；
// ② 预算口径续弧：同文重按（卡片上处理完审批的真实后续）沿同一 actionId 在途
//    → 30s 到期转面板 → 面板核对仍用该原 actionId（不换新 ID）。
// 现状已准确的契约钉（非行为红测——如实申报）。
// ============================================================
describe("CodexSendFlow：R7 前探拦截的预算口径（本次未发送 ≠ 投递事实）", () => {
  it("r7_probe_intercept_budget_arc：预算内拦截=零投递事实展示（无未送达措辞/无面板）→ 重按同文在途到期转面板 → 核对沿原 actionId", async () => {
    vi.useFakeTimers();
    installFetch();
    // 探测序列（真实端点形态）：挂载探不在场 → 点击①发送复探审批在场（拦截）
    // → 点击②复探不在场（审批已在卡片上处理——重按放行的真实前置）
    routes.approveOptionsSequence = [
      { available: false, options: [], verifiedWith: "t", currentVersion: null, drift: false },
      {
        available: true,
        options: [{ label: "Allow once" }],
        verifiedWith: "t",
        currentVersion: null,
        drift: false,
      },
      { available: false, options: [], verifiedWith: "t", currentVersion: null, drift: false },
    ];
    routes.sendHang = true;
    render(<MessageComposer session={{ id: "sess-1" }} />);
    await flushAsync();
    // ===== 点击①：前探在 30s 预算内返回明确拦截 =====
    const input = screen.getByTestId("composer-input") as HTMLTextAreaElement;
    await act(async () => {
      fireEvent.change(input, { target: { value: "看看这次改动" } });
    });
    await flushAsync();
    fireEvent.click(screen.getByTestId("composer-send"));
    await flushAsync();
    // 事实展示：拦截 chip = 中性指引（终端等待审批），措辞不含任何投递结论
    const blocked = screen.getByTestId("send-receipt-blocked");
    expect(blocked.textContent).toBe(i18n.t("mobile.composer.approvePresent"));
    expect(blocked.textContent).not.toContain("未送达");
    expect(blocked.textContent).not.toContain("已送达");
    expect(blocked.textContent).not.toContain("已发送");
    // 零投递：本次尝试没有发出任何 session-send 请求（精确等值匹配——
    // /session-send-info 前缀包含 /session-send，不得误计）
    const sendCalls = () =>
      fetchMock.mock.calls.filter(([u]) => String(u) === "/m/api/v1/session-send").length;
    expect(sendCalls()).toBe(0);
    // 拦截是明确事实：无待确认面板（Unknown 未发生）、草稿保留（未发送的直接呈现）
    expect(screen.queryByTestId("codex-recovery-panel")).toBeNull();
    expect(input.value).toBe("看看这次改动");
    // ===== 点击②：同文重按（审批已处理）→ 前探放行 → session-send 在途 =====
    await act(async () => {
      fireEvent.click(screen.getByTestId("composer-send"));
    });
    await flushAsync();
    expect(sendCalls()).toBe(1);
    expect(screen.getByTestId("send-receipt-delivering")).toBeTruthy();
    // ===== 30s 前台预算到期 → 面板接管（转圈结束）=====
    await act(async () => {
      await vi.advanceTimersByTimeAsync(30_000);
    });
    expect(screen.queryByTestId("send-receipt-delivering")).toBeNull();
    expect(screen.getByTestId("codex-recovery-panel")).toBeTruthy();
    const anchorShort = panelActionShort();
    expect(anchorShort).toHaveLength(8);
    // ===== 面板核对仍可用原 actionId（与面板锚同一 ID）=====
    routes.verifyQueue = [
      {
        actionId: "act-1",
        stage: "dispatching",
        outcome: "unresolved",
        turnId: null,
        runLimitReleased: false,
        reasonCode: null,
        verifiedAt: 1,
      },
    ];
    fireEvent.click(screen.getByTestId("codex-recovery-check"));
    await flushAsync();
    expect(verifyUrls).toHaveLength(1);
    expect(verifyUrls[0]).toContain(`action_id=${anchorShort}`);
    // unresolved → 继续待确认（「没找到」不等于「没发送」——面板不给出投递结论）
    expect(screen.getByTestId("codex-recovery-panel")).toBeTruthy();
  });
});

// ============================================================
// 复审 round 5 · W2（P2）：人工继续的后继是**新动作**，resumed 响应的 `send`
// 字段（真实回执 / 排队位次）必须按五分类呈现——不得凭「后继已创建」宣称已发。
// 核对/重试沿**后继 ID**；already_resumed 不宣称投递成功，只「待查」+ 核对沿后继。
// 场景基线：原动作 Unknown → 面板 → 核对 unresolved → 三件风险 → 确认 →
// 后继创建（resumed）→ 后继走正常发送路径得到真实结果。
// ============================================================
describe("CodexSendFlow：人工继续后继的真实发送结果（W2）", () => {
  /** 走到「三件风险已展开、等待确认」的现场；返回原动作短 ID。 */
  async function armManualResume(text: string): Promise<string> {
    vi.useFakeTimers();
    installFetch();
    routes.sendHang = true;
    render(<MessageComposer session={{ id: "sess-1" }} />);
    await flushAsync();
    const input = screen.getByTestId("composer-input") as HTMLTextAreaElement;
    await act(async () => {
      fireEvent.change(input, { target: { value: text } });
    });
    await flushAsync();
    fireEvent.click(screen.getByTestId("composer-send"));
    await flushAsync();
    await act(async () => {
      await vi.advanceTimersByTimeAsync(30_000);
    });
    routes.verifyQueue = [
      {
        actionId: "act-1",
        stage: "dispatching",
        outcome: "unresolved",
        turnId: null,
        runLimitReleased: false,
        reasonCode: null,
        verifiedAt: 1,
      },
    ];
    fireEvent.click(screen.getByTestId("codex-recovery-check"));
    await flushAsync();
    const originalShort = panelActionShort();
    fireEvent.click(screen.getByTestId("codex-recovery-resend"));
    await flushAsync();
    expect(resumeBodies).toHaveLength(0); // 未确认零请求
    return originalShort;
  }

  it("w2_resume_notsent_native_pending：后继被 pending 门拒绝（真实 NotSent 回执）→ 显示未送达不说已发；重试沿后继 ID 建新动作", async () => {
    const originalShort = await armManualResume("重启服务");
    routes.resume = {
      status: "resumed",
      predecessor: {},
      successorActionId: "act-succ-9",
      send: {
        receipt: ipcReceipt({
          actionId: "act-succ-9",
          delivery: "not_sent",
          reasonCode: "native_pending",
        }),
      },
    };
    fireEvent.click(screen.getByTestId("codex-recovery-resend-confirm"));
    await flushAsync();
    expect(resumeBodies).toHaveLength(1);
    const body = JSON.parse(resumeBodies[0]) as Record<string, string>;
    expect(body.newActionId).not.toBe(body.actionId);
    // 真实拒绝回执 → 明确未送达五分类，**绝不**显示「已重新发送」
    expect(screen.queryByText(/已重新发送/)).toBeNull();
    expect(screen.queryByTestId("codex-recovery-result")).toBeNull(); // 面板让位
    const chip = screen.getByTestId("send-receipt-codex-failed");
    expect(chip.textContent).toContain("桌面端有未处理的请求"); // native_pending 专属理由文案
    expect(screen.getByTestId("codex-retry-notsent")).toBeTruthy();
    // 重试沿**后继 ID**：新动作 ≠ 后继 ≠ 原动作；正文 = 原文；关联展示指向后继
    routes.sendHang = false;
    routes.sendQueue = [
      { status: "desktop_ipc", receipt: ipcReceipt({ actionId: "act-3", delivery: "accepted" }) },
    ];
    fireEvent.click(screen.getByTestId("codex-retry-notsent"));
    await flushAsync();
    expect(sendBodies).toHaveLength(1);
    const retry = JSON.parse(sendBodies[0]) as Record<string, string>;
    expect(retry.actionId.startsWith("act-succ")).toBe(false);
    expect(retry.actionId.startsWith(originalShort)).toBe(false);
    expect(retry.text).toBe("重启服务");
    expect(screen.getByTestId("codex-retried-from").textContent).toContain("act-succ");
  });

  it("w2_resume_queued：后继忙态留队 → 显示已排队位次不说已发；父子关联在场", async () => {
    const originalShort = await armManualResume("排队试试");
    routes.resume = {
      status: "resumed",
      predecessor: {},
      successorActionId: "act-succ-q",
      send: { queued: true, position: 3 },
    };
    fireEvent.click(screen.getByTestId("codex-recovery-resend-confirm"));
    await flushAsync();
    expect(resumeBodies).toHaveLength(1);
    expect(screen.queryByText(/已重新发送/)).toBeNull();
    const chip = screen.getByTestId("send-receipt-codex-queued");
    expect(chip.textContent).toContain("第3位");
    // 父子关系保留展示：原动作 → 后继（安全引用）
    expect(screen.getByTestId("codex-queued-resumed-from").textContent).toContain(originalShort);
  });

  it("w2_resume_unknown：后继结果未知 → 面板按后继 ID 重挂（F7 隔离 + 父子关联），只核对沿后继、绝不自动重发", async () => {
    const originalShort = await armManualResume("未知后继");
    routes.resume = {
      status: "resumed",
      predecessor: {},
      successorActionId: "act-succ-u",
      send: {
        receipt: ipcReceipt({
          actionId: "act-succ-u",
          delivery: "unknown_delivery",
          reasonCode: "terminal_busy",
        }),
      },
    };
    fireEvent.click(screen.getByTestId("codex-recovery-resend-confirm"));
    await flushAsync();
    expect(resumeBodies).toHaveLength(1);
    expect(screen.queryByText(/已重新发送/)).toBeNull();
    // 面板按**后继 ID** 重挂（不是沿用原动作的面板）
    expect(screen.getByTestId("codex-recovery-panel")).toBeTruthy();
    expect(panelActionShort()).toBe("act-succ");
    expect(screen.getByTestId("codex-recovery-resumed-from").textContent).toContain(originalShort);
    // Unknown 后继只核对：未核对前无任何重发/重试入口
    expect(screen.queryByTestId("codex-recovery-resend")).toBeNull();
    expect(screen.queryByTestId("codex-recovery-retry")).toBeNull();
    // 核对沿后继 ID（不拿原动作的核对冒充后继结果）
    routes.verifyQueue = [
      {
        actionId: "act-succ-u",
        stage: "dispatching",
        outcome: "unresolved",
        turnId: null,
        runLimitReleased: false,
        reasonCode: null,
        verifiedAt: 2,
      },
    ];
    fireEvent.click(screen.getByTestId("codex-recovery-check"));
    await flushAsync();
    expect(verifyUrls[verifyUrls.length - 1]).toContain("act-succ-u");
  });

  it("w2_already_resumed：幂等回执不宣称投递成功 → 「此前已创建后继…待查」；核对沿后继 ID；后继 unresolved 不再提供重发", async () => {
    await armManualResume("再次恢复");
    routes.resume = { status: "already_resumed", predecessor: {}, successorActionId: "act-succ-p" };
    fireEvent.click(screen.getByTestId("codex-recovery-resend-confirm"));
    await flushAsync();
    expect(resumeBodies).toHaveLength(1);
    const result = screen.getByTestId("codex-recovery-result").textContent ?? "";
    expect(result).toContain("此前已创建后继");
    expect(result).toContain("act-succ");
    expect(result).not.toContain("已重新发送");
    // 核对沿后继 ID
    routes.verifyQueue = [
      {
        actionId: "act-succ-p",
        stage: "dispatching",
        outcome: "unresolved",
        turnId: null,
        runLimitReleased: false,
        reasonCode: null,
        verifiedAt: 2,
      },
    ];
    fireEvent.click(screen.getByTestId("codex-recovery-check"));
    await flushAsync();
    expect(verifyUrls[verifyUrls.length - 1]).toContain("act-succ-p");
    // 后继 unresolved → 原动作最多一个后继，不再提供重发（再点只会幂等回弹）；核对入口仍在
    expect(screen.queryByTestId("codex-recovery-resend")).toBeNull();
    expect(screen.getByTestId("codex-recovery-check")).toBeTruthy();
  });
});

// ============================================================
// 复审 round 6 · Q2（P2）：迟到的人工继续响应不得覆盖新现场。
// `onSuccessorResolved` 是**父组件回调**——不受面板按 actionId 重挂的局部状态
// 保护：旧面板卸载后，仍在途的人工继续 promise 依然会调用它。定序基线（复审
// 原文）：A Unknown → 核对 unresolved → 确认人工继续 A（响应挂起）→
// ① 用户修改正文发送 B、B 的 Unknown 面板已挂载 → A 的后继 NotSent/Queued/
//    Unknown 响应迟到 → B 的回执、面板与核对锚不得被 A 覆盖；
// ② 无 B 发送、在途编辑新正文/附件 → A 迟到 Queued → 新草稿不得被清
//（只有实际提交的草稿版本可以被成功/入队清理）。
// ============================================================
describe("CodexSendFlow：Q2 迟到人工继续响应的归属守卫", () => {
  /** 走到「人工继续请求已发出、响应仍挂起」的现场；返回原动作短 ID。 */
  async function armResumePending(
    text: string,
    resumePayload: Record<string, unknown>
  ): Promise<string> {
    vi.useFakeTimers();
    installFetch();
    routes.sendHang = true;
    render(<MessageComposer session={{ id: "sess-1" }} />);
    await flushAsync();
    const input = screen.getByTestId("composer-input") as HTMLTextAreaElement;
    await act(async () => {
      fireEvent.change(input, { target: { value: text } });
    });
    await flushAsync();
    fireEvent.click(screen.getByTestId("composer-send"));
    await flushAsync();
    await act(async () => {
      await vi.advanceTimersByTimeAsync(30_000);
    });
    routes.verifyQueue = [
      {
        actionId: "act-1",
        stage: "dispatching",
        outcome: "unresolved",
        turnId: null,
        runLimitReleased: false,
        reasonCode: null,
        verifiedAt: 1,
      },
    ];
    fireEvent.click(screen.getByTestId("codex-recovery-check"));
    await flushAsync();
    const originalShort = panelActionShort();
    fireEvent.click(screen.getByTestId("codex-recovery-resend"));
    await flushAsync();
    routes.resume = resumePayload;
    routes.resumeHang = true;
    fireEvent.click(screen.getByTestId("codex-recovery-resend-confirm"));
    await flushAsync();
    expect(resumeBodies).toHaveLength(1); // 请求已发出、响应仍挂起
    return originalShort;
  }

  /** B：修改正文后发送（真实 desktop_ipc unknown_delivery 回执）→ B 的 Unknown 面板挂载。
   *  全程同步查询 + 微任务冲刷（findBy* 的 waitFor 在假定时器下会挂起，§7.5 同款）。 */
  async function sendBToUnknownPanel() {
    routes.sendHang = false;
    routes.sendQueue = [
      {
        status: "desktop_ipc",
        receipt: ipcReceipt({
          actionId: "act-b",
          delivery: "unknown_delivery",
          reasonCode: "terminal_busy",
        }),
      },
    ];
    const input = screen.getByTestId("composer-input") as HTMLTextAreaElement;
    await act(async () => {
      fireEvent.change(input, { target: { value: "B 的新正文" } });
    });
    await flushAsync();
    fireEvent.click(screen.getByTestId("composer-send"));
    await flushAsync();
    expect(screen.getByTestId("codex-recovery-panel")).toBeTruthy();
    expect(panelActionShort()).toBe("act-b");
  }

  it("q2_late_resume_notsent_cannot_override_b_panel：A 人工继续挂起 → B 已发送且 Unknown 面板挂载 → A 迟到 NotSent——B 回执/面板/核对锚不被覆盖", async () => {
    await armResumePending("A 的原文", {
      status: "resumed",
      predecessor: {},
      successorActionId: "act-succ-n",
      send: {
        receipt: ipcReceipt({
          actionId: "act-succ-n",
          delivery: "not_sent",
          reasonCode: "native_pending",
        }),
      },
    });
    await sendBToUnknownPanel();
    releaseResumeWith();
    await flushAsync();
    // B 的待确认面板不被 A 后继的 NotSent chip 覆盖（面板仍锚定 B）
    expect(screen.getByTestId("codex-recovery-panel")).toBeTruthy();
    expect(panelActionShort()).toBe("act-b");
    expect(screen.queryByTestId("send-receipt-codex-failed")).toBeNull();
    // 核对锚仍沿 B：核对请求靶向 B 的动作 ID
    routes.verifyQueue = [
      {
        actionId: "act-b",
        stage: "dispatching",
        outcome: "unresolved",
        turnId: null,
        runLimitReleased: false,
        reasonCode: null,
        verifiedAt: 2,
      },
    ];
    fireEvent.click(screen.getByTestId("codex-recovery-check"));
    await flushAsync();
    expect(verifyUrls[verifyUrls.length - 1]).toContain("act-b");
  });

  it("q2_late_resume_queued_cannot_override_b_panel：同定序 → A 迟到 Queued——B 面板不被覆盖、B 的在途草稿不被代清", async () => {
    await armResumePending("A 的原文", {
      status: "resumed",
      predecessor: {},
      successorActionId: "act-succ-q",
      send: { queued: true, position: 3 },
    });
    await sendBToUnknownPanel();
    const before = (screen.getByTestId("composer-input") as HTMLTextAreaElement).value;
    releaseResumeWith();
    await flushAsync();
    expect(screen.getByTestId("codex-recovery-panel")).toBeTruthy();
    expect(panelActionShort()).toBe("act-b");
    expect(screen.queryByTestId("send-receipt-codex-queued")).toBeNull();
    // 入队臂的无条件清空不得碰 B 的在途草稿
    expect((screen.getByTestId("composer-input") as HTMLTextAreaElement).value).toBe(before);
  });

  it("q2_late_resume_unknown_cannot_override_b_panel：同定序 → A 迟到 Unknown——面板不串成 A 后继（父子关联行也不得冒现）", async () => {
    await armResumePending("A 的原文", {
      status: "resumed",
      predecessor: {},
      successorActionId: "act-succ-u",
      send: {
        receipt: ipcReceipt({
          actionId: "act-succ-u",
          delivery: "unknown_delivery",
          reasonCode: "terminal_busy",
        }),
      },
    });
    await sendBToUnknownPanel();
    releaseResumeWith();
    await flushAsync();
    // 面板仍锚定 B 的动作 ID（不是 A 的后继）
    expect(screen.getByTestId("codex-recovery-panel")).toBeTruthy();
    expect(panelActionShort()).toBe("act-b");
    // A 后继的父子关联行不得冒现在 B 的面板上
    expect(screen.queryByTestId("codex-recovery-resumed-from")).toBeNull();
  });

  it("q2_late_resume_queued_cannot_clear_new_draft：无 B 发送、人工继续在途编辑新正文+附件 → A 迟到 Queued——新草稿与附件不被清，后继排队如实呈现", async () => {
    // uploadAttachment 走 XHR（fetch 规范无上传进度事件）——最小假体：上行立即成功
    class FakeAttachXHR {
      upload: { onprogress: ((e: ProgressEvent) => void) | null } = { onprogress: null };
      onload: ((e: ProgressEvent) => void) | null = null;
      onerror: (() => void) | null = null;
      onabort: (() => void) | null = null;
      status = 0;
      responseText = "";
      open(_method: string, _url: string) {}
      setRequestHeader(_name: string, _value: string) {}
      send() {
        this.respond(200, JSON.stringify({ path: "/tmp/att-new.bin", size: 1 }));
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
    vi.stubGlobal("XMLHttpRequest", FakeAttachXHR as unknown as typeof XMLHttpRequest);
    const originalShort = await armResumePending("排队试试", {
      status: "resumed",
      predecessor: {},
      successorActionId: "act-succ-q",
      send: { queued: true, position: 3 },
    });
    // 在途编辑新正文 + 新附件（后继实际提交的是原文，与这份新草稿无关）
    const input = screen.getByTestId("composer-input") as HTMLTextAreaElement;
    await act(async () => {
      fireEvent.change(input, { target: { value: "全新的草稿" } });
    });
    await flushAsync();
    fireEvent.change(screen.getByTestId("attach-file-input"), {
      target: { files: [new File([new Uint8Array([1])], "new-attach.bin")] },
    });
    await flushAsync();
    expect(screen.getByTestId(/^attach-chip-/)).toBeTruthy();
    releaseResumeWith();
    await flushAsync();
    // 新草稿与附件都不被迟到入队代清
    expect((screen.getByTestId("composer-input") as HTMLTextAreaElement).value).toBe("全新的草稿");
    expect(screen.getByTestId(/^attach-chip-/)).toBeTruthy();
    // 后继真实排队位次仍如实呈现（回执真值更新与草稿保留并行；父子关联在场）
    expect(screen.getByTestId("send-receipt-codex-queued").textContent).toContain("第3位");
    expect(screen.getByTestId("codex-queued-resumed-from").textContent).toContain(originalShort);
  });
});

// ============================================================
// 复审 round 7 · Q2（P2）：draftVersion 必须在**提交时点**冻结，而不是在回执/
// 计时器落地时采样「响应时的当前值」。textarea 只按 injectable 禁用——发送等待
// 期间仍可编辑，于是：
// ① 初始等待期编辑出 B → A 以 unknown_delivery 落地 → owner 被采样成当前（B 的）
//    版本 → 人工继续 Queued 时版本门误判「草稿未变」→ 清掉未提交的 B；
// ② 多层 Unknown：后继 Unknown 的重登记又把 owner 版本重置为当前值，抹掉原提交
//    版本与新稿的区别 → 再人工继续 Queued 同样误清 B。
// 修复口径：owner 登记一律携带**本次实际提交时点**的草稿版本；同文人工继续的
// 后继**继承**原提交版本（后继重发的就是原消息，ManualResumeBody.text 契约）。
// 清稿只清实际提交的那份——正文与附件同门。
// ============================================================
describe("CodexSendFlow：Q2 round 7 提交时点冻结草稿版本（等待期编辑不误清）", () => {
  /** 通用 unresolved 核对载荷（verifyQueue 按次弹出，逐次装填） */
  function unresolvedVerify(): Record<string, unknown> {
    return {
      actionId: "act-x",
      stage: "dispatching",
      outcome: "unresolved",
      turnId: null,
      runLimitReleased: false,
      reasonCode: null,
      verifiedAt: 1,
    };
  }

  /** 场景基线①：A 提交后 session-send **挂起**（响应未落地），返回输入框元素。
   *  真实时钟即可：不依赖 30 秒截止（unknown 由迟到响应臂/后续编排落地）。 */
  async function armSendPending(text: string): Promise<HTMLTextAreaElement> {
    installFetch();
    routes.sendHang = true;
    render(<MessageComposer session={{ id: "sess-1" }} />);
    await flushAsync();
    const input = screen.getByTestId("composer-input") as HTMLTextAreaElement;
    await act(async () => {
      fireEvent.change(input, { target: { value: text } });
    });
    await flushAsync();
    fireEvent.click(screen.getByTestId("composer-send"));
    await flushAsync();
    expect(hangingSends).toHaveLength(1);
    return input;
  }

  /** 场景基线②：发送即以 unknown_delivery 落地（无等待期，owner 版本在两种实现下
   *  都正确）→ 待确认面板挂载。返回输入框元素。 */
  async function armImmediateUnknown(text: string): Promise<HTMLTextAreaElement> {
    installFetch();
    routes.sendQueue = [
      {
        status: "desktop_ipc",
        receipt: ipcReceipt({
          actionId: "act-a",
          delivery: "unknown_delivery",
          reasonCode: "terminal_busy",
        }),
      },
    ];
    render(<MessageComposer session={{ id: "sess-1" }} />);
    await flushAsync();
    const input = screen.getByTestId("composer-input") as HTMLTextAreaElement;
    await act(async () => {
      fireEvent.change(input, { target: { value: text } });
    });
    await flushAsync();
    fireEvent.click(screen.getByTestId("composer-send"));
    await flushAsync();
    expect(screen.getByTestId("codex-recovery-panel")).toBeTruthy();
    expect(panelActionShort()).toBe("act-a");
    return input;
  }

  /** 在当前待确认面板上走完人工继续：核对 unresolved → 三件风险 → 确认。
   *  resume 载荷由调用方预先装填 routes.resume。 */
  async function resumeViaPanel(): Promise<void> {
    routes.verifyQueue = [unresolvedVerify()];
    fireEvent.click(screen.getByTestId("codex-recovery-check"));
    await flushAsync();
    const sentBefore = resumeBodies.length;
    fireEvent.click(screen.getByTestId("codex-recovery-resend"));
    await flushAsync();
    expect(resumeBodies).toHaveLength(sentBefore); // 未确认零请求
    fireEvent.click(screen.getByTestId("codex-recovery-resend-confirm"));
    await flushAsync();
  }

  it("q2_initial_wait_edit_late_queued_keeps_new_draft：初始等待期改出 B → A 落 unknown_delivery → 人工继续 Queued——未提交的 B 不被清、排队如实呈现", async () => {
    const input = await armSendPending("A 的原文");
    // 等待期把输入框改成新稿 B（真实 textarea onChange → 草稿版本前移）
    await act(async () => {
      fireEvent.change(input, { target: { value: "B 的新稿" } });
    });
    await flushAsync();
    // A 以 unknown_delivery 落地（响应臂登记恢复归属）→ 待确认面板
    await act(async () => {
      releaseSend!(
        json({
          status: "desktop_ipc",
          receipt: ipcReceipt({
            actionId: "act-a",
            delivery: "unknown_delivery",
            reasonCode: "terminal_busy",
          }),
        })
      );
    });
    await flushAsync();
    expect(screen.getByTestId("codex-recovery-panel")).toBeTruthy();
    expect(panelActionShort()).toBe("act-a");
    // 人工继续 A：后继实际提交的是 A 的原文（B 与这次提交无关）
    routes.resume = {
      status: "resumed",
      predecessor: {},
      successorActionId: "act-succ-q",
      send: { queued: true, position: 3 },
    };
    await resumeViaPanel();
    // 未提交的新稿 B 不被代清；后继排队位次如实呈现
    expect((screen.getByTestId("composer-input") as HTMLTextAreaElement).value).toBe("B 的新稿");
    expect(screen.getByTestId("send-receipt-codex-queued").textContent).toContain("第3位");
  });

  it("q2_multilayer_unknown_owner_version_inherited：A unknown → 编辑 B → 人工继续落后继 unknown → 再人工继续 Queued——B 仍不被清（后继继承原提交版本）", async () => {
    const input = await armImmediateUnknown("A 的原文");
    // 等待人工继续期间编辑出 B（版本前移；此刻 A 的 owner 版本仍正确）
    await act(async () => {
      fireEvent.change(input, { target: { value: "B 的新稿" } });
    });
    await flushAsync();
    // 第一层人工继续：后继 unknown → 面板切后继（归属快照重登记——不得重置为当前版本）
    routes.resume = {
      status: "resumed",
      predecessor: {},
      successorActionId: "act-succ-u",
      send: {
        receipt: ipcReceipt({
          actionId: "act-succ-u",
          delivery: "unknown_delivery",
          reasonCode: "terminal_busy",
        }),
      },
    };
    await resumeViaPanel();
    expect(screen.getByTestId("codex-recovery-panel")).toBeTruthy();
    expect(panelActionShort()).toBe("act-succ");
    // 第二层人工继续（沿后继，同文重发）：Queued → B 仍不得被清
    routes.resume = {
      status: "resumed",
      predecessor: {},
      successorActionId: "act-succ-2",
      send: { queued: true, position: 2 },
    };
    await resumeViaPanel();
    expect((screen.getByTestId("composer-input") as HTMLTextAreaElement).value).toBe("B 的新稿");
    expect(screen.getByTestId("send-receipt-codex-queued").textContent).toContain("第2位");
  });

  it("q2_attachment_add_during_wait_not_cleared：初始等待期增补附件（正文不动）→ A 落 unknown → 人工继续 Queued——正文与附件都不被清", async () => {
    // uploadAttachment 走 XHR（fetch 规范无上传进度事件）——最小假体：上行立即成功
    class FakeAttachXHR {
      upload: { onprogress: ((e: ProgressEvent) => void) | null } = { onprogress: null };
      onload: ((e: ProgressEvent) => void) | null = null;
      onerror: (() => void) | null = null;
      onabort: (() => void) | null = null;
      status = 0;
      responseText = "";
      open(_method: string, _url: string) {}
      setRequestHeader(_name: string, _value: string) {}
      send() {
        this.respond(200, JSON.stringify({ path: "/tmp/wait-add.bin", size: 1 }));
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
    vi.stubGlobal("XMLHttpRequest", FakeAttachXHR as unknown as typeof XMLHttpRequest);
    await armSendPending("A 的原文");
    // 等待期只增补附件（addFiles → 版本前移；正文保持提交原样）
    fireEvent.change(screen.getByTestId("attach-file-input"), {
      target: { files: [new File([new Uint8Array([1])], "wait-add.bin")] },
    });
    await flushAsync();
    expect(screen.getByTestId(/^attach-chip-/)).toBeTruthy();
    // A 以 unknown_delivery 落地 → 待确认面板
    await act(async () => {
      releaseSend!(
        json({
          status: "desktop_ipc",
          receipt: ipcReceipt({
            actionId: "act-a",
            delivery: "unknown_delivery",
            reasonCode: "terminal_busy",
          }),
        })
      );
    });
    await flushAsync();
    expect(screen.getByTestId("codex-recovery-panel")).toBeTruthy();
    routes.resume = {
      status: "resumed",
      predecessor: {},
      successorActionId: "act-succ-q",
      send: { queued: true, position: 1 },
    };
    await resumeViaPanel();
    // 正文（实际提交的那份）与等待期增补的附件都不被代清
    expect((screen.getByTestId("composer-input") as HTMLTextAreaElement).value).toBe("A 的原文");
    expect(screen.getByTestId(/^attach-chip-/)).toBeTruthy();
    expect(screen.getByTestId("send-receipt-codex-queued").textContent).toContain("第1位");
  });

  it("q2_positive_control_no_edit_queued_clears_submitted_draft：正向对照——提交后未编辑，人工继续 Queued 后实际提交的草稿应当被清", async () => {
    await armImmediateUnknown("A 的原文");
    // 全程零编辑：版本门应当放行清空（防过度保护）
    routes.resume = {
      status: "resumed",
      predecessor: {},
      successorActionId: "act-succ-q",
      send: { queued: true, position: 2 },
    };
    await resumeViaPanel();
    expect((screen.getByTestId("composer-input") as HTMLTextAreaElement).value).toBe("");
    expect(screen.getByTestId("send-receipt-codex-queued").textContent).toContain("第2位");
  });
});
