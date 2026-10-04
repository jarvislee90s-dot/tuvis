import { act, cleanup, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import MessageComposer from "@/mobile/MessageComposer";
import type { QueueItemView, SendInfo } from "@/mobile/api";

// M7 Task 7：移动端发送 UI（W4）。fetch 全量 stub（盖过 setup.ts 的 msw），
// 按 URL 分路到 send-info / session-send / queue 三族端点（注意 URL 前缀包含
// 关系：/session-send-info ⊃ /session-send、/session-queue/jump|retract ⊃
// /session-queue，长路径必须先判）。组件挂载即拉 send-info，用例先 findBy
// 输入框就绪再交互。

/** 可注入态夹具（后端 SendInfo camelCase 契约） */
function sendInfo(overrides: Partial<SendInfo> = {}): SendInfo {
  return { injectable: true, channels: ["tmux"], visibility: "realtime", ...overrides };
}

/** 排队条目夹具（GET /session-queue 的 items 元素） */
function queueItem(overrides: Partial<QueueItemView> = {}): QueueItemView {
  // 丁T3 裁2：服务端 compose 形态为 `{正文} [mobile 设备名]`——夹具用**真机形态**
  return { id: 7, content: "你好 [mobile 测试机]", enqueuedAt: 1000, position: 1, ...overrides };
}

interface Routes {
  info?: SendInfo;
  infoStatus?: number;
  /** 丁T3 接入②：审批卡在场探针（/session-approve-options）载荷；缺省 available=false
   *  （不在场）——只有显式给 true 的用例才走分流。 */
  approveOptions?: { available: boolean; planPending?: boolean };
  /** 探针端点非 2xx（探针失败路径：api.ts 抛 ApiError → composer 按不在场处理） */
  approveOptionsStatus?: number;
  questionStatus?: number;
  /** 丁T3 接入②：问答卡在场探针（/session-question）载荷；缺省 available=false。
   *  丁T6 复评起载荷可带 **answerable / freeText / questions**——composer 据此判
   *  「能否转向自由作答」（四态分流的数据源；判据与后端同源，见
   *  MessageComposer 的 `probeCardPresence`）。 */
  questionInfo?: {
    available: boolean;
    answerable?: boolean;
    freeText?: boolean;
    questions?: Array<{ question: string; multiSelect: boolean }>;
  };
  /** POST /session-question/answer 回执（丁T6：composer 转向 freeText 的路径；
   *  缺省 key_sent+verified:true）*/
  answer?: Record<string, unknown>;
  /** 问答应答非 2xx（409/400；错误码在**响应体 `error` 字段**——与 /session-send 的
   *  `reason` 字段**不同**，丁T6 复评核出并加锁） */
  answerStatus?: number;
  answerBody?: Record<string, unknown>;
  send?: Record<string, unknown>;
  sendStatus?: number;
  /** 非 2xx 时响应体 JSON（403 not_injectable{reason,reasonCode} 等，P2-10） */
  sendBody?: Record<string, unknown>;
  /** 发送请求挂起不响应（灰3：投递中 chip 的慢消费者场景） */
  sendHang?: boolean;
  queue?: QueueItemView[];
  jump?: Record<string, unknown>;
  retractOk?: boolean;
  /** 撤回网络层异常（fetch 直接 reject，P2-7 对账测试用） */
  retractReject?: boolean;
  /** 撤回忙时拒收：200 {status:"failed",error}（P2-6 忙时回执，条目仍在队） */
  retractBusy?: boolean;
  /** 队列列表 /session-queue 拉取网络层异常（复核失败场景） */
  queueReject?: boolean;
  /** 队列列表挂起不响应（T4 复评 I1/I2 时序窗：在途 tick 快照由 releaseQueue 手动放行） */
  queueHang?: boolean;
  /** 附件上传（2026-09-20）：成功载荷 / 413·404 等非 2xx 状态 */
  attach?: { path: string; size: number };
  attachStatus?: number;
  attachError?: string;
  /** 附件上传挂起不响应（上传中禁发测试） */
  attachHang?: boolean;
  /** 上传进度事件（Task 10 §C5）：FakeXHR 在 send 时先发一发 onprogress（loaded,total） */
  attachProgress?: [number, number];
  /** 通道能力装饰载荷（GET /m/api/v1/channel，§C5 带宽提示）：缺省 lan 直连不限速
   *  = 不出横幅，既有用例不受影响 */
  channel?: { via: string; limited: boolean; est_mbps_down: number; est_mbps_up: number };
  /** 原始响应体（畸形载荷用：JSON.stringify(Infinity) 会变 null，测不到 1e999 解析） */
  channelRaw?: string;
}

let routes: Routes;
let fetchMock: ReturnType<typeof vi.fn>;
/** sendHang 挂起请求的放行器（测试中手动 resolve 模拟响应到达） */
let releaseSend: ((r: Response) => void) | null = null;
/** attachHang 挂起请求的放行器（上传中禁发测试用；XHR 后改为对挂起 FakeXHR respond） */
let releaseAttach: ((r: Response) => void) | null = null;
/** queueHang 挂起请求的放行器（在途 tick 快照时序窗测试用） */
let releaseQueue: ((r: Response) => void) | null = null;

beforeEach(() => {
  routes = {};
  releaseSend = null;
  releaseAttach = null;
  releaseQueue = null;
});
afterEach(() => {
  cleanup();
  vi.useRealTimers();
  vi.unstubAllGlobals();
});

/** 按 URL 分路的 fetch stub（判序：长路径在前，避免前缀误吞） */
function installFetch() {
  fetchMock = vi.fn(async (input: RequestInfo | URL) => {
    const url = String(input);
    // §C5 带宽提示（装饰）：缺省 = 局域网直连不限速（不出横幅）；403/网络异常
    // 由 fetchChannel 内部静默降级为 null
    if (url.includes("/m/api/v1/channel")) {
      if (routes.channelRaw !== undefined) return new Response(routes.channelRaw, { status: 200 });
      return new Response(
        JSON.stringify(
          routes.channel ?? { via: "lan", limited: false, est_mbps_down: 0, est_mbps_up: 0 }
        ),
        { status: 200 }
      );
    }
    if (url.includes("/session-send-info")) {
      if (routes.infoStatus) return new Response("no", { status: routes.infoStatus });
      return new Response(JSON.stringify(routes.info ?? sendInfo()), { status: 200 });
    }
    // 丁T3 接入②：卡片在场探针（两条 GET，判序在 /session-send 之前——长路径优先）
    if (url.includes("/session-approve-options")) {
      if (routes.approveOptionsStatus) {
        return new Response("no", { status: routes.approveOptionsStatus });
      }
      return new Response(
        JSON.stringify(
          routes.approveOptions ?? {
            available: false,
            options: [],
            verifiedWith: "t",
            currentVersion: null,
            drift: false,
          }
        ),
        { status: 200 }
      );
    }
    // 丁T6 复评：问答应答端点（composer 转向 freeText 的出口）。**判序必须在
    // /session-question 之前**——`/session-question/answer` ⊃ `/session-question`
    // （与前缀包含关系同一惯例：长路径先判）
    if (url.includes("/session-question/answer")) {
      if (routes.answerStatus) {
        return new Response(JSON.stringify(routes.answerBody ?? { error: "internal" }), {
          status: routes.answerStatus,
        });
      }
      return new Response(
        JSON.stringify(routes.answer ?? { status: "key_sent", done: true, verified: true }),
        { status: 200 }
      );
    }
    if (url.includes("/session-question")) {
      if (routes.questionStatus) return new Response("no", { status: routes.questionStatus });
      return new Response(
        JSON.stringify(routes.questionInfo ?? { available: false, questions: [] }),
        { status: 200 }
      );
    }
    if (url.includes("/session-queue/jump")) {
      return new Response(JSON.stringify(routes.jump ?? { status: "delivered" }), { status: 200 });
    }
    if (url.includes("/session-queue/retract")) {
      if (routes.retractReject) throw new TypeError("retract 网络断开（模拟网络层异常）");
      if (routes.retractBusy) {
        return new Response(JSON.stringify({ status: "failed", error: "投递进行中，请稍后重试" }), {
          status: 200,
        });
      }
      return new Response(JSON.stringify({ ok: routes.retractOk ?? true }), { status: 200 });
    }
    if (url.includes("/session-send")) {
      if (routes.sendHang) {
        return new Promise<Response>((resolve) => {
          releaseSend = resolve;
        });
      }
      if (routes.sendStatus) {
        return new Response(JSON.stringify(routes.sendBody ?? { error: "internal" }), {
          status: routes.sendStatus,
        });
      }
      return new Response(JSON.stringify(routes.send ?? { status: "delivered" }), { status: 200 });
    }
    if (url.includes("/session-queue")) {
      if (routes.queueHang) {
        return new Promise<Response>((resolve) => {
          releaseQueue = resolve;
        });
      }
      if (routes.queueReject) throw new TypeError("queue 网络断开（模拟复核失败）");
      return new Response(JSON.stringify({ items: routes.queue ?? [] }), { status: 200 });
    }
    throw new Error(`unexpected fetch: ${url}`);
  });
  vi.stubGlobal("fetch", fetchMock);
  // attachHang 放行器：对挂起中的 FakeXHR respond（Response → status+text 异步提取）
  releaseAttach = (r: Response) => {
    const xhr = pendingAttachXhr;
    pendingAttachXhr = null;
    if (!xhr) return;
    void r.text().then((body) => xhr.respond(r.status, body));
  };
  // 上行不再走 fetch（Task 10 §C5：uploadAttachment 换 XHR——fetch 规范无上传进度
  // 事件），/session-attachment 的路由改由上面的 FakeXHR 承接（同一 routes 语义）
  vi.stubGlobal("XMLHttpRequest", FakeXHR as unknown as typeof XMLHttpRequest);
}

// ==== FakeXHR：uploadAttachment 的 XHR 假体（路由语义与原 fetch 分支逐字对应）====
/** 已 open 的上传 URL（原 fetch 分支经 fetchMock.mock.calls 断言，XHR 后改经此） */
const attachOpens: string[] = [];
/** attachHang 挂起中的 XHR 实例（releaseAttach 放行时对其 respond） */
let pendingAttachXhr: FakeXHR | null = null;

class FakeXHR {
  upload = { onprogress: null as ((e: ProgressEvent) => void) | null };
  onload: ((e: ProgressEvent) => void) | null = null;
  onerror: (() => void) | null = null;
  onabort: (() => void) | null = null;
  status = 0;
  responseText = "";
  open(_method: string, url: string) {
    attachOpens.push(url);
  }
  setRequestHeader() {}
  send() {
    // 上传进度事件（§C5）：可指定 loaded/total（缺省不发——既有用例不受影响）
    if (routes.attachProgress) {
      const [loaded, total] = routes.attachProgress;
      this.upload.onprogress?.(new ProgressEvent("progress", { loaded, total }));
    }
    if (routes.attachHang) {
      pendingAttachXhr = this;
      return;
    }
    this.respond(
      routes.attachStatus ?? 200,
      JSON.stringify(
        routes.attachStatus
          ? { error: routes.attachError ?? "too_large" }
          : (routes.attach ?? { path: "E:/proj/.tuvis-attachments/s-1/1-a.bin", size: 0 })
      )
    );
  }
  abort() {
    this.onabort?.();
  }
  /** 测试放行器：置 status/responseText 后触发 onload（模拟响应到达） */
  respond(status: number, body: string) {
    this.status = status;
    this.responseText = body;
    this.onload?.(new ProgressEvent("load"));
  }
}

/** POST /session-send 的调用（URL 精确到 /session-send 结尾，排除 -info 前缀） */
function sendCalls(): Array<Array<unknown>> {
  return fetchMock.mock.calls.filter((c: unknown[]) => /\/session-send$/.test(String(c[0])));
}

/** GET /session-queue（列表）调用数（列表带 ?session_id= 查询串，排除 jump/retract） */
function queueListCalls(): number {
  return fetchMock.mock.calls.filter((c: unknown[]) => {
    const u = String(c[0]);
    return u.includes("/session-queue?") || u.endsWith("/session-queue");
  }).length;
}

/** POST /session-question/answer 的调用（丁T6：composer 转向 freeText 的出口） */
function answerCalls(): Array<Array<unknown>> {
  return fetchMock.mock.calls.filter((c: unknown[]) =>
    String(c[0]).includes("/session-question/answer")
  );
}

/** 可自由作答的问答载荷夹具（claude 已定案形态：answerable + freeText + 单题单选） */
function freeTextQuestionInfo(): NonNullable<Routes["questionInfo"]> {
  return {
    available: true,
    answerable: true,
    freeText: true,
    questions: [{ question: "构建产物放哪个目录？", multiSelect: false }],
  };
}

/** 放行 mock fetch 的 promise 链（若干轮微任务冲刷，足以走完 fetch→json→setState） */
async function flushAsync() {
  for (let i = 0; i < 6; i += 1) {
    await act(async () => {
      await Promise.resolve();
    });
  }
}

describe("MessageComposer：发送与回执（W4）", () => {
  it("injectable=true：多行文本原样上行（body={sessionId,text}），回执「已送达终端」且输入清空", async () => {
    installFetch();
    routes.info = sendInfo();
    routes.send = { status: "delivered" };
    render(<MessageComposer session={{ id: "sess-1" }} />);
    const input = (await screen.findByTestId("composer-input")) as HTMLTextAreaElement;
    fireEvent.change(input, { target: { value: "第一行\n第二行" } });
    fireEvent.click(screen.getByTestId("composer-send"));
    expect(await screen.findByTestId("send-receipt-delivered").then((el) => el.textContent)).toBe(
      "已送达终端"
    );
    // fetch body 契约：多行原样上行（归一在服务端，前端不动文本）
    expect(sendCalls()).toHaveLength(1);
    expect(JSON.parse(String((sendCalls()[0][1] as RequestInit).body))).toEqual({
      sessionId: "sess-1",
      text: "第一行\n第二行",
    });
    // 送达成功 → 输入清空
    expect((screen.getByTestId("composer-input") as HTMLTextAreaElement).value).toBe("");
  });

  it("injectable=false：输入区禁用 + 展示 reason（send-disabled-reason）", async () => {
    installFetch();
    routes.info = sendInfo({
      injectable: false,
      reasonCode: "blackbox",
      reason: "WorkBuddy 黑盒会话无法注入",
    });
    render(<MessageComposer session={{ id: "sess-1" }} />);
    expect(
      await screen.findByTestId("send-disabled-reason").then((el) => el.textContent)
    ).toContain("WorkBuddy 黑盒会话无法注入");
    expect((screen.getByTestId("composer-input") as HTMLTextAreaElement).disabled).toBe(true);
    expect((screen.getByTestId("composer-send") as HTMLButtonElement).disabled).toBe(true);
  });

  it("排队态：列表行「第1位」+ 立即发送/撤回；撤回成功（{ok:true}）→ 免复核直接收敛", async () => {
    installFetch();
    routes.info = sendInfo();
    routes.send = { status: "queued", itemId: 7, position: 1 };
    routes.queue = [];
    render(<MessageComposer session={{ id: "sess-1" }} />);
    const input = await screen.findByTestId("composer-input");
    fireEvent.change(input, { target: { value: "跑个长任务" } });
    fireEvent.click(screen.getByTestId("composer-send"));
    // D8：排队呈现由列表行承担（单回执槽不再渲染 queued chip）
    await screen.findByTestId("queue-row-7");
    expect(screen.getByTestId("queue-position-7").textContent).toContain("第1位");
    expect(screen.getByTestId("queue-jump").textContent).toContain("立即发送");
    expect(screen.getByTestId("queue-retract").textContent).toContain("撤回");
    // 撤回：服务端确认已撤（200 {ok:true}）→ 免复核直接收敛（评审必须1快路径）
    fireEvent.click(screen.getByTestId("queue-retract"));
    await waitFor(() => expect(screen.queryByTestId("queue-row-7")).toBeNull());
    expect(
      fetchMock.mock.calls.some((c: unknown[]) => String(c[0]).includes("/session-queue/retract"))
    ).toBe(true);
    // 挂载拉取 1 次 = 唯一的列表调用：{ok:true} 快路径不再拉队列复核（原锁保留）
    expect(queueListCalls()).toBe(1);
  });

  it("立即发送（插队）：按 itemId 点名直发，delivered 后行移除且回执转「已送达终端」", async () => {
    installFetch();
    routes.info = sendInfo();
    routes.send = { status: "queued", itemId: 7, position: 1 };
    routes.jump = { status: "delivered" };
    render(<MessageComposer session={{ id: "sess-1" }} />);
    const input = await screen.findByTestId("composer-input");
    fireEvent.change(input, { target: { value: "插队试试" } });
    fireEvent.click(screen.getByTestId("composer-send"));
    await screen.findByTestId("queue-row-7");
    fireEvent.click(screen.getByTestId("queue-jump"));
    expect(await screen.findByTestId("send-receipt-delivered")).toBeTruthy();
    const jumpCall = fetchMock.mock.calls.find((c: unknown[]) =>
      String(c[0]).includes("/session-queue/jump")
    );
    expect(jumpCall).toBeTruthy();
    expect(JSON.parse(String((jumpCall![1] as RequestInit).body))).toEqual({
      sessionId: "sess-1",
      itemId: 7,
    });
    // 条目已出队：行即时移除（原 queued chip 收敛断言的列表等价）
    expect(screen.queryByTestId("queue-row-7")).toBeNull();
  });

  it("回车不触发发送（textarea 天然换行）；text.trim() 为空时发送按钮禁用", async () => {
    installFetch();
    routes.info = sendInfo();
    render(<MessageComposer session={{ id: "sess-1" }} />);
    const input = (await screen.findByTestId("composer-input")) as HTMLTextAreaElement;
    const sendBtn = screen.getByTestId("composer-send") as HTMLButtonElement;
    expect(sendBtn.disabled).toBe(true); // 初始空文案禁用
    fireEvent.change(input, { target: { value: "   " } });
    expect(sendBtn.disabled).toBe(true); // 纯空白仍禁用
    fireEvent.change(input, { target: { value: "第一行" } });
    expect(sendBtn.disabled).toBe(false);
    fireEvent.keyDown(input, { key: "Enter" });
    await flushAsync();
    expect(sendCalls()).toHaveLength(0); // 回车不发送
    expect(input.value).toBe("第一行"); // 输入保持
  });

  it("发送失败（failed 回执）：红 chip 带错误文案、输入保留，可重试转绿", async () => {
    installFetch();
    routes.info = sendInfo();
    routes.send = { status: "failed", error: "定位终端失败：tmux 会话不存在" };
    render(<MessageComposer session={{ id: "sess-1" }} />);
    const input = await screen.findByTestId("composer-input");
    fireEvent.change(input, { target: { value: "再来一次" } });
    fireEvent.click(screen.getByTestId("composer-send"));
    const chip = await screen.findByTestId("send-receipt-failed");
    expect(chip.textContent).toContain("发送失败");
    expect(chip.textContent).toContain("定位终端失败：tmux 会话不存在");
    // 失败不清空输入（保留原文供重试）
    expect((screen.getByTestId("composer-input") as HTMLTextAreaElement).value).toBe("再来一次");
    // 重试 = 再按发送：修正路由后转 delivered
    routes.send = { status: "delivered" };
    fireEvent.click(screen.getByTestId("composer-send"));
    expect(await screen.findByTestId("send-receipt-delivered")).toBeTruthy();
    expect(screen.queryByTestId("send-receipt-failed")).toBeNull();
    expect(sendCalls()).toHaveLength(2);
  });

  it("网络层 ApiError（如 500）：错误文案可见且可重试", async () => {
    installFetch();
    routes.info = sendInfo();
    routes.sendStatus = 500;
    render(<MessageComposer session={{ id: "sess-1" }} />);
    const input = await screen.findByTestId("composer-input");
    fireEvent.change(input, { target: { value: "你好" } });
    fireEvent.click(screen.getByTestId("composer-send"));
    const chip = await screen.findByTestId("send-receipt-failed");
    expect(chip.textContent).toContain("500");
    routes.sendStatus = undefined;
    routes.send = { status: "delivered" };
    fireEvent.click(screen.getByTestId("composer-send"));
    expect(await screen.findByTestId("send-receipt-delivered")).toBeTruthy();
  });
});

describe("MessageComposer：排队轮询（3s 定时器，unmount 清理）", () => {
  it("排队态 3s 轮询刷新队位；条目从队列消失则行收敛；卸载后停止轮询", async () => {
    vi.useFakeTimers();
    installFetch();
    routes.info = sendInfo();
    routes.send = { status: "queued", itemId: 7, position: 1 };
    routes.queue = [queueItem({ id: 7, position: 2 })];
    const { unmount } = render(<MessageComposer session={{ id: "sess-1" }} />);
    await flushAsync(); // send-info + 挂载队列拉取落地（D8：此时已有 id7 行，队位 2）
    fireEvent.change(screen.getByTestId("composer-input"), { target: { value: "长任务" } });
    fireEvent.click(screen.getByTestId("composer-send"));
    await flushAsync(); // send → queued（行乐观 upsert 为发送响应的队位 1）
    expect(screen.getByTestId("queue-position-7").textContent).toContain("第1位");
    // 挂载拉取 1 次；尚未到轮询点（原断言 0 的等价口径——挂载拉取计入）
    expect(queueListCalls()).toBe(1);
    // 3s 轮询一次：队位已前移（队首被 flush）→ 行更新为 第2位
    await act(async () => {
      await vi.advanceTimersByTimeAsync(3000);
    });
    expect(queueListCalls()).toBe(2);
    expect(screen.getByTestId("queue-position-7").textContent).toContain("第2位");
    // 条目消失（已被 flush 送达/他端撤回）→ 行随权威列表收敛
    routes.queue = [];
    await act(async () => {
      await vi.advanceTimersByTimeAsync(3000);
    });
    expect(screen.queryByTestId("queue-row-7")).toBeNull();
    // 卸载后定时器清理：不再轮询
    unmount();
    const before = queueListCalls();
    await act(async () => {
      await vi.advanceTimersByTimeAsync(9000);
    });
    expect(queueListCalls()).toBe(before);
  });
});

// ==== M9R 注入加固前端对齐（P2-7 对账恢复 / 灰3 投递中 chip / P2-10 补锁）====
describe("MessageComposer：M9R 注入加固（P2-7 / 灰3 / P2-10）", () => {
  it("jump_busy_restores_queued_view：插队遇忙（200 failed）→ fetchQueue 复核条目仍在 → 恢复排队视图与按钮；复核确认不在队 → 中性 gone 收敛（不落 failed）", async () => {
    installFetch();
    routes.info = sendInfo();
    routes.send = { status: "queued", itemId: 7, position: 1 };
    routes.jump = { status: "failed", error: "投递进行中，请稍后重试" };
    routes.queue = [queueItem({ id: 7, position: 2 })];
    render(<MessageComposer session={{ id: "sess-1" }} />);
    const input = await screen.findByTestId("composer-input");
    fireEvent.change(input, { target: { value: "插队一下" } });
    fireEvent.click(screen.getByTestId("composer-send"));
    await screen.findByTestId("queue-row-7");
    // 插队遇忙 → 复核 /session-queue → 条目仍在 pending → 恢复排队视图，
    // 队位刷新 第1位→第2位（行位次变化即复核发生的真证据，不用调用计数——轮询同端点）
    fireEvent.click(screen.getByTestId("queue-jump"));
    await waitFor(() =>
      expect(screen.getByTestId("queue-position-7").textContent).toContain("第2位")
    );
    await waitFor(() =>
      expect((screen.getByTestId("queue-jump") as HTMLButtonElement).disabled).toBe(false)
    );
    expect(screen.getByTestId("queue-retract")).toBeTruthy();
    expect(screen.queryByTestId("send-receipt-failed")).toBeNull();
    // 复核真不在队（并发消费已把条目投出）→ 中性 gone 收敛：
    // 大概率已送达（守卫方 flush 循环刚投出），不得落 failed「可重试」诱发重复注入
    routes.queue = [];
    fireEvent.click(screen.getByTestId("queue-jump"));
    const gone = await screen.findByTestId("send-receipt-gone");
    expect(gone.textContent).toBe("条目已离开队列（可能已送达，可在会话内容中确认）");
    expect(gone.textContent).not.toContain("可重试");
    expect(screen.queryByTestId("send-receipt-failed")).toBeNull();
    expect(screen.queryByTestId("queue-row-7")).toBeNull(); // 确认不在队 → 行同步移除
  });

  it("jump_submitted_neutral_chip：插队回执 submitted（L14：确认面不可达平台 macOS/Linux）→ 中性「已投递未确认」chip + 行移除；不落 failed（无「可重试」＝不诱导双发）", async () => {
    installFetch();
    routes.info = sendInfo();
    routes.send = { status: "queued", itemId: 7, position: 1 };
    // L14：非 Windows 无排空/屏读确认面 → 后端插队回执恒 submitted（旧口径回
    // delivered 是 macOS 假成功）。前端分支 j.status === "submitted"（kimi 排队制
    // 同款）此前无用例覆盖——本条即该分支的锁
    routes.jump = { status: "submitted" };
    routes.queue = [queueItem({ id: 7, position: 2 })];
    render(<MessageComposer session={{ id: "sess-1" }} />);
    const input = await screen.findByTestId("composer-input");
    fireEvent.change(input, { target: { value: "插队一下" } });
    fireEvent.click(screen.getByTestId("composer-send"));
    await screen.findByTestId("queue-row-7");
    fireEvent.click(screen.getByTestId("queue-jump"));
    const chip = await screen.findByTestId("send-receipt-submitted");
    expect(chip.textContent).toBe("已投递至终端输入，agent 空闲后处理（未确认落盘）");
    expect(chip.textContent).not.toContain("可重试");
    // 消息已在 TUI 手里 → 行离开 MAM 队列（不得走「复核 → gone」对账，
    // 更不得落 failed：两者都会诱导用户重发 = 对 TUI 那份双发）
    expect(screen.queryByTestId("queue-row-7")).toBeNull();
    expect(screen.queryByTestId("send-receipt-failed")).toBeNull();
    expect(screen.queryByTestId("send-receipt-gone")).toBeNull();
  });

  it("retract_failure_same_reconcile：撤回网络错 → 同款队列复核 → 条目仍在 → 恢复排队视图（不落 failed 终态，可重试）", async () => {
    installFetch();
    routes.info = sendInfo();
    routes.send = { status: "queued", itemId: 7, position: 1 };
    routes.retractReject = true;
    routes.queue = [queueItem({ id: 7, position: 1 })];
    render(<MessageComposer session={{ id: "sess-1" }} />);
    const input = await screen.findByTestId("composer-input");
    fireEvent.change(input, { target: { value: "待撤回" } });
    fireEvent.click(screen.getByTestId("composer-send"));
    await screen.findByTestId("queue-row-7");
    fireEvent.click(screen.getByTestId("queue-retract"));
    // 网络错 → 复核 /session-queue → 条目仍在队 → 排队视图恢复（按钮回到可点，可重试撤回）
    await screen.findByTestId("queue-row-7");
    expect(screen.getByTestId("queue-position-7").textContent).toContain("第1位");
    await waitFor(() =>
      expect((screen.getByTestId("queue-retract") as HTMLButtonElement).disabled).toBe(false)
    );
    expect(screen.queryByTestId("send-receipt-failed")).toBeNull();
  });

  it("maxlength_guard：textarea maxLength=10000；超限输入被截断（与后端 MAX_SEND_CHARS 对齐）", async () => {
    installFetch();
    routes.info = sendInfo();
    render(<MessageComposer session={{ id: "sess-1" }} />);
    const input = (await screen.findByTestId("composer-input")) as HTMLTextAreaElement;
    expect(input.maxLength).toBe(10000);
    fireEvent.change(input, { target: { value: "a".repeat(10003) } });
    expect((screen.getByTestId("composer-input") as HTMLTextAreaElement).value).toHaveLength(10000);
  });

  it("delivering_chip_during_await：发送 await 全程显示「投递中…」chip，完成后被结果 chip 覆盖（灰3 慢消费者不空白）", async () => {
    installFetch();
    routes.info = sendInfo();
    routes.sendHang = true;
    render(<MessageComposer session={{ id: "sess-1" }} />);
    const input = await screen.findByTestId("composer-input");
    fireEvent.change(input, { target: { value: "一万字长文（慢消费者）" } });
    fireEvent.click(screen.getByTestId("composer-send"));
    // await 未返回期间：投递中 chip 在场（长文投递可达分钟级，界面不空白）
    const delivering = await screen.findByTestId("send-receipt-delivering");
    expect(delivering.textContent).toContain("投递中…");
    expect(delivering.textContent).toContain("长文投递可能需要几分钟"); // 副文案（评审 Minor4）
    expect(screen.queryByTestId("send-receipt-delivered")).toBeNull();
    // 放行响应：结果 chip 覆盖投递中
    await act(async () => {
      releaseSend!(new Response(JSON.stringify({ status: "delivered" }), { status: 200 }));
    });
    expect(await screen.findByTestId("send-receipt-delivered")).toBeTruthy();
    expect(screen.queryByTestId("send-receipt-delivering")).toBeNull();
  });

  it("send_403_reason_chip：session-send 403 not_injectable → 失败 chip 含后端 reason 原文（P2-10）", async () => {
    installFetch();
    routes.info = sendInfo();
    routes.sendStatus = 403;
    routes.sendBody = {
      error: "not_injectable",
      reason: "会话形态已漂移为黑盒，无法注入",
      reasonCode: "blackbox",
    };
    render(<MessageComposer session={{ id: "sess-1" }} />);
    const input = await screen.findByTestId("composer-input");
    fireEvent.change(input, { target: { value: "你好" } });
    fireEvent.click(screen.getByTestId("composer-send"));
    const chip = await screen.findByTestId("send-receipt-failed");
    expect(chip.textContent).toContain("会话形态已漂移为黑盒，无法注入");
  });

  it("queued position=0 容忍：并发消费窗口返回位次 0 → 行显示「排队中」不带位次数字，按钮保留", async () => {
    installFetch();
    routes.info = sendInfo();
    routes.send = { status: "queued", itemId: 7, position: 0 };
    render(<MessageComposer session={{ id: "sess-1" }} />);
    const input = await screen.findByTestId("composer-input");
    fireEvent.change(input, { target: { value: "刚入队即撞上消费窗口" } });
    fireEvent.click(screen.getByTestId("composer-send"));
    await screen.findByTestId("queue-row-7");
    expect(screen.getByTestId("queue-position-7").textContent).toBe("排队中");
    expect(screen.getByTestId("queue-jump")).toBeTruthy();
    expect(screen.getByTestId("queue-retract")).toBeTruthy();
  });

  it("retract busy 分流（评审必须1）：撤回忙时（200 failed 条目仍在队）→ 复核在队恢复（队位 1→2 为复核真证据）；复核也失败 → 保守恢复排队视图不失控", async () => {
    installFetch();
    routes.info = sendInfo();
    routes.send = { status: "queued", itemId: 7, position: 1 };
    routes.retractBusy = true; // 后端忙时拒收：200 {status:"failed"}，条目未被撤
    routes.queue = [queueItem({ id: 7, position: 2 })];
    render(<MessageComposer session={{ id: "sess-1" }} />);
    const input = await screen.findByTestId("composer-input");
    fireEvent.change(input, { target: { value: "撤回我" } });
    fireEvent.click(screen.getByTestId("composer-send"));
    await screen.findByTestId("queue-row-7");
    // 阶段一：忙时撤回 → 复核 /session-queue → 条目仍在 → 恢复排队视图；
    // 队位 第1位→第2位（行位次变化即复核发生的真证据，不用调用计数——轮询同端点会污染）
    fireEvent.click(screen.getByTestId("queue-retract"));
    await waitFor(() =>
      expect(screen.getByTestId("queue-position-7").textContent).toContain("第2位")
    );
    await waitFor(() =>
      expect((screen.getByTestId("queue-retract") as HTMLButtonElement).disabled).toBe(false)
    );
    expect(screen.queryByTestId("send-receipt-failed")).toBeNull();
    expect(screen.queryByTestId("send-receipt-gone")).toBeNull();
    // 阶段二：复核也网络失败 → 保守恢复排队视图（沿用最近已知队位），条目实际仍在队、
    // 撤回/插队按钮不失控，3s 轮询随后自愈——不得落回执消失/终态
    routes.queueReject = true;
    fireEvent.click(screen.getByTestId("queue-retract"));
    await waitFor(() =>
      expect((screen.getByTestId("queue-retract") as HTMLButtonElement).disabled).toBe(false)
    );
    expect(screen.getByTestId("queue-position-7").textContent).toContain("第2位");
    expect(screen.getByTestId("queue-jump")).toBeTruthy();
    expect(screen.queryByTestId("send-receipt-failed")).toBeNull();
    expect(screen.queryByTestId("send-receipt-gone")).toBeNull();
  });

  it("retract gone 中性收敛（评审必须2）：撤回失败后复核确认不在队 → 中性「条目已不在队列」，不标失败不带可重试", async () => {
    installFetch();
    routes.info = sendInfo();
    routes.send = { status: "queued", itemId: 7, position: 1 };
    routes.retractReject = true; // 撤回网络错
    routes.queue = []; // 复核确认条目已不在队（已被消费/他端撤回）
    render(<MessageComposer session={{ id: "sess-1" }} />);
    const input = await screen.findByTestId("composer-input");
    fireEvent.change(input, { target: { value: "试试撤回" } });
    fireEvent.click(screen.getByTestId("composer-send"));
    await screen.findByTestId("queue-row-7");
    fireEvent.click(screen.getByTestId("queue-retract"));
    const gone = await screen.findByTestId("send-receipt-gone");
    expect(gone.textContent).toBe("条目已不在队列");
    expect(gone.textContent).not.toContain("可重试");
    expect(screen.queryByTestId("send-receipt-failed")).toBeNull();
    expect(screen.queryByTestId("queue-row-7")).toBeNull();
  });

  it("sending 期间排队按钮加闸（评审必须3）：发送 await 未返回时 queue-jump/queue-retract 可见但禁用", async () => {
    installFetch();
    routes.info = sendInfo();
    routes.send = { status: "queued", itemId: 7, position: 1 };
    render(<MessageComposer session={{ id: "sess-1" }} />);
    const input = await screen.findByTestId("composer-input");
    fireEvent.change(input, { target: { value: "第一条（入队）" } });
    fireEvent.click(screen.getByTestId("composer-send"));
    await screen.findByTestId("queue-row-7");
    // 第二条发送挂起（慢消费者）：sending=true 期间排队操作面可见但不可点
    routes.sendHang = true;
    fireEvent.change(screen.getByTestId("composer-input"), {
      target: { value: "第二条（慢投递）" },
    });
    fireEvent.click(screen.getByTestId("composer-send"));
    await screen.findByTestId("send-receipt-delivering");
    expect(screen.getByTestId("queue-jump")).toBeTruthy(); // 可见（不失联）
    expect(screen.getByTestId("queue-retract")).toBeTruthy();
    expect((screen.getByTestId("queue-jump") as HTMLButtonElement).disabled).toBe(true);
    expect((screen.getByTestId("queue-retract") as HTMLButtonElement).disabled).toBe(true);
    // 放行第二条 → delivered：单回执槽转 delivered；已排队的第一条仍按条保留在列
    // （列表按条呈现，不随最近一次发送的回执消失），闸随 sending 翻转解除
    await act(async () => {
      releaseSend!(new Response(JSON.stringify({ status: "delivered" }), { status: 200 }));
    });
    expect(await screen.findByTestId("send-receipt-delivered")).toBeTruthy();
    expect(screen.getByTestId("queue-row-7")).toBeTruthy();
    expect((screen.getByTestId("queue-jump") as HTMLButtonElement).disabled).toBe(false);
    expect((screen.getByTestId("queue-retract") as HTMLButtonElement).disabled).toBe(false);
  });
});

// ==== 「修改」按钮 + 排队条目他端消失提示（2026-09-20 用户裁决）====
describe("排队回执：修改按钮（撤回保持丢弃语义）", () => {
  /** 发送一条进入排队态的公共前缀：D8 下排队呈现=列表行（queue-row-7），
   *  queued 回执仅内部存在（不再渲染 chip） */
  async function sendIntoQueued(text = "跑个长任务") {
    installFetch();
    routes.info = sendInfo();
    routes.send = { status: "queued", itemId: 7, position: 1 };
    render(<MessageComposer session={{ id: "sess-1" }} />);
    const input = await screen.findByTestId("composer-input");
    fireEvent.change(input, { target: { value: text } });
    fireEvent.click(screen.getByTestId("composer-send"));
    await screen.findByTestId("queue-row-7");
    return input;
  }

  it("排队态三钮并存：立即发送 / 修改 / 撤回", async () => {
    await sendIntoQueued();
    expect(screen.getByTestId("queue-jump").textContent).toContain("立即发送");
    expect(screen.getByTestId("queue-edit").textContent).toContain("修改");
    expect(screen.getByTestId("queue-retract").textContent).toContain("撤回");
  });

  it("修改：确认出队（{ok:true}）→ 正文放回输入框（可继续编辑），行收敛", async () => {
    const input = await sendIntoQueued("跑个长任务");
    fireEvent.click(screen.getByTestId("queue-edit"));
    await waitFor(() => expect(screen.queryByTestId("queue-row-7")).toBeNull());
    expect((screen.getByTestId("composer-input") as HTMLTextAreaElement).value).toBe("跑个长任务");
    expect(
      fetchMock.mock.calls.some((c: unknown[]) => String(c[0]).includes("/session-queue/retract"))
    ).toBe(true);
  });

  it("修改：正文超长时截断到 MAX_SEND_CHARS（与输入框 maxLength 对齐）", async () => {
    const long = "长".repeat(10001);
    const input = await sendIntoQueued(long);
    fireEvent.click(screen.getByTestId("queue-edit"));
    await waitFor(() => expect(screen.queryByTestId("queue-row-7")).toBeNull());
    expect((screen.getByTestId("composer-input") as HTMLTextAreaElement).value).toHaveLength(10000);
  });

  it("修改：忙时 failed（条目仍在队）→ 排队视图保留、输入框保持为空（防双份）", async () => {
    routes.retractBusy = true;
    routes.queue = [queueItem({ id: 7, position: 1, content: "跑个长任务" })];
    const input = await sendIntoQueued("跑个长任务");
    // 复核发现条目仍在队 → 恢复排队视图；正文不得放回（否则队里 + 输入框双份）。
    // 挂载拉取已计入列表调用，改用增量断言锁定「复核确已发生」
    const callsBefore = queueListCalls();
    fireEvent.click(screen.getByTestId("queue-edit"));
    await waitFor(() => expect(queueListCalls()).toBeGreaterThan(callsBefore));
    expect(screen.getByTestId("queue-row-7")).toBeTruthy();
    expect((screen.getByTestId("composer-input") as HTMLTextAreaElement).value).toBe("");
  });

  it("撤回回归锁：仍为完全取消——确认出队后行收敛且输入框保持为空", async () => {
    const input = await sendIntoQueued("只想取消");
    fireEvent.click(screen.getByTestId("queue-retract"));
    await waitFor(() => expect(screen.queryByTestId("queue-row-7")).toBeNull());
    expect((screen.getByTestId("composer-input") as HTMLTextAreaElement).value).toBe("");
  });
});

// ==== 修改重发只入队（D6，验收问题 #4）：修改后的重发强制走队列，防变相插队 ====
describe("修改重发只入队（D6）", () => {
  /** 发送一条进入排队态的公共前缀：D8 下排队呈现=列表行（queue-row-7） */
  async function sendIntoQueued(text: string) {
    installFetch();
    routes.info = sendInfo();
    routes.send = { status: "queued", itemId: 7, position: 1 };
    render(<MessageComposer session={{ id: "sess-1" }} />);
    const input = await screen.findByTestId("composer-input");
    fireEvent.change(input, { target: { value: text } });
    fireEvent.click(screen.getByTestId("composer-send"));
    await screen.findByTestId("queue-row-7");
    return input;
  }

  it("edit_resend_queues_only：修改确认出队后再发送 → 请求体带 queueOnly:true（手动改字不清标志）", async () => {
    const input = await sendIntoQueued("跑个长任务");
    fireEvent.click(screen.getByTestId("queue-edit"));
    await waitFor(() => expect(screen.queryByTestId("queue-row-7")).toBeNull());
    expect((input as HTMLTextAreaElement).value).toBe("跑个长任务");
    // 用户手动改字不清除标志（保守语义：修改后的重发一律入队）。入队后的放行
    // 节奏：会话转闲跃迁后事件臂即时放行；已空闲且无跃迁时由 60s 周期兜底放行
    // （可达分钟级）
    fireEvent.change(input, { target: { value: "改好的重发" } });
    routes.send = { status: "queued", itemId: 9, position: 1 };
    fireEvent.click(screen.getByTestId("composer-send"));
    await screen.findByTestId("queue-row-9"); // 修改后的重发以新行回到列表
    // 调用序列：[0]=初次入队发送（不带标志），[1]=修改后的重发（带标志）
    expect(sendCalls()).toHaveLength(2);
    expect("queueOnly" in JSON.parse(String((sendCalls()[0][1] as RequestInit).body))).toBe(false);
    expect(JSON.parse(String((sendCalls()[1][1] as RequestInit).body))).toEqual({
      sessionId: "sess-1",
      text: "改好的重发",
      queueOnly: true,
    });
  });

  it("normal_send_omits_flag：未经修改的普通发送 → 请求体不含 queueOnly 键", async () => {
    installFetch();
    routes.info = sendInfo();
    routes.send = { status: "delivered" };
    render(<MessageComposer session={{ id: "sess-1" }} />);
    const input = await screen.findByTestId("composer-input");
    fireEvent.change(input, { target: { value: "普通发送" } });
    fireEvent.click(screen.getByTestId("composer-send"));
    await screen.findByTestId("send-receipt-delivered");
    expect(sendCalls()).toHaveLength(1);
    const body = JSON.parse(String((sendCalls()[0][1] as RequestInit).body));
    // api.ts 口径：未修改不带该键（缺省键，保持既有请求体形态零漂移）
    expect("queueOnly" in body).toBe(false);
    expect(body).toEqual({ sessionId: "sess-1", text: "普通发送" });
  });

  it("flag_consumed_on_send：修改→发送（带标志）→再发送 → 第二次不再带标志（消费即清）", async () => {
    await sendIntoQueued("第一版");
    fireEvent.click(screen.getByTestId("queue-edit"));
    await waitFor(() => expect(screen.queryByTestId("queue-row-7")).toBeNull());
    routes.send = { status: "delivered" };
    fireEvent.click(screen.getByTestId("composer-send")); // 修改后的重发：带标志
    await screen.findByTestId("send-receipt-delivered");
    // 调用序列：[0]=初次入队发送，[1]=修改后的重发（带标志）
    expect(sendCalls()).toHaveLength(2);
    expect(JSON.parse(String((sendCalls()[1][1] as RequestInit).body)).queueOnly).toBe(true);
    // 第二次发送：标志已消费即清，回归普通发送语义（失败重试/新消息均不带）
    fireEvent.change(screen.getByTestId("composer-input"), { target: { value: "下一条" } });
    fireEvent.click(screen.getByTestId("composer-send"));
    await screen.findByTestId("send-receipt-delivered");
    expect(sendCalls()).toHaveLength(3);
    const third = JSON.parse(String((sendCalls()[2][1] as RequestInit).body));
    expect("queueOnly" in third).toBe(false);
  });

  it("session_switch_drops_flag：换会话（同组件 rerender）即弃修改标志 → 新会话首条普通发送不含 queueOnly", async () => {
    installFetch();
    routes.info = sendInfo();
    routes.send = { status: "queued", itemId: 7, position: 1 };
    const view = render(<MessageComposer session={{ id: "sess-1" }} />);
    const input = await screen.findByTestId("composer-input");
    fireEvent.change(input, { target: { value: "旧会话排队" } });
    fireEvent.click(screen.getByTestId("composer-send"));
    await screen.findByTestId("queue-row-7");
    // 修改确认出队 → 标志置位（正文放回输入框）
    fireEvent.click(screen.getByTestId("queue-edit"));
    await waitFor(() => expect(screen.queryByTestId("queue-row-7")).toBeNull());
    expect((screen.getByTestId("composer-input") as HTMLTextAreaElement).value).toBe("旧会话排队");
    // 同组件换会话：queueOnlyNext 随 session.id effect 复位——旧会话「修改」的
    // 遗愿不得泄漏为新会话首条发送的入队意图
    view.rerender(<MessageComposer session={{ id: "sess-2" }} />);
    const input2 = await screen.findByTestId("composer-input"); // send-info 重拉后重新就绪
    routes.send = { status: "delivered" };
    fireEvent.change(input2, { target: { value: "新会话首条" } });
    fireEvent.click(screen.getByTestId("composer-send"));
    await screen.findByTestId("send-receipt-delivered");
    expect(sendCalls()).toHaveLength(2);
    const body = JSON.parse(String((sendCalls()[1][1] as RequestInit).body));
    expect("queueOnly" in body).toBe(false);
    expect(body).toEqual({ sessionId: "sess-2", text: "新会话首条" });
  });

  it("edit_busy_failure_does_not_arm_flag：修改忙时失败（复核条目仍在队）→ 不置标志，随后的新文本发送不含 queueOnly", async () => {
    routes.retractBusy = true; // 忙时拒收：条目未被撤、仍在队
    routes.queue = [queueItem({ id: 7, position: 1, content: "跑个长任务" })];
    const input = await sendIntoQueued("跑个长任务");
    // 复核条目仍在队 → 排队视图恢复、正文不放回（onConfirmed 未触发 → 标志未置位）。
    // 挂载拉取已计入列表调用，改用增量断言锁定「复核确已发生」
    const callsBefore = queueListCalls();
    fireEvent.click(screen.getByTestId("queue-edit"));
    await waitFor(() => expect(queueListCalls()).toBeGreaterThan(callsBefore));
    expect(screen.getByTestId("queue-row-7")).toBeTruthy();
    expect((screen.getByTestId("composer-input") as HTMLTextAreaElement).value).toBe("");
    // 随后输入全新文本发送：这是未经修改确认的普通发送，不得携带入队标志
    routes.send = { status: "delivered" };
    fireEvent.change(input, { target: { value: "全新消息" } });
    fireEvent.click(screen.getByTestId("composer-send"));
    await screen.findByTestId("send-receipt-delivered");
    expect(sendCalls()).toHaveLength(2);
    const body = JSON.parse(String((sendCalls()[1][1] as RequestInit).body));
    expect("queueOnly" in body).toBe(false);
  });
});

describe("排队条目他端消失（2026-09-20 调查修复）：轮询收敛留痕", () => {
  it("3s 轮询发现条目不在队 → 中性 gone 提示（含电脑端去向），不再静默消失", async () => {
    vi.useFakeTimers();
    installFetch();
    routes.info = sendInfo();
    routes.send = { status: "queued", itemId: 7, position: 1 };
    routes.queue = []; // 轮询时条目已不在队（被 flush 送达 / 桌面端处理）
    render(<MessageComposer session={{ id: "sess-1" }} />);
    await act(async () => {});
    fireEvent.change(screen.getByTestId("composer-input"), {
      target: { value: "长任务" },
    });
    fireEvent.click(screen.getByTestId("composer-send"));
    await act(async () => {});
    expect(screen.getByTestId("queue-row-7")).toBeTruthy();
    await act(async () => {
      await vi.advanceTimersByTimeAsync(3100);
    });
    const gone = screen.getByTestId("send-receipt-gone");
    expect(gone.textContent).toContain("电脑端");
    vi.useRealTimers();
  });
});

// ==== 直发确认分诊回执（D7/T3，验收问题 #5）：submitted 中性回执非失败 ====
describe("直发确认分诊回执（D7/T3）", () => {
  it("submitted 中性回执：中性文案、不带可重试、输入框已清空（对齐 delivered 口径）", async () => {
    installFetch();
    routes.info = sendInfo();
    routes.send = { status: "submitted" };
    render(<MessageComposer session={{ id: "sess-1" }} />);
    const input = await screen.findByTestId("composer-input");
    fireEvent.change(input, { target: { value: "已被 TUI 收进队列的消息" } });
    fireEvent.click(screen.getByTestId("composer-send"));
    const chip = await screen.findByTestId("send-receipt-submitted");
    expect(chip.textContent).toBe("已投递至终端输入，agent 空闲后处理（未确认落盘）");
    // 中性态：不冒充失败——failed chip 不在场、无「可重试」语义（重试 = 双发，
    // TUI 那份无法撤回）
    expect(chip.textContent).not.toContain("可重试");
    expect(screen.queryByTestId("send-receipt-failed")).toBeNull();
    // 消息已离开前端 → 输入框清空（对齐 delivered 口径）
    expect((screen.getByTestId("composer-input") as HTMLTextAreaElement).value).toBe("");
  });

  it("failed 防重警示回执照旧（T3 回归）：红 chip 携带后端防重文案与（可重试）", async () => {
    installFetch();
    routes.info = sendInfo();
    routes.send = {
      status: "failed",
      error: "已注入未确认（未见会话记录），请检查终端后重试",
    };
    render(<MessageComposer session={{ id: "sess-1" }} />);
    const input = await screen.findByTestId("composer-input");
    fireEvent.change(input, { target: { value: "滞留真失败" } });
    fireEvent.click(screen.getByTestId("composer-send"));
    const chip = await screen.findByTestId("send-receipt-failed");
    expect(chip.textContent).toContain("发送失败");
    expect(chip.textContent).toContain("已注入未确认（未见会话记录），请检查终端后重试");
    expect(chip.textContent).toContain("（可重试）");
    // 分诊两态不混淆：failed 不得渲染成 submitted 中性 chip
    expect(screen.queryByTestId("send-receipt-submitted")).toBeNull();
  });
});

// ==== 附件上传（2026-09-20）：+ 钮 / 粘贴图片 / 发送拼内联标记行 ====
describe("移动端附件上传（2026-09-20）", () => {
  /** jsdom 的 Blob 可能缺 arrayBuffer（Node 内建 File 才有）——兜底补齐 */
  function ensureFileArrayBuffer(file: File) {
    const proto = Object.getPrototypeOf(file) as { arrayBuffer?: unknown };
    if (typeof proto.arrayBuffer !== "function") {
      (Object.getPrototypeOf(file) as { arrayBuffer: () => Promise<ArrayBuffer> }).arrayBuffer =
        async () => new TextEncoder().encode("x").buffer as ArrayBuffer;
    }
  }

  function pngFile(name = "shot.png"): File {
    const f = new File([new Uint8Array([0x89, 0x50])], name, { type: "image/png" });
    ensureFileArrayBuffer(f);
    return f;
  }

  async function sendIntoQueuedWithAttachment() {
    installFetch();
    routes.info = sendInfo();
    routes.send = { status: "delivered" };
    routes.attach = { path: "E:/proj/.tuvis-attachments/s-1/1-shot.png", size: 2 };
    render(<MessageComposer session={{ id: "sess-1" }} />);
    const input = await screen.findByTestId("composer-input");
    fireEvent.change(input, { target: { value: "看这张图" } });
    fireEvent.change(screen.getByTestId("attach-file-input"), {
      target: { files: [pngFile()] },
    });
    // 上传完成 → ready chip 出现（名字显示）
    await screen.findByText("shot.png");
    fireEvent.click(screen.getByTestId("composer-send"));
    await screen.findByTestId("send-receipt-delivered");
  }

  it("选文件上传：chip 状态机 uploading→ready；发送文本含 <image path> 标记行", async () => {
    await sendIntoQueuedWithAttachment();
    const sendCall = sendCalls()[0];
    const sent = JSON.parse(String((sendCall![1] as RequestInit).body)).text as string;
    expect(sent).toContain("看这张图");
    expect(sent).toContain('<image path="E:/proj/.tuvis-attachments/s-1/1-shot.png">');
    // 发送成功 → chips 清空
    expect(screen.queryByTestId(/^attach-chip-/)).toBeNull();
    // 上传端点被调用（XHR open URL 含 session_id 与文件名；上行已换 XHR，§C5）
    expect(attachOpens.some((u) => u.includes("name=shot.png"))).toBe(true);
  });

  it("文档附件（非图片）发送拼 <file path> 标记行", async () => {
    installFetch();
    routes.info = sendInfo();
    routes.send = { status: "delivered" };
    routes.attach = { path: "E:/proj/.tuvis-attachments/s-1/1-报告.docx", size: 9 };
    render(<MessageComposer session={{ id: "sess-1" }} />);
    const input = await screen.findByTestId("composer-input");
    fireEvent.change(input, { target: { value: "见附件" } });
    const docx = new File([new Uint8Array([1, 2])], "报告.docx", {
      type: "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
    });
    ensureFileArrayBuffer(docx);
    fireEvent.change(screen.getByTestId("attach-file-input"), { target: { files: [docx] } });
    await screen.findByText("报告.docx");
    fireEvent.click(screen.getByTestId("composer-send"));
    await screen.findByTestId("send-receipt-delivered");
    const sendCall = sendCalls()[0];
    const sent = JSON.parse(String((sendCall![1] as RequestInit).body)).text as string;
    expect(sent).toContain('<file path="E:/proj/.tuvis-attachments/s-1/1-报告.docx">');
  });

  it("粘贴图片：textarea onPaste 捕获 clipboard 图片文件并走上传链路", async () => {
    installFetch();
    routes.info = sendInfo();
    routes.send = { status: "delivered" };
    routes.attach = { path: "E:/proj/.tuvis-attachments/s-1/1-paste.png", size: 2 };
    render(<MessageComposer session={{ id: "sess-1" }} />);
    const input = await screen.findByTestId("composer-input");
    fireEvent.change(input, { target: { value: "贴图" } });
    const file = pngFile("pasted.png");
    fireEvent.paste(input, { clipboardData: { files: [file] } });
    await screen.findByText("pasted.png");
    fireEvent.click(screen.getByTestId("composer-send"));
    await screen.findByTestId("send-receipt-delivered");
    const sendCall = sendCalls()[0];
    const sent = JSON.parse(String((sendCall![1] as RequestInit).body)).text as string;
    expect(sent).toContain('<image path="E:/proj/.tuvis-attachments/s-1/1-paste.png">');
  });

  it("上传中禁发（挂起请求不放行）；放行后 ready 可发送", async () => {
    installFetch();
    routes.info = sendInfo();
    routes.send = { status: "delivered" };
    routes.attachHang = true;
    render(<MessageComposer session={{ id: "sess-1" }} />);
    const input = await screen.findByTestId("composer-input");
    fireEvent.change(input, { target: { value: "边传边发？" } });
    fireEvent.change(screen.getByTestId("attach-file-input"), {
      target: { files: [pngFile("hang.png")] },
    });
    // chip 出现（uploading 态，文本前缀「上传中：」）→ 发送钮禁用
    await screen.findByTestId("attachment-chips");
    expect(screen.getByText("上传中：hang.png")).toBeTruthy();
    expect((screen.getByTestId("composer-send") as HTMLButtonElement).disabled).toBe(true);
    // 上传中 × 可点：移除 = 取消（中断在途 fetch，chip 消失，不再落盘）
    fireEvent.click(screen.getByRole("button", { name: "移除附件 hang.png" }));
    // 放行上传 → ready → 可发送
    releaseAttach!(new Response(JSON.stringify({ path: "E:/p", size: 1 }), { status: 200 }));
    await waitFor(() =>
      expect((screen.getByTestId("composer-send") as HTMLButtonElement).disabled).toBe(false)
    );
    // 取消后 chip 消失、发送钮恢复可用（无 ready 附件也不拦发送）
    await waitFor(() => screen.queryByTestId("attach-chips") === null);
    fireEvent.click(screen.getByTestId("composer-send"));
    await screen.findByTestId("send-receipt-delivered");
  });

  it("上传进度落 chip（§C5）：已传字节 + 百分比（有 total 才有）+ 预估剩余", async () => {
    installFetch();
    routes.info = sendInfo();
    routes.send = { status: "delivered" };
    routes.attachHang = true;
    routes.attachProgress = [3, 10]; // send 时先发一发 onprogress（30%）
    render(<MessageComposer session={{ id: "sess-1" }} />);
    const input = await screen.findByTestId("composer-input");
    fireEvent.change(input, { target: { value: "边传边看" } });
    fireEvent.change(screen.getByTestId("attach-file-input"), {
      target: { files: [pngFile("prog.png")] },
    });
    // 进度细节 chip：已传 3 B / 10 B · 30%（「上传中：prog.png」仍是独立文本节点）
    await screen.findByTestId("attachment-chips");
    // 负向先行断言排除进度条（Task 10 复评 I-2 起 total 已知时同屏还有
    // `attach-progress-bar-<id>`；本用例只要文字进度那个节点，否则命中多个）
    const progress = await screen.findByTestId(/^attach-progress-(?!bar-)/);
    expect(progress.textContent).toContain("已传 3 B");
    expect(progress.textContent).toContain("10 B");
    expect(progress.textContent).toContain("30%");
  });

  // ==== Task 10 §C5 复评 I-2（2026-10-07 用户裁决以线稿为准）：上传也要进度条 ====
  // 线稿 .upl 画块 = upl-head（文字进度）+ .pbar（进度条）+ .upl-meta（口径说明），
  // 三者缺一即与线稿不一致。类串照抄下行 FilePreview.tsx:389-399 的写法。
  it("§C5/I-2：上传中渲染进度条（total 已知 → 宽度 = 百分比，类串同下行）", async () => {
    installFetch();
    routes.info = sendInfo();
    routes.send = { status: "delivered" };
    routes.attachHang = true;
    routes.attachProgress = [3, 10]; // 30%
    render(<MessageComposer session={{ id: "sess-1" }} />);
    const input = await screen.findByTestId("composer-input");
    fireEvent.change(input, { target: { value: "边传边看条" } });
    fireEvent.change(screen.getByTestId("attach-file-input"), {
      target: { files: [pngFile("bar.png")] },
    });
    await screen.findByTestId("attachment-chips");
    const bar = (await screen.findByTestId(/^attach-progress-bar-/)) as HTMLElement;
    // 内层填充 = 百分比宽（与下行 preview-progress-bar 同款「填充带 testid」写法）
    expect(bar.style.width).toBe("30%");
    expect(bar.className).toContain("bg-[var(--btnp)]");
    // 轨道 = 线稿 .pbar（h-1.5 圆角 + 底色）
    expect(bar.parentElement?.className).toContain("h-1.5");
    expect(bar.parentElement?.className).toContain("bg-[var(--cb)]");
    // 进度条与文字进度同处 chip 区（不是挤在 chip 里：进度条是 chip 的兄弟节点）
    expect(screen.getByTestId("attachment-chips").contains(bar)).toBe(true);
  });

  it("§C5/I-2：total 未知（事件报 0）→ 不渲染进度条，只显示已传字节（不编假百分比）", async () => {
    installFetch();
    routes.info = sendInfo();
    routes.send = { status: "delivered" };
    routes.attachHang = true;
    routes.attachProgress = [3, 0]; // total=0 = 未知（隧道不透传 content-length）
    render(<MessageComposer session={{ id: "sess-1" }} />);
    const input = await screen.findByTestId("composer-input");
    fireEvent.change(input, { target: { value: "不知道多大" } });
    fireEvent.change(screen.getByTestId("attach-file-input"), {
      target: { files: [pngFile("nototal.png")] },
    });
    await screen.findByTestId("attachment-chips");
    const progress = await screen.findByTestId(/^attach-progress-/);
    expect(progress.textContent).toContain("已传 3 B");
    // 不编假百分比：既无「%」字样，也无进度条元素
    expect(progress.textContent).not.toContain("%");
    expect(screen.queryByTestId(/^attach-progress-bar-/)).toBeNull();
  });

  it("§C5/I-2：上传中显示线稿 .upl-meta 口径说明行（上传结束即收起）", async () => {
    installFetch();
    routes.info = sendInfo();
    routes.send = { status: "delivered" };
    routes.attachHang = true;
    routes.attachProgress = [3, 10];
    render(<MessageComposer session={{ id: "sess-1" }} />);
    const input = await screen.findByTestId("composer-input");
    fireEvent.change(input, { target: { value: "口径说明" } });
    fireEvent.change(screen.getByTestId("attach-file-input"), {
      target: { files: [pngFile("meta.png")] },
    });
    await screen.findByTestId("attachment-chips");
    const meta = await screen.findByTestId("attach-upl-meta");
    // 线稿 :364 逐字（upl-meta）——锁定口径，防回退成「只在 chip 里加一行字」
    expect(meta.textContent).toBe(
      "受限通道按本通道实测速率预估剩余时间；速率未知时只显示已传字节，不显示假百分比。"
    );
    // 不在 chip 内（简报：别挤在 chip 里）
    expect(screen.getByTestId(/^attach-chip-/).contains(meta)).toBe(false);
    // 上传完成（ready）→ 「上传中」态结束 → 口径行收起
    releaseAttach!(new Response(JSON.stringify({ path: "E:/p", size: 3 }), { status: 200 }));
    await waitFor(() => expect(screen.queryByTestId("attach-upl-meta")).toBeNull());
  });

  // ==== Task 10 §C5 复评 I-2 后半：附件区也要出带宽横幅 ====
  // 设计说明书 §C5 原文「在文件面板与附件区显示提示」——只上传、不浏览文件的用户
  // 也要看得到「这是通道限制，不是故障」。判据与文案与 FilePanel 逐字同源（共用
  // ChannelBwNote 组件），同样带有限正数守卫。落点：附件 chips 容器下方（「附件区」
  // = 用户已进入附件流程时才就地出现，纯问答会话不常驻噪音——与 attach-hint 的
  // 「看过即收、不平铺常驻」同一取向）。

  /** 挂载 + 选一个附件（ready chip）→ 附件区成立 */
  async function renderComposerWithAttachment(name = "big.pdf") {
    render(<MessageComposer session={{ id: "sess-1" }} />);
    const input = await screen.findByTestId("composer-input");
    fireEvent.change(screen.getByTestId("attach-file-input"), {
      target: { files: [pngFile(name)] },
    });
    await screen.findByTestId("attachment-chips");
    return input;
  }

  it("§C5/I-2：受限通道（est 有效）→ 附件区出横幅，文案与 FilePanel 同源（含耗时预估）", async () => {
    installFetch();
    routes.info = sendInfo();
    routes.channel = { via: "quick", limited: true, est_mbps_down: 1.8, est_mbps_up: 0.8 };
    render(<MessageComposer session={{ id: "sess-1" }} />);
    const input = await screen.findByTestId("composer-input");
    // 未进入附件流程：附件区尚未成立 → 不常驻（判据之一，防「每个会话都挂一条」）。
    // 先等通道载荷真的到过（否则「没横幅」可能只是还没拉到，断言不算数）
    await waitFor(() =>
      expect(fetchMock.mock.calls.some((c) => String(c[0]).includes("/m/api/v1/channel"))).toBe(
        true
      )
    );
    expect(screen.queryByTestId("composer-bw-note")).toBeNull();
    fireEvent.change(screen.getByTestId("attach-file-input"), {
      target: { files: [pngFile("体检报告.pdf")] },
    });
    await screen.findByTestId("attachment-chips");
    const note = await screen.findByTestId("composer-bw-note");
    // 与 FilePanel 同口径：是通道限制不是故障 + 满速升级指引 + 实测速率耗时
    expect(note.textContent).toContain("通道限制");
    expect(note.textContent).toContain("不是故障");
    expect(note.textContent).toContain("Tailscale");
    expect(note.textContent).toContain("1.8");
    expect(note.textContent).toContain("0.8");
    // 20 MB 参考：下行 20*8/1.8/60≈1.5 分钟 / 上行 ≈3.3 分钟
    expect(note.textContent).toContain("1.5");
    expect(note.textContent).toContain("3.3");
    // 落点：chips 容器下方（附件区），且不在 chip 里
    expect(screen.getByTestId(/^attach-chip-/).contains(note)).toBe(false);
    expect(input).toBeTruthy();
  });

  it("§C5/I-2：limited=false（局域网直连）→ 附件区不出横幅", async () => {
    installFetch();
    routes.info = sendInfo();
    routes.channel = { via: "lan", limited: false, est_mbps_down: 0, est_mbps_up: 0 };
    await renderComposerWithAttachment();
    expect(screen.queryByTestId("composer-bw-note")).toBeNull();
  });

  it("§C5/I-2：畸形载荷（limited=true 但 est=0）→ 不出横幅（不渲染 Infinity 分钟）", async () => {
    installFetch();
    routes.info = sendInfo();
    routes.channel = { via: "quick", limited: true, est_mbps_down: 0, est_mbps_up: 0 };
    await renderComposerWithAttachment();
    expect(screen.queryByTestId("composer-bw-note")).toBeNull();
    expect(screen.queryByText(/Infinity/)).toBeNull();
  });

  it("§C5/I-2：est 非有限正数（JSON 1e999 → Infinity）→ 不出横幅（有限正数守卫）", async () => {
    installFetch();
    routes.info = sendInfo();
    // 原生文本：JSON.stringify(Infinity) 会写成 null，测不到 1e999 被解析为 Infinity
    routes.channelRaw = '{"via":"quick","limited":true,"est_mbps_down":1e999,"est_mbps_up":0.8}';
    await renderComposerWithAttachment();
    expect(screen.queryByTestId("composer-bw-note")).toBeNull();
    expect(screen.queryByText(/Infinity/)).toBeNull();
  });

  it("§C5/I-2：通道端点失败（403/网络异常）→ 静默不出横幅（装饰能力不阻塞输入区）", async () => {
    installFetch();
    routes.info = sendInfo();
    fetchMock.mockImplementation(async (input: RequestInfo | URL) => {
      const url = String(input);
      if (url.includes("/m/api/v1/channel")) throw new TypeError("channel 网络断开");
      if (url.includes("/session-send-info")) {
        return new Response(JSON.stringify(sendInfo()), { status: 200 });
      }
      if (url.includes("/session-approve-options")) {
        return new Response(JSON.stringify({ available: false, options: [] }), { status: 200 });
      }
      return new Response(JSON.stringify({ available: false, questions: [] }), { status: 200 });
    });
    await renderComposerWithAttachment("offline.pdf");
    expect(screen.queryByTestId("composer-bw-note")).toBeNull();
    // 输入区照常可用（不因装饰失败而禁用）
    expect((screen.getByTestId("composer-input") as HTMLTextAreaElement).disabled).toBe(false);
  });

  it("404 no_cwd：chip 标失败 + 「+」钮禁用（与 resume 禁用口径同源）", async () => {
    installFetch();
    routes.info = sendInfo();
    routes.send = { status: "delivered" };
    routes.attachStatus = 404;
    routes.attachError = "no_cwd";
    render(<MessageComposer session={{ id: "sess-1" }} />);
    const input = await screen.findByTestId("composer-input");
    fireEvent.change(screen.getByTestId("attach-file-input"), {
      target: { files: [pngFile("nc.png")] },
    });
    const chip = await screen.findByText("失败：nc.png", { exact: false });
    expect(chip.textContent).toContain("该会话没有项目目录信息");
    expect((screen.getByTestId("attach-add") as HTMLButtonElement).disabled).toBe(true);
  });

  it("「?」徽标：点开展开存储说明（含用户项目目录字样），再点收起", async () => {
    installFetch();
    routes.info = sendInfo();
    render(<MessageComposer session={{ id: "sess-1" }} />);
    const input = await screen.findByTestId("composer-input");
    expect(screen.queryByTestId("attach-hint")).toBeNull();
    fireEvent.click(screen.getByTestId("attach-help"));
    const hint = screen.getByTestId("attach-hint");
    expect(hint.textContent).toContain("用户项目目录");
    expect(hint.textContent).toContain(".tuvis-attachments");
    fireEvent.click(screen.getByTestId("attach-help"));
    expect(screen.queryByTestId("attach-hint")).toBeNull();
  });
});

// ==== 多列队 UI（D8，验收问题 #6）：完整 /session-queue 列表 + 逐条操作 ====
describe("多列队 UI（D8）：完整队列列表", () => {
  /** 三条既有队列夹具（挂载拉取即见——桌面端/他端排的队）：分别覆盖
   *  尾签名剥离+截断 / 短文全显 / 附件标记替换（丁T3 裁2：签名在尾部） */
  function threeQueueItems(): QueueItemView[] {
    return [
      queueItem({
        id: 11,
        position: 1,
        content: "这是一条超过十二个字符的长消息需要截断显示 [mobile 测试机]",
      }),
      queueItem({ id: 12, position: 2, content: "短消息" }),
      queueItem({ id: 13, position: 3, content: '<image path="E:/pool/a.png">看这张截图' }),
    ];
  }

  it("挂载即拉既有队列：三行预览（去 [mobile] 前缀/截 12 字符/附件替换）+ 各自三钮 + 底部说明", async () => {
    installFetch();
    routes.info = sendInfo();
    routes.queue = threeQueueItems();
    render(<MessageComposer session={{ id: "sess-1" }} />);
    const row11 = await screen.findByTestId("queue-row-11");
    // 尾签名剥离 + 截前 12 字符 + …，且 [mobile 签名不得泄漏到预览（丁T3 裁2）
    expect(row11.textContent).toContain("这是一条超过十二个字符的…");
    expect(row11.textContent).not.toContain("[mobile");
    // 不足 12 字符全显
    expect(screen.getByTestId("queue-row-12").textContent).toContain("短消息");
    // 附件内联标记替换为 [附件]
    expect(screen.getByTestId("queue-row-13").textContent).toContain("[附件]看这张截图");
    // 位次沿用「第 N 位」口径
    expect(screen.getByTestId("queue-position-11").textContent).toBe("第1位");
    expect(screen.getByTestId("queue-position-12").textContent).toBe("第2位");
    expect(screen.getByTestId("queue-position-13").textContent).toBe("第3位");
    // 逐条三钮（行内作用域）
    for (const id of [11, 12, 13]) {
      const row = screen.getByTestId(`queue-row-${id}`);
      expect(within(row).getByTestId("queue-jump").textContent).toContain("立即发送");
      expect(within(row).getByTestId("queue-edit").textContent).toContain("修改");
      expect(within(row).getByTestId("queue-retract").textContent).toContain("撤回");
    }
    // 底部说明
    expect(screen.getByTestId("queue-hint").textContent).toBe("空闲时将按序自动发送");
  });

  it("逐条操作带对 id：第二条立即发送 / 第三条撤回 / 第一条修改（放回正文去前缀）", async () => {
    installFetch();
    routes.info = sendInfo();
    routes.queue = threeQueueItems();
    routes.jump = { status: "delivered" };
    render(<MessageComposer session={{ id: "sess-1" }} />);
    await screen.findByTestId("queue-row-11");
    // 第二条「立即发送」→ jump 携带 itemId 12；成功后该行移除、他行保留
    fireEvent.click(within(screen.getByTestId("queue-row-12")).getByTestId("queue-jump"));
    await waitFor(() => expect(screen.queryByTestId("queue-row-12")).toBeNull());
    const jumpCall = fetchMock.mock.calls.find((c: unknown[]) =>
      String(c[0]).includes("/session-queue/jump")
    );
    expect(JSON.parse(String((jumpCall![1] as RequestInit).body))).toEqual({
      sessionId: "sess-1",
      itemId: 12,
    });
    expect(screen.getByTestId("queue-row-11")).toBeTruthy();
    expect(screen.getByTestId("queue-row-13")).toBeTruthy();
    // 第三条「撤回」→ retract 携带 itemId 13
    fireEvent.click(within(screen.getByTestId("queue-row-13")).getByTestId("queue-retract"));
    await waitFor(() => expect(screen.queryByTestId("queue-row-13")).toBeNull());
    const retractCalls = fetchMock.mock.calls.filter((c: unknown[]) =>
      String(c[0]).includes("/session-queue/retract")
    );
    expect(JSON.parse(String((retractCalls[0][1] as RequestInit).body))).toEqual({
      sessionId: "sess-1",
      itemId: 13,
    });
    // 第一条「修改」→ retract 携带 itemId 11；放回正文剥离 [mobile] **尾**签名
    // （行 content 是服务端 compose 后的带签名文本，不剥则重发二次叠加）
    fireEvent.click(within(screen.getByTestId("queue-row-11")).getByTestId("queue-edit"));
    await waitFor(() => expect(screen.queryByTestId("queue-row-11")).toBeNull());
    const retractCallsAfterEdit = fetchMock.mock.calls.filter((c: unknown[]) =>
      String(c[0]).includes("/session-queue/retract")
    );
    expect(retractCallsAfterEdit).toHaveLength(2);
    expect(JSON.parse(String((retractCallsAfterEdit[1][1] as RequestInit).body))).toEqual({
      sessionId: "sess-1",
      itemId: 11,
    });
    expect((screen.getByTestId("composer-input") as HTMLTextAreaElement).value).toBe(
      "这是一条超过十二个字符的长消息需要截断显示"
    );
    // 列表清空 → 列表与说明整体退场
    expect(screen.queryByTestId("queue-list")).toBeNull();
  });

  it("发送入队即时反馈：queued 回执不渲染 chip（单回执槽让位列表），乐观行立即在场", async () => {
    installFetch();
    routes.info = sendInfo();
    routes.send = { status: "queued", itemId: 7, position: 1 };
    routes.queue = [];
    render(<MessageComposer session={{ id: "sess-1" }} />);
    const input = await screen.findByTestId("composer-input");
    fireEvent.change(input, { target: { value: "刚入队的消息" } });
    fireEvent.click(screen.getByTestId("composer-send"));
    const row = await screen.findByTestId("queue-row-7");
    expect(screen.getByTestId("queue-position-7").textContent).toBe("第1位");
    expect(row.textContent).toContain("刚入队的消息");
    // 单回执槽：queued 态不再渲染回执 chip（由列表取代）；底部说明在场
    expect(screen.queryByTestId("send-receipt-queued")).toBeNull();
    expect(screen.getByTestId("queue-hint")).toBeTruthy();
  });
});

// ==== D8：列表消费收敛与轮询生命周期（复用 3s 通道）====
describe("多列队 UI（D8）：列表消费收敛与轮询生命周期", () => {
  it("轮询发现我方条目消失（他条仍在）→ gone 留痕 + 我方行移除他条保留；列表全空 → 轮询停止", async () => {
    vi.useFakeTimers();
    installFetch();
    routes.info = sendInfo();
    routes.send = { status: "queued", itemId: 7, position: 1 };
    routes.queue = [];
    render(<MessageComposer session={{ id: "sess-1" }} />);
    await flushAsync();
    fireEvent.change(screen.getByTestId("composer-input"), { target: { value: "我方的长任务" } });
    fireEvent.click(screen.getByTestId("composer-send"));
    await flushAsync();
    expect(screen.getByTestId("queue-row-7")).toBeTruthy();
    // 轮询一：我方条目已被消费（他端 flush / 桌面处理），他条仍在 → gone 中性留痕，
    // 我方行随权威列表移除、他条保留可操作
    routes.queue = [queueItem({ id: 9, position: 1, content: "他端排的队 [mobile 台式机]" })];
    await act(async () => {
      await vi.advanceTimersByTimeAsync(3000);
    });
    const gone = screen.getByTestId("send-receipt-gone");
    expect(gone.textContent).toContain("电脑端");
    expect(screen.queryByTestId("queue-row-7")).toBeNull();
    expect(screen.getByTestId("queue-row-9")).toBeTruthy();
    // 轮询二：列表全空 → 行清空、轮询停止（interval 清除：再推进时间无新调用）
    routes.queue = [];
    await act(async () => {
      await vi.advanceTimersByTimeAsync(3000);
    });
    expect(screen.queryByTestId("queue-list")).toBeNull();
    const before = queueListCalls();
    await act(async () => {
      await vi.advanceTimersByTimeAsync(9000);
    });
    expect(queueListCalls()).toBe(before);
  });
});

// ==== D8：单回执槽独立性（最近发送的终态回执与列表并存）====
describe("多列队 UI（D8）：单回执槽独立性", () => {
  /** 挂载即有两条既有队列的公共前缀 */
  async function renderWithExistingQueue() {
    installFetch();
    routes.info = sendInfo();
    routes.queue = [
      queueItem({ id: 21, position: 1, content: "既有甲" }),
      queueItem({ id: 22, position: 2, content: "既有乙" }),
    ];
    render(<MessageComposer session={{ id: "sess-1" }} />);
    await screen.findByTestId("queue-row-21");
  }

  it("最近发送 failed：红 chip 与既有列表并存（列表不因失败回执消失）", async () => {
    await renderWithExistingQueue();
    routes.send = { status: "failed", error: "定位终端失败" };
    fireEvent.change(screen.getByTestId("composer-input"), { target: { value: "新消息" } });
    fireEvent.click(screen.getByTestId("composer-send"));
    expect(await screen.findByTestId("send-receipt-failed")).toBeTruthy();
    expect(screen.getByTestId("queue-row-21")).toBeTruthy();
    expect(screen.getByTestId("queue-row-22")).toBeTruthy();
    expect(screen.getByTestId("queue-hint")).toBeTruthy();
  });

  it("最近发送 submitted/delivered：中性/绿色 chip 与既有列表并存", async () => {
    await renderWithExistingQueue();
    routes.send = { status: "submitted" };
    fireEvent.change(screen.getByTestId("composer-input"), { target: { value: "消息一" } });
    fireEvent.click(screen.getByTestId("composer-send"));
    expect(await screen.findByTestId("send-receipt-submitted")).toBeTruthy();
    expect(screen.getByTestId("queue-row-21")).toBeTruthy();
    routes.send = { status: "delivered" };
    fireEvent.change(screen.getByTestId("composer-input"), { target: { value: "消息二" } });
    fireEvent.click(screen.getByTestId("composer-send"));
    expect(await screen.findByTestId("send-receipt-delivered")).toBeTruthy();
    expect(screen.getByTestId("queue-row-22")).toBeTruthy();
  });
});

// ==== T4 复评（I1/M1/I2）：轮询在途快照的时序防御 ====
// 时序窗模拟：queueHang 让 tick GET 挂起 → 本地做一次认知翻转（入队/撤回/换会话）
// → releaseQueue 放行「早于翻转发起」的旧快照，验证其被整份作废。
describe("多列队 UI（D8）复评：在途 tick 快照时序防御", () => {
  it("I1 发送入队后，早于入队发起的在途 tick 快照到货 → 整份作废：乐观行不抹、不假收敛 gone、轮询不停摆", async () => {
    vi.useFakeTimers();
    installFetch();
    routes.info = sendInfo();
    // 挂载即有既有条目（先让轮询转起来，才能制造「早于入队发出的 GET」）
    routes.queue = [queueItem({ id: 9, position: 1, content: "既有条目" })];
    render(<MessageComposer session={{ id: "sess-1" }} />);
    await flushAsync(); // 挂载拉取落地 → 列表 [9]，轮询启动
    routes.queueHang = true;
    await act(async () => {
      await vi.advanceTimersByTimeAsync(3000); // tick1 GET 发出并挂起（早于下面的入队）
    });
    // tick1 在途期间发送入队：乐观行 X 上板，本地「X 在队」认知成立（晚于 tick1 发起）
    routes.send = { status: "queued", itemId: 7, position: 2 };
    routes.queueHang = false;
    fireEvent.change(screen.getByTestId("composer-input"), { target: { value: "新入队条目" } });
    fireEvent.click(screen.getByTestId("composer-send"));
    await flushAsync();
    expect(screen.getByTestId("queue-row-7")).toBeTruthy();
    // 放行 tick1 的旧快照：不含 X（入队前的服务端视图）→ 整份作废——
    // 不抹乐观行、不落假 gone（否则 X 实际仍在队、转闲会被 flush，用户重发=重复注入）
    await act(async () => {
      releaseQueue!(
        new Response(JSON.stringify({ items: [queueItem({ id: 9, position: 1 })] }), {
          status: 200,
        })
      );
    });
    expect(screen.getByTestId("queue-row-7")).toBeTruthy();
    expect(screen.queryByTestId("send-receipt-gone")).toBeNull();
    expect(screen.getByTestId("queue-row-9")).toBeTruthy(); // 旧视图未整表覆盖回退
    // 下一轮（晚于入队发起）的快照正常应用：权威列表上板、轮询自愈未停摆
    routes.queue = [
      queueItem({ id: 9, position: 1, content: "既有条目" }),
      queueItem({ id: 7, position: 2, content: "新入队条目" }),
    ];
    await act(async () => {
      await vi.advanceTimersByTimeAsync(3000);
    });
    expect(screen.getByTestId("queue-position-7").textContent).toBe("第2位");
  });

  it("I1/M1 撤回确认出队后，早于撤回发起的在途 tick 快照到货 → 不复活幽灵行", async () => {
    vi.useFakeTimers();
    installFetch();
    routes.info = sendInfo();
    routes.queue = [queueItem({ id: 7, position: 1, content: "待撤条目" })];
    render(<MessageComposer session={{ id: "sess-1" }} />);
    await flushAsync(); // 挂载拉取 → 行 7 在列，轮询启动
    routes.queueHang = true;
    await act(async () => {
      await vi.advanceTimersByTimeAsync(3000); // tick1 GET 发出并挂起（早于撤回）
    });
    // 撤回确认（{ok:true} 快路径）：行移除 + 本地「该条不在队」认知成立
    routes.queueHang = false;
    fireEvent.click(within(screen.getByTestId("queue-row-7")).getByTestId("queue-retract"));
    await flushAsync();
    expect(screen.queryByTestId("queue-row-7")).toBeNull();
    // 放行 tick1 旧快照（仍显示该条在队）→ 作废，幽灵行不得复活
    await act(async () => {
      releaseQueue!(
        new Response(
          JSON.stringify({ items: [queueItem({ id: 7, position: 1, content: "待撤条目" })] }),
          { status: 200 }
        )
      );
    });
    expect(screen.queryByTestId("queue-row-7")).toBeNull();
  });

  it("I2 换会话时在途 tick 响应到货 → 旧会话的行不落入新会话（alive 会话换防）", async () => {
    vi.useFakeTimers();
    installFetch();
    routes.info = sendInfo();
    routes.queue = [queueItem({ id: 9, position: 1, content: "旧会话的队列条目" })];
    const view = render(<MessageComposer session={{ id: "sess-1" }} />);
    await flushAsync(); // sess-1 挂载拉取 → [9]，轮询启动
    routes.queueHang = true;
    await act(async () => {
      await vi.advanceTimersByTimeAsync(3000); // sess-1 的 tick GET 发出并挂起
    });
    // 换会话：旧 tick effect 清理（alive=false）+ sess-2 挂载拉取（空列表）
    routes.queueHang = false;
    routes.queue = [];
    view.rerender(<MessageComposer session={{ id: "sess-2" }} />);
    await flushAsync(); // sess-2 的 send-info + 挂载队列拉取落地（微任务冲刷，假定时器下不用 findBy）
    expect(screen.getByTestId("composer-input")).toBeTruthy(); // sess-2 渲染就绪（断言才有意义）
    // 放行 sess-1 的在途 tick 快照（含旧会话条目）→ alive 守卫作废，不落入 sess-2
    await act(async () => {
      releaseQueue!(
        new Response(
          JSON.stringify({
            items: [queueItem({ id: 9, position: 1, content: "旧会话的队列条目" })],
          }),
          { status: 200 }
        )
      );
    });
    expect(screen.queryByTestId("queue-row-9")).toBeNull();
    expect(screen.queryByTestId("queue-list")).toBeNull();
  });
});

// ==== 丁T3 裁2：签名后置（尾部）——预览与「修改」回填剥**尾**签名 ====
describe("丁T3 签名后置：尾部签名剥离", () => {
  it("预览剥尾部签名：真机形态 `{正文} [mobile 设备名]` 不得在预览里残留签名", async () => {
    installFetch();
    routes.info = sendInfo();
    routes.queue = [
      queueItem({
        id: 31,
        position: 1,
        content: "这是一条超过十二个字符的长消息 [mobile iPhone 15]",
      }),
      queueItem({ id: 32, position: 2, content: "短消息 [mobile iPhone 15]" }),
    ];
    render(<MessageComposer session={{ id: "sess-1" }} />);
    const row31 = await screen.findByTestId("queue-row-31");
    expect(row31.textContent).toContain("这是一条超过十二个字符的…");
    expect(row31.textContent).not.toContain("[mobile");
    expect(screen.getByTestId("queue-row-32").textContent).not.toContain("[mobile");
    expect(screen.getByTestId("queue-row-32").textContent).toContain("短消息");
  });

  it("「修改」回填剥尾部签名（防重发二次叠加）", async () => {
    installFetch();
    routes.info = sendInfo();
    routes.queue = [queueItem({ id: 41, position: 1, content: "跑个长任务 [mobile 测试机]" })];
    render(<MessageComposer session={{ id: "sess-1" }} />);
    await screen.findByTestId("queue-row-41");
    fireEvent.click(screen.getByTestId("queue-edit"));
    await waitFor(() => expect(screen.queryByTestId("queue-row-41")).toBeNull());
    expect((screen.getByTestId("composer-input") as HTMLTextAreaElement).value).toBe("跑个长任务");
  });

  it("正文里中段的 [mobile…] 字样**不误剥**（只剥形态完整的尾签名——与 Rust 同口径）", async () => {
    installFetch();
    routes.info = sendInfo();
    routes.queue = [
      queueItem({ id: 42, position: 1, content: "看看 [mobile X] 这个标签怎么写 [mobile 测试机]" }),
    ];
    render(<MessageComposer session={{ id: "sess-2" }} />);
    await screen.findByTestId("queue-row-42");
    fireEvent.click(screen.getByTestId("queue-edit"));
    await waitFor(() => expect(screen.queryByTestId("queue-row-42")).toBeNull());
    expect((screen.getByTestId("composer-input") as HTMLTextAreaElement).value).toBe(
      "看看 [mobile X] 这个标签怎么写"
    );
  });

  it("无签名的队列条目（斜杠命令裸注入形态）原样回填，不被剥离改造", async () => {
    installFetch();
    routes.info = sendInfo();
    routes.queue = [queueItem({ id: 43, position: 1, content: "/permissions" })];
    render(<MessageComposer session={{ id: "sess-3" }} />);
    await screen.findByTestId("queue-row-43");
    fireEvent.click(screen.getByTestId("queue-edit"));
    await waitFor(() => expect(screen.queryByTestId("queue-row-43")).toBeNull());
    expect((screen.getByTestId("composer-input") as HTMLTextAreaElement).value).toBe(
      "/permissions"
    );
  });
});

// ==== 丁T3 接入② / 丁T6 复评：卡片在场分流（§2.4 裁3）====
// 四态矩阵：approve / questionFreeText / questionBlocked / none——各一条主用例。
describe("卡片在场分流：审批拦截 / 问答转向自由作答 / 不可作答拦截", () => {
  it("审批卡在场：发送被拦截（零 /session-send 调用）+ 回执提示用卡片按钮 + 输入保留", async () => {
    installFetch();
    routes.info = sendInfo();
    routes.approveOptions = { available: true };
    render(<MessageComposer session={{ id: "sess-1" }} />);
    const input = (await screen.findByTestId("composer-input")) as HTMLTextAreaElement;
    // 在场提示条（挂载探针落地后）
    await screen.findByTestId("composer-card-presence");
    expect(screen.getByTestId("composer-card-presence").getAttribute("data-presence")).toBe(
      "approve"
    );
    fireEvent.change(input, { target: { value: "帮我改一下" } });
    fireEvent.click(screen.getByTestId("composer-send"));
    const chip = await screen.findByTestId("send-receipt-blocked");
    expect(chip.textContent).toContain("终端等待审批，请用卡片按钮");
    // 零注入：session-send 未被调用（裁3 安全面——放行 = 误触选项/误批准）
    expect(sendCalls()).toHaveLength(0);
    // 拦截 ≠ 失败：不得渲染 failed（红 chip 带「可重试」会误导——重按仍会被拦）
    expect(screen.queryByTestId("send-receipt-failed")).toBeNull();
    // 输入保留（用户可复制到卡片输入框或终端）
    expect((screen.getByTestId("composer-input") as HTMLTextAreaElement).value).toBe("帮我改一下");
  });

  // ===== 丁T6 复评（契约 §2.4 裁3）：问答在场 + answerable + 单题 → **转向** freeText =====
  it("问答可自由作答（claude 单题单选）：发送转向 freeText 端点（非 sessionSend）+ 成功清输入框", async () => {
    installFetch();
    routes.info = sendInfo();
    routes.questionInfo = freeTextQuestionInfo();
    routes.answer = { status: "key_sent", done: true, stage: "free-text", verified: true };
    render(<MessageComposer session={{ id: "sess-1" }} />);
    const input = (await screen.findByTestId("composer-input")) as HTMLTextAreaElement;
    await screen.findByTestId("composer-card-presence");
    expect(screen.getByTestId("composer-card-presence").getAttribute("data-presence")).toBe(
      "questionFreeText"
    );
    expect(input.placeholder).toBe("输入内容将作为本题的回答发送");
    fireEvent.change(input, { target: { value: "构建产物放 dist" } });
    fireEvent.click(screen.getByTestId("composer-send"));
    // 回执：verified=true（走完整条闭环）→ delivered（与卡内路径同口径）
    expect(await screen.findByTestId("send-receipt-delivered")).toBeTruthy();
    // **断言调用参数含用户文本**（转向的核心契约：走的是同一条 freeText 出口）
    expect(answerCalls()).toHaveLength(1);
    expect(JSON.parse(String((answerCalls()[0][1] as RequestInit).body))).toEqual({
      sessionId: "sess-1",
      action: "freeText",
      text: "构建产物放 dist",
    });
    // 零 /session-send：转向路径**不得**走原直发（否则又落回「自由文本被读成选项」）
    expect(sendCalls()).toHaveLength(0);
    // 成功后清空输入框（与 delivered 同口径——留着会让用户以为没发出去）
    expect((screen.getByTestId("composer-input") as HTMLTextAreaElement).value).toBe("");
  });

  it("问答可自由作答但回执 verified=false：中性 submitted 回执（不冒充完成）+ 仍清输入框", async () => {
    installFetch();
    routes.info = sendInfo();
    routes.questionInfo = freeTextQuestionInfo();
    routes.answer = { status: "key_sent", done: true, stage: "free-text", verified: false };
    render(<MessageComposer session={{ id: "sess-1" }} />);
    const input = (await screen.findByTestId("composer-input")) as HTMLTextAreaElement;
    await screen.findByTestId("composer-card-presence");
    fireEvent.change(input, { target: { value: "回答内容" } });
    fireEvent.click(screen.getByTestId("composer-send"));
    // false = 「读到屏但未见终态锚」（不谎报完成）→ 走中性 submitted（D7/T3 同一纪律）
    expect(await screen.findByTestId("send-receipt-submitted")).toBeTruthy();
    expect(screen.queryByTestId("send-receipt-delivered")).toBeNull();
    expect(answerCalls()).toHaveLength(1);
    expect(sendCalls()).toHaveLength(0);
    expect((screen.getByTestId("composer-input") as HTMLTextAreaElement).value).toBe("");
  });

  it("问答多题：仍拦截（不调 freeText、不调 sessionSend），placeholder 不承诺发送", async () => {
    installFetch();
    routes.info = sendInfo();
    routes.questionInfo = {
      available: true,
      answerable: true,
      freeText: true,
      questions: [
        { question: "问题一", multiSelect: false },
        { question: "问题二", multiSelect: false },
      ],
    };
    render(<MessageComposer session={{ id: "sess-1" }} />);
    const input = (await screen.findByTestId("composer-input")) as HTMLTextAreaElement;
    // 提示去重（2026-10-10 用户指令）：questionBlocked 段不再渲染——同一条指引由
    // ModeBar 提示与输入框占位语各承载一次，不再三处堆叠
    expect(screen.queryByTestId("composer-card-presence")).toBeNull();
    expect(input.placeholder).toContain("本题请到卡片或终端作答");
    fireEvent.change(input, { target: { value: "回答" } });
    fireEvent.click(screen.getByTestId("composer-send"));
    const chip = await screen.findByTestId("send-receipt-blocked");
    expect(chip.textContent).toContain("本题不能在本输入框作答");
    expect(answerCalls()).toHaveLength(0);
    expect(sendCalls()).toHaveLength(0);
    expect((screen.getByTestId("composer-input") as HTMLTextAreaElement).value).toBe("回答");
  });

  it("问答 answerable=false（未验工具）：仍拦截（不调 freeText、不调 sessionSend）", async () => {
    installFetch();
    routes.info = sendInfo();
    routes.questionInfo = {
      available: true,
      answerable: false,
      freeText: false,
      questions: [{ question: "codex 的题", multiSelect: false }],
    };
    render(<MessageComposer session={{ id: "sess-1" }} />);
    const input = (await screen.findByTestId("composer-input")) as HTMLTextAreaElement;
    // 提示去重（2026-10-10 用户指令）：questionBlocked 段不再渲染——同一条指引由
    // ModeBar 提示与输入框占位语各承载一次，不再三处堆叠
    expect(screen.queryByTestId("composer-card-presence")).toBeNull();
    fireEvent.change(input, { target: { value: "回答" } });
    fireEvent.click(screen.getByTestId("composer-send"));
    await screen.findByTestId("send-receipt-blocked");
    expect(answerCalls()).toHaveLength(0);
    expect(sendCalls()).toHaveLength(0);
  });

  it("问答 freeText 未定案（answerable=true 但 freeText 缺省）：仍拦截（未验不出键）", async () => {
    installFetch();
    routes.info = sendInfo();
    // codex/opencode/kimi 形态：点选已验（answerable=true）但自由作答未定案
    routes.questionInfo = {
      available: true,
      answerable: true,
      questions: [{ question: "点选可答但自由作答未验", multiSelect: false }],
    };
    render(<MessageComposer session={{ id: "sess-1" }} />);
    const input = (await screen.findByTestId("composer-input")) as HTMLTextAreaElement;
    // 提示去重（2026-10-10 用户指令）：questionBlocked 段不再渲染——同一条指引由
    // ModeBar 提示与输入框占位语各承载一次，不再三处堆叠
    expect(screen.queryByTestId("composer-card-presence")).toBeNull();
    fireEvent.change(input, { target: { value: "回答" } });
    fireEvent.click(screen.getByTestId("composer-send"));
    await screen.findByTestId("send-receipt-blocked");
    expect(answerCalls()).toHaveLength(0);
    expect(sendCalls()).toHaveLength(0);
  });

  it("answerable=false 是**独立判据**：freeText=true 也仍拦截（未验不出键优先）", async () => {
    installFetch();
    routes.info = sendInfo();
    // 夹具刻意让 freeText=true（其他三条判据全过）——只有 answerable=false 拦住它。
    // 这是「answerable 守卫不可省」的**隔离锁**（若只测「freeText 缺省」那种夹具，
    // 把 answerable 条件删掉测试也不会红 = 空断言）
    routes.questionInfo = {
      available: true,
      answerable: false,
      freeText: true,
      questions: [{ question: "未验工具的题", multiSelect: false }],
    };
    render(<MessageComposer session={{ id: "sess-1" }} />);
    const input = (await screen.findByTestId("composer-input")) as HTMLTextAreaElement;
    // 提示去重（2026-10-10 用户指令）：questionBlocked 段不再渲染——同一条指引由
    // ModeBar 提示与输入框占位语各承载一次，不再三处堆叠
    expect(screen.queryByTestId("composer-card-presence")).toBeNull();
    fireEvent.change(input, { target: { value: "回答" } });
    fireEvent.click(screen.getByTestId("composer-send"));
    await screen.findByTestId("send-receipt-blocked");
    expect(answerCalls()).toHaveLength(0);
    expect(sendCalls()).toHaveLength(0);
  });

  it("answerable 缺省（旧后端兼容）：freeText=true 单题单选 → 走转向（缺省按 true）", async () => {
    installFetch();
    routes.info = sendInfo();
    // 旧后端不带 answerable 字段 → 按 true 处理（api.ts:610 注释的口径）。本用例锁
    // 「缺省语义」：若有人把判据写成 `=== true`（严格要求字段在场），本测试会红
    routes.questionInfo = {
      available: true,
      freeText: true,
      questions: [{ question: "旧后端的题", multiSelect: false }],
    };
    render(<MessageComposer session={{ id: "sess-1" }} />);
    const input = (await screen.findByTestId("composer-input")) as HTMLTextAreaElement;
    await screen.findByTestId("composer-card-presence");
    expect(screen.getByTestId("composer-card-presence").getAttribute("data-presence")).toBe(
      "questionFreeText"
    );
    fireEvent.change(input, { target: { value: "回答" } });
    fireEvent.click(screen.getByTestId("composer-send"));
    expect(await screen.findByTestId("send-receipt-delivered")).toBeTruthy();
    expect(answerCalls()).toHaveLength(1);
    expect(sendCalls()).toHaveLength(0);
  });

  it("问答多选题：仍拦截（多选屏自由作答行判据不匹配，后端恒 409）", async () => {
    installFetch();
    routes.info = sendInfo();
    routes.questionInfo = {
      available: true,
      answerable: true,
      freeText: true,
      questions: [{ question: "选哪些？", multiSelect: true }],
    };
    render(<MessageComposer session={{ id: "sess-1" }} />);
    const input = (await screen.findByTestId("composer-input")) as HTMLTextAreaElement;
    // 提示去重（2026-10-10 用户指令）：questionBlocked 段不再渲染——同一条指引由
    // ModeBar 提示与输入框占位语各承载一次，不再三处堆叠
    expect(screen.queryByTestId("composer-card-presence")).toBeNull();
    fireEvent.change(input, { target: { value: "回答" } });
    fireEvent.click(screen.getByTestId("composer-send"));
    await screen.findByTestId("send-receipt-blocked");
    expect(answerCalls()).toHaveLength(0);
  });

  it("转向自由作答 + 带附件：拦截并提示（**不静默丢附件**）", async () => {
    installFetch();
    routes.info = sendInfo();
    routes.questionInfo = freeTextQuestionInfo();
    routes.attach = { path: "E:/proj/.tuvis-attachments/s-1/1-a.png", size: 5 };
    render(<MessageComposer session={{ id: "sess-1" }} />);
    const input = await screen.findByTestId("composer-input");
    await screen.findByTestId("composer-card-presence");
    // 上传一个附件（走 + 钮同链路）
    fireEvent.change(screen.getByTestId("attach-file-input"), {
      target: { files: [new File(["x"], "a.png", { type: "image/png" })] },
    });
    await screen.findByTestId("attachment-chips");
    fireEvent.change(input, { target: { value: "带附件的回答" } });
    fireEvent.click(screen.getByTestId("composer-send"));
    const chip = await screen.findByTestId("send-receipt-blocked");
    expect(chip.textContent).toContain("附件不能随「回答」发送");
    // 零调用：既不发 freeText（会丢附件）也不发 sessionSend（自由文本会被读成选项）
    expect(answerCalls()).toHaveLength(0);
    expect(sendCalls()).toHaveLength(0);
  });

  it("转向 freeText 失败（failed 回执）：红 chip 展示后端 error 原文 + 输入保留可重试", async () => {
    installFetch();
    routes.info = sendInfo();
    routes.questionInfo = freeTextQuestionInfo();
    routes.answer = {
      status: "failed",
      aborted: true,
      stage: "free-row",
      error: "屏上未出现自由作答行",
    };
    render(<MessageComposer session={{ id: "sess-1" }} />);
    const input = (await screen.findByTestId("composer-input")) as HTMLTextAreaElement;
    await screen.findByTestId("composer-card-presence");
    fireEvent.change(input, { target: { value: "回答内容" } });
    fireEvent.click(screen.getByTestId("composer-send"));
    const chip = await screen.findByTestId("send-receipt-failed");
    expect(chip.textContent).toContain("屏上未出现自由作答行");
    expect(sendCalls()).toHaveLength(0);
    // 失败保留输入（与既有 failed 态同口径：可重试）
    expect((screen.getByTestId("composer-input") as HTMLTextAreaElement).value).toBe("回答内容");
  });

  // ===== 丁T6 复评：问答端点的错误码分诊（**字段名与 /session-send 不同**）=====
  // 问答端点的错误码在响应体 `error` 字段（`{error:"multi_questions"}`），而
  // `/session-send` 的失败细节在 `reason` 字段——两条路径**不能复用同一段读取**。
  // 下面三条锁住「按 `error` 读并映射成中文」（若误读 `reason`，会显示成
  // 「session-question/answer 409」这种对用户无意义的串）。
  it("转向 freeText 遇 409 multi_questions（TOCTOU：探针后终端换了多题）→ 中文分诊文案", async () => {
    installFetch();
    routes.info = sendInfo();
    routes.questionInfo = freeTextQuestionInfo();
    routes.answerStatus = 409;
    routes.answerBody = { error: "multi_questions" };
    render(<MessageComposer session={{ id: "sess-1" }} />);
    const input = (await screen.findByTestId("composer-input")) as HTMLTextAreaElement;
    await screen.findByTestId("composer-card-presence");
    fireEvent.change(input, { target: { value: "回答内容" } });
    fireEvent.click(screen.getByTestId("composer-send"));
    const chip = await screen.findByTestId("send-receipt-failed");
    expect(chip.textContent).toContain("多个问题请回到终端完成作答");
    expect(chip.textContent).not.toContain("409");
    // 失败保留输入（可重试）
    expect((screen.getByTestId("composer-input") as HTMLTextAreaElement).value).toBe("回答内容");
  });

  it("转向 freeText 遇 409 tool_readonly → 中文分诊文案（未验不出键）", async () => {
    installFetch();
    routes.info = sendInfo();
    routes.questionInfo = freeTextQuestionInfo();
    routes.answerStatus = 409;
    routes.answerBody = { error: "tool_readonly" };
    render(<MessageComposer session={{ id: "sess-1" }} />);
    const input = (await screen.findByTestId("composer-input")) as HTMLTextAreaElement;
    await screen.findByTestId("composer-card-presence");
    fireEvent.change(input, { target: { value: "回答内容" } });
    fireEvent.click(screen.getByTestId("composer-send"));
    const chip = await screen.findByTestId("send-receipt-failed");
    expect(chip.textContent).toContain("该工具的远程作答尚未实测，请在终端完成作答");
  });

  it("转向 freeText 遇 409 no_question（问答已消失）→ 中文分诊文案", async () => {
    installFetch();
    routes.info = sendInfo();
    routes.questionInfo = freeTextQuestionInfo();
    routes.answerStatus = 409;
    routes.answerBody = { error: "no_question" };
    render(<MessageComposer session={{ id: "sess-1" }} />);
    const input = (await screen.findByTestId("composer-input")) as HTMLTextAreaElement;
    await screen.findByTestId("composer-card-presence");
    fireEvent.change(input, { target: { value: "回答内容" } });
    fireEvent.click(screen.getByTestId("composer-send"));
    const chip = await screen.findByTestId("send-receipt-failed");
    expect(chip.textContent).toContain("当前没有待回答的问题");
  });

  it("两者都在场：审批优先（裁3 安全面更重——审批框放行自由文本 = 误触选项）", async () => {
    installFetch();
    routes.info = sendInfo();
    routes.approveOptions = { available: true, planPending: true };
    routes.questionInfo = freeTextQuestionInfo();
    render(<MessageComposer session={{ id: "sess-1" }} />);
    await screen.findByTestId("composer-card-presence");
    expect(screen.getByTestId("composer-card-presence").getAttribute("data-presence")).toBe(
      "approve"
    );
  });

  it("都不在场：placeholder 与发送行为完全不变（回归锁——分流不得污染正常路径）", async () => {
    installFetch();
    routes.info = sendInfo();
    routes.send = { status: "delivered" };
    render(<MessageComposer session={{ id: "sess-1" }} />);
    const input = (await screen.findByTestId("composer-input")) as HTMLTextAreaElement;
    expect(input.placeholder).toBe("输入消息发送到终端…");
    expect(screen.queryByTestId("composer-card-presence")).toBeNull();
    fireEvent.change(input, { target: { value: "普通消息" } });
    fireEvent.click(screen.getByTestId("composer-send"));
    expect(await screen.findByTestId("send-receipt-delivered")).toBeTruthy();
    expect(sendCalls()).toHaveLength(1);
    expect(answerCalls()).toHaveLength(0);
  });

  it("发送时刻复探（快照陈旧防线）：挂载时不在场，发送前对话框出现 → 本次发送被拦截", async () => {
    installFetch();
    routes.info = sendInfo();
    routes.send = { status: "delivered" };
    render(<MessageComposer session={{ id: "sess-1" }} />);
    const input = await screen.findByTestId("composer-input");
    // 挂载探针已落地（不在场）
    await flushAsync();
    expect(screen.queryByTestId("composer-card-presence")).toBeNull();
    // 对话框在挂载之后出现（模型刚提问）——快照变陈旧
    routes.approveOptions = { available: true };
    fireEvent.change(input, { target: { value: "这条不能直发" } });
    fireEvent.click(screen.getByTestId("composer-send"));
    const chip = await screen.findByTestId("send-receipt-blocked");
    expect(chip.textContent).toContain("终端等待审批");
    expect(sendCalls()).toHaveLength(0);
  });

  it("探针失败（非 2xx）→ 按不在场放行（能力缺失不阻断，与后端 blocks_control_injection 同裁决）", async () => {
    installFetch();
    routes.info = sendInfo();
    routes.send = { status: "delivered" };
    // 两个探针端点都返回非 2xx（api.ts 抛 ApiError → probeCardPresence 的 catch →
    // 「none」）→ 发送照常。裁决依据：能力缺失 ≠ 对话框在场，混同会让弱网下
    // 输入区永久失效，且回执文案会说假话（我们并不知道有没有对话框）。
    routes.approveOptionsStatus = 500;
    routes.questionStatus = 500;
    render(<MessageComposer session={{ id: "sess-1" }} />);
    const input = await screen.findByTestId("composer-input");
    fireEvent.change(input, { target: { value: "照发" } });
    fireEvent.click(screen.getByTestId("composer-send"));
    expect(await screen.findByTestId("send-receipt-delivered")).toBeTruthy();
    expect(sendCalls()).toHaveLength(1);
    expect(screen.queryByTestId("composer-card-presence")).toBeNull();
  });
});
