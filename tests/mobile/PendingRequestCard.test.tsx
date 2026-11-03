import { act, cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, beforeAll, beforeEach, describe, expect, it, vi } from "vitest";
import i18n from "@/i18n";
import PendingRequestCard, { CodexAvailabilityBanner } from "@/mobile/PendingRequestCard";
import type { ActionReceipt, PendingView, QuestionView } from "@/mobile/codexIpcApi";

// Task 9：原生 Pending 卡（command / permissions / user_input / unsupported）。
// 计划 Step 1 的断言逐条落地：
// - `getByText("仅可使用兼容模式")`（四档能力按服务端渲染）；
// - UNKNOWN_DELIVERY 无「重试」按钮（S5：不确定结果绝不自动重试）；
// - `postBody.requestKey === "n:9007199254740993"`（大整数 key 字符串无损）；
// - `postBody` 无 ownerClientId / targetClientId（S3：靶向由服务端解析）；
// - isSecret password 输入且完成清草稿；两题全答才可提交。
// fetch 全量 stub；测试语言固定 zh（文案断言用中文字面量）。

beforeAll(async () => {
  await i18n.changeLanguage("zh");
});

function commandPending(over: Partial<PendingView> = {}): PendingView {
  return {
    requestKey: "n:9007199254740993",
    contextToken: "tok-1",
    kind: "command_approval",
    actionable: true,
    disabledReasonCode: null,
    command: "rm -rf build/",
    cwd: "/tmp/proj",
    reason: "clean build dir",
    permissionSummary: null,
    questions: [],
    ...over,
  };
}

function permissionsPending(over: Partial<PendingView> = {}): PendingView {
  return {
    requestKey: "s:req-9",
    contextToken: "tok-2",
    kind: "permissions_approval",
    actionable: true,
    disabledReasonCode: null,
    command: null,
    cwd: null,
    reason: null,
    permissionSummary: "读取 ~/.ssh/id_rsa（scope: turn）",
    questions: [],
    ...over,
  };
}

function question(over: Partial<QuestionView> = {}): QuestionView {
  return {
    id: "q1",
    header: "Deploy",
    question: "部署到哪个环境？",
    isOther: false,
    isSecret: false,
    options: [
      { label: "Staging", description: "预发环境" },
      { label: "Production", description: "" },
    ],
    ...over,
  };
}

function userInputPending(questions: QuestionView[], over: Partial<PendingView> = {}): PendingView {
  return {
    requestKey: "n:12",
    contextToken: "tok-3",
    kind: "user_input",
    actionable: true,
    disabledReasonCode: null,
    command: null,
    cwd: null,
    reason: null,
    permissionSummary: null,
    questions,
    ...over,
  };
}

function receipt(delivery: string): ActionReceipt {
  return {
    actionId: "act-1",
    delivery,
    route: "desktop_ipc",
    turnId: "turn-1",
    envelopeId: "env-1",
    reasonCode: null,
  };
}

interface Routes {
  respond?: ActionReceipt;
  respondStatus?: number;
  respondBody?: Record<string, unknown>;
  /** R12：POST 挂起不响应（手动放行——「提交在飞时 token 轮换」窗口） */
  respondHang?: boolean;
  /** R12：POST 网络层异常（fetch 直接 reject——丢回执场景） */
  respondReject?: boolean;
  /** R12：GET /codex/action 只读核对载荷；"missing"=404；"reject"=网络异常 */
  codexAction?: Record<string, unknown> | "missing" | "reject";
  /** H11：GET /codex/action/verify 只读核对载荷（缺省 unresolved）。 */
  codexVerify?: Record<string, unknown> | "missing" | "reject";
}

function verifyView(over: Record<string, unknown> = {}): Record<string, unknown> {
  return {
    actionId: "act-1",
    stage: "settled",
    outcome: "unresolved",
    turnId: null,
    runLimitReleased: false,
    reasonCode: null,
    verifiedAt: 1,
    ...over,
  };
}

let routes: Routes;
let fetchMock: ReturnType<typeof vi.fn>;
/** respondHang 挂起请求的放行器（测试中手动 resolve 模拟旧响应迟到） */
let releaseRespond: ((r: Response) => void) | null = null;

beforeEach(() => {
  routes = {};
  releaseRespond = null;
});
afterEach(() => {
  window.sessionStorage.clear();
  cleanup();
  vi.unstubAllGlobals();
});

function installFetch() {
  fetchMock = vi.fn(async (input: RequestInfo | URL) => {
    const url = String(input);
    if (url.includes("/codex/pending-response")) {
      if (routes.respondHang) {
        return new Promise<Response>((resolve) => {
          releaseRespond = resolve;
        });
      }
      if (routes.respondReject) throw new TypeError("pending-response 网络断开（模拟丢回执）");
      if (routes.respondStatus) {
        return new Response(JSON.stringify(routes.respondBody ?? { error: "stale_context" }), {
          status: routes.respondStatus,
        });
      }
      return new Response(JSON.stringify(routes.respond ?? receipt("accepted")), { status: 200 });
    }
    if (url.includes("/codex/action/verify")) {
      if (routes.codexVerify === "reject") throw new TypeError("codex/action/verify 网络断开");
      if (routes.codexVerify === "missing") {
        return new Response(JSON.stringify({ error: "not_found" }), { status: 404 });
      }
      return new Response(JSON.stringify(routes.codexVerify ?? verifyView()), { status: 200 });
    }
    if (url.includes("/codex/action")) {
      // R12：fetchCodexAction 只读核对（缺省未终态 queued；"missing"=404 查不到）
      if (routes.codexAction === "reject") throw new TypeError("codex/action 网络断开");
      if (routes.codexAction === "missing") {
        return new Response(JSON.stringify({ error: "not_found" }), { status: 404 });
      }
      return new Response(
        JSON.stringify(
          routes.codexAction ?? {
            actionId: "act-1",
            state: "queued",
            position: null,
            receipt: null,
          }
        ),
        { status: 200 }
      );
    }
    throw new Error(`unexpected fetch: ${url}`);
  });
  vi.stubGlobal("fetch", fetchMock);
}

function respondBodies(): Record<string, unknown>[] {
  return fetchMock.mock.calls
    .filter(([u]) => String(u).includes("/codex/pending-response"))
    .map(([u, init]) => {
      expect(String(u).includes("POST") || init?.method === "POST").toBeTruthy();
      return JSON.parse(String(init?.body)) as Record<string, unknown>;
    });
}

describe("PendingRequestCard：command / permissions 两按钮", () => {
  it("command_two_buttons_wire_body：渲染命令原文与两按钮；批准 → approve 契约体", async () => {
    installFetch();
    routes.respond = receipt("accepted");
    const onReceipt = vi.fn();
    render(<PendingRequestCard sessionId="s1" request={commandPending()} onReceipt={onReceipt} />);
    expect(screen.getByText("rm -rf build/")).toBeInTheDocument();
    expect(screen.getByTestId("codex-pending-approve").textContent).toBe("批准");
    expect(screen.getByTestId("codex-pending-reject").textContent).toBe("拒绝");
    fireEvent.click(screen.getByTestId("codex-pending-approve"));
    const body = (await waitFor(() => respondBodies()))[0];
    // requestKey 永远是 string：大整数 2^53+ 无损（计划 Review Focus 3）
    expect(body.requestKey).toBe("n:9007199254740993");
    expect(typeof body.requestKey).toBe("string");
    // 靶向由服务端 Owner Registry 解析：客户端绝不回带 owner/target/raw method（S3）
    expect(body).not.toHaveProperty("ownerClientId");
    expect(body).not.toHaveProperty("targetClientId");
    expect(body.method).toBeUndefined();
    expect(body).toEqual({
      sessionId: "s1",
      actionId: expect.any(String),
      requestKey: "n:9007199254740993",
      contextToken: "tok-1",
      action: "approve",
      // H11：点击起算的前台剩余预算如实透传
      frontBudgetMs: expect.any(Number),
    });
    // 回执上行（Accepted 只表示已受理——不标 Turn 完成）
    await waitFor(() => expect(onReceipt).toHaveBeenCalledWith(receipt("accepted")));
    expect(screen.getByTestId("codex-pending-note").textContent).toContain("已受理");
    // 已受理后按钮加闸（防双答）
    expect((screen.getByTestId("codex-pending-approve") as HTMLButtonElement).disabled).toBe(true);
  });

  it("permissions_two_buttons：权限摘要渲染；拒绝 → action=reject 且 requestKey 字符串形态原样", async () => {
    installFetch();
    routes.respond = receipt("rejected");
    const onReceipt = vi.fn();
    render(
      <PendingRequestCard sessionId="s1" request={permissionsPending()} onReceipt={onReceipt} />
    );
    expect(screen.getByText("读取 ~/.ssh/id_rsa（scope: turn）")).toBeInTheDocument();
    fireEvent.click(screen.getByTestId("codex-pending-reject"));
    const body = (await waitFor(() => respondBodies()))[0];
    expect(body.requestKey).toBe("s:req-9");
    expect(body.action).toBe("reject");
    await waitFor(() =>
      expect(onReceipt).toHaveBeenCalledWith(expect.objectContaining({ delivery: "rejected" }))
    );
  });
});

describe("PendingRequestCard：user_input 阻塞问答", () => {
  it("tone：批准=approve 红卡、阻塞问答=question 蓝卡", () => {
    installFetch();
    const { unmount } = render(<PendingRequestCard sessionId="s1" request={commandPending()} />);
    expect(screen.getByTestId("codex-pending-card").getAttribute("data-tone")).toBe("approve");
    unmount();
    render(
      <PendingRequestCard
        sessionId="s1"
        request={userInputPending([question({ id: "q-tone", isOther: true })])}
      />
    );
    expect(screen.getByTestId("codex-pending-card").getAttribute("data-tone")).toBe("question");
  });

  it("two_questions_require_all_answers：两题全答才可提交；提交体按原 qid 映射", async () => {
    installFetch();
    const onReceipt = vi.fn();
    const q2 = question({
      id: "q2",
      header: "Region",
      question: "部署到哪个区域？",
      options: [
        { label: "us", description: "" },
        { label: "cn", description: "" },
      ],
    });
    render(
      <PendingRequestCard
        sessionId="s1"
        request={userInputPending([question(), q2])}
        onReceipt={onReceipt}
      />
    );
    // B04：只显示第 1 题；未答时不能前进
    expect(screen.getByTestId("codex-pending-progress").textContent).toContain("1／2");
    expect(screen.queryByTestId("codex-q-q2")).toBeNull();
    const next = screen.getByTestId("codex-pending-next") as HTMLButtonElement;
    expect(next.disabled).toBe(true);
    fireEvent.click(screen.getByTestId(`codex-q-q1-opt-0`));
    expect(next.disabled).toBe(false);
    fireEvent.click(next);
    // 第 2 题；可返回第 1 题修改
    expect(screen.getByTestId("codex-pending-progress").textContent).toContain("2／2");
    expect(screen.getByTestId("codex-q-q2")).toBeTruthy();
    fireEvent.click(screen.getByTestId(`codex-q-q2-opt-1`));
    fireEvent.click(screen.getByTestId("codex-pending-prev"));
    expect((screen.getByTestId("codex-q-q1-opt-0") as HTMLInputElement).checked).toBe(true);
    fireEvent.click(screen.getByTestId("codex-pending-next"));
    const submit = screen.getByTestId("codex-pending-submit") as HTMLButtonElement;
    expect(submit.textContent).toContain("提交全部答案");
    expect(submit.disabled).toBe(false);
    fireEvent.click(submit);
    const body = (await waitFor(() => respondBodies()))[0];
    expect(body.action).toBe("answer");
    // 原问题 ID 为键（不按显示顺序当 ID），答案 = label 数组
    expect(body.answers).toEqual({ q1: ["Staging"], q2: ["cn"] });
    expect(body.requestKey).toBe("n:12");
    expect(body.contextToken).toBe("tok-3");
    await waitFor(() => expect(onReceipt).toHaveBeenCalledTimes(1));
  });

  it("is_other_free_text：isOther=true 允许自由作答；isOther=false 只有选项（无自由输入框）", async () => {
    installFetch();
    const freeQ = question({
      id: "qf",
      question: "还有什么要补充？",
      isOther: true,
      options: [],
    });
    const strictQ = question({ id: "qs", question: "选一个", isOther: false });
    render(<PendingRequestCard sessionId="s1" request={userInputPending([freeQ, strictQ])} />);
    // isOther=true：自由文本框（非 secret → text）
    const free = screen.getByTestId("codex-q-qf-free") as HTMLInputElement;
    expect(free.type).toBe("text");
    fireEvent.change(free, { target: { value: "先跑冒烟再全量" } });
    fireEvent.click(screen.getByTestId("codex-pending-next"));
    // isOther=false：不能自由文本
    expect(screen.queryByTestId("codex-q-qs-free")).toBeNull();
    fireEvent.click(screen.getByTestId("codex-q-qs-opt-0"));
    fireEvent.click(screen.getByTestId("codex-pending-submit"));
    const body = (await waitFor(() => respondBodies()))[0];
    expect(body.answers).toEqual({ qf: ["先跑冒烟再全量"], qs: ["Staging"] });
  });

  it("is_secret_password_and_draft_cleared：isSecret 用 password 输入；提交成功后草稿清空（换卡前不残留）", async () => {
    installFetch();
    const secretQ = question({
      id: "qs",
      header: "Token",
      question: "输入访问令牌",
      isOther: true,
      isSecret: true,
      options: [],
    });
    render(<PendingRequestCard sessionId="s1" request={userInputPending([secretQ])} />);
    const input = screen.getByTestId("codex-q-qs-free") as HTMLInputElement;
    expect(input.type).toBe("password"); // 秘密输入不回显
    fireEvent.change(input, { target: { value: "sk-secret-123" } });
    fireEvent.click(screen.getByTestId("codex-pending-submit"));
    await waitFor(() => expect(respondBodies()).toHaveLength(1));
    // 完成后清草稿：密码不残留在 DOM（换卡/投影更新前也不残留）
    await waitFor(() =>
      expect((screen.getByTestId("codex-q-qs-free") as HTMLInputElement).value).toBe("")
    );
    const body = respondBodies()[0];
    expect(body.answers).toEqual({ qs: ["sk-secret-123"] });
  });
});

describe("PendingRequestCard：unsupported / 禁写 / 回执分诊", () => {
  it("unsupported_visible_not_operable：未知 method → 可见不可操作 + 「此请求暂不支持远程处理」", () => {
    installFetch();
    render(
      <PendingRequestCard
        sessionId="s1"
        request={commandPending({
          kind: "unsupported",
          actionable: false,
          disabledReasonCode: "capability_unavailable",
        })}
      />
    );
    expect(screen.getByTestId("codex-pending-card")).toBeInTheDocument();
    expect(screen.getByText("此请求暂不支持远程处理")).toBeInTheDocument();
    expect(screen.queryByTestId("codex-pending-approve")).toBeNull();
    expect(screen.queryByTestId("codex-pending-reject")).toBeNull();
  });

  it("unverified_capability_copy：已知但 profile 未验证 → 「当前 Desktop 版本未验证该操作」", () => {
    installFetch();
    render(
      <PendingRequestCard
        sessionId="s1"
        request={commandPending({ actionable: false, disabledReasonCode: "capability_unverified" })}
      />
    );
    expect(screen.getByText("当前 Desktop 版本未验证该操作")).toBeInTheDocument();
    expect(screen.queryByTestId("codex-pending-approve")).toBeNull();
  });

  it("unknown_delivery_no_retry_button：UNKNOWN 显示「投递结果未知，请在 Desktop 核对」，绝不出现自动重试按钮", async () => {
    installFetch();
    routes.respond = receipt("unknown_delivery");
    const onReceipt = vi.fn();
    render(<PendingRequestCard sessionId="s1" request={commandPending()} onReceipt={onReceipt} />);
    fireEvent.click(screen.getByTestId("codex-pending-approve"));
    await waitFor(() => expect(onReceipt).toHaveBeenCalledTimes(1));
    const note = screen.getByTestId("codex-pending-note");
    expect(note.textContent).toContain("投递结果未知");
    expect(note.textContent).toContain("Desktop");
    // S5 红线：不确定结果绝不提供自动重试入口
    expect(screen.queryByRole("button", { name: "重试" })).not.toBeInTheDocument();
    // 已发出的动作不得再点（防双答）
    expect((screen.getByTestId("codex-pending-approve") as HTMLButtonElement).disabled).toBe(true);
    expect((screen.getByTestId("codex-pending-reject") as HTMLButtonElement).disabled).toBe(true);
  });

  it("stale_context_409：过期上下文如实提示（不是网络错误），按钮恢复等新投影", async () => {
    installFetch();
    routes.respondStatus = 409;
    routes.respondBody = { error: "stale_context" };
    render(<PendingRequestCard sessionId="s1" request={commandPending()} />);
    fireEvent.click(screen.getByTestId("codex-pending-approve"));
    const note = await screen.findByTestId("codex-pending-note");
    expect(note.textContent).toContain("过期");
    // 409 后 busy 解除（用户可再点；权威修复来自新投影的新 token）
    await waitFor(() =>
      expect((screen.getByTestId("codex-pending-approve") as HTMLButtonElement).disabled).toBe(
        false
      )
    );
  });

  it("write_locked_stale：投影 stale（禁写）→ 卡可见但按钮全禁用", () => {
    installFetch();
    render(<PendingRequestCard sessionId="s1" request={commandPending()} writeLocked={true} />);
    expect(screen.getByTestId("codex-pending-card")).toBeInTheDocument();
    expect((screen.getByTestId("codex-pending-approve") as HTMLButtonElement).disabled).toBe(true);
    expect((screen.getByTestId("codex-pending-reject") as HTMLButtonElement).disabled).toBe(true);
  });

  it("token_or_owner_change_resets_state：requestKey 不变但 contextToken 变化 → 清旧草稿与回执（不误答）", async () => {
    installFetch();
    routes.respond = receipt("accepted");
    const { rerender } = render(
      <PendingRequestCard sessionId="s1" request={userInputPending([question({ id: "q1" })])} />
    );
    fireEvent.click(screen.getByTestId("codex-q-q1-opt-0"));
    // owner 换代 / token 变化：同 requestKey 新 token（服务端重发的同一请求）
    rerender(
      <PendingRequestCard
        sessionId="s1"
        request={userInputPending([question({ id: "q1" })], { contextToken: "tok-NEW" })}
      />
    );
    // N14：旧稿不自动成为新上下文答案——提交保持禁用，需显式「恢复旧稿」。
    expect((screen.getByTestId("codex-pending-submit") as HTMLButtonElement).disabled).toBe(true);
    expect(screen.getByTestId("codex-pending-incomplete")).toBeInTheDocument();
    const restore = screen.getByTestId("codex-pending-restore-draft");
    fireEvent.click(restore);
    await waitFor(() =>
      expect((screen.getByTestId("codex-pending-submit") as HTMLButtonElement).disabled).toBe(false)
    );
  });
});

// ============================================================
// C5 白名单补口（2026-10-19 复审）：POST /codex/pending-response **登记前**的
// 确定性拒绝码（答案限额五码 + action_mismatch）必须落「可重试错误」臂——
// 显示错误、ID 让位解锁、**绝不**进 unknown 核对臂（fetchCodexAction 必 404，
// 误入即永久锁卡）。
// ============================================================
describe("PendingRequestCard：投递前确定性拒绝（C5 白名单）", () => {
  async function flushAsync() {
    for (let i = 0; i < 6; i += 1) {
      await act(async () => {
        await Promise.resolve();
      });
    }
  }

  function postBodies(): Record<string, unknown>[] {
    return fetchMock.mock.calls
      .filter(([u]) => String(u).includes("/codex/pending-response"))
      .map(([, init]) => JSON.parse(String(init?.body)) as Record<string, unknown>);
  }

  function actionQueryCount(): number {
    return fetchMock.mock.calls.filter(([u]) => String(u).includes("/codex/action")).length;
  }

  it("validate_answers_400_is_retryable_error_not_unknown_lock：empty_answers 400 → 错误底注 + 解锁，不进核对臂", async () => {
    installFetch();
    routes.respondStatus = 400;
    routes.respondBody = { error: "empty_answers" };
    render(<PendingRequestCard sessionId="s1" request={commandPending()} />);
    fireEvent.click(screen.getByTestId("codex-pending-approve"));
    const note = await screen.findByTestId("codex-pending-note");
    // 确定性拒绝 = 可重试错误（不是「投递结果未知」）
    expect(note.textContent).toContain("应答失败");
    expect(note.textContent).not.toContain("投递结果未知");
    // ID 让位：按钮解锁（再按 = 新 actionId，零投递上重试安全）
    expect((screen.getByTestId("codex-pending-approve") as HTMLButtonElement).disabled).toBe(false);
    // 绝不进 unknown 核对臂：零次 GET /codex/action，且再等微任务也不补查
    expect(actionQueryCount()).toBe(0);
    await flushAsync();
    expect(actionQueryCount()).toBe(0);
    expect(postBodies()).toHaveLength(1);
  });

  it("action_mismatch_409_falls_through_to_pre_delivery_arm：409 action_mismatch → 同走投递前拒绝臂", async () => {
    installFetch();
    // 409 但不是 stale_context/action_conflict（那两码由专用臂处理）——
    // action_mismatch 必须穿过 409 臂落进 C5 白名单臂
    routes.respondStatus = 409;
    routes.respondBody = { error: "action_mismatch" };
    render(<PendingRequestCard sessionId="s1" request={commandPending()} />);
    fireEvent.click(screen.getByTestId("codex-pending-approve"));
    const note = await screen.findByTestId("codex-pending-note");
    expect(note.textContent).toContain("应答失败");
    expect(note.textContent).not.toContain("投递结果未知");
    expect((screen.getByTestId("codex-pending-reject") as HTMLButtonElement).disabled).toBe(false);
    expect(actionQueryCount()).toBe(0);
    await flushAsync();
    expect(actionQueryCount()).toBe(0);
  });

  it("capability_unavailable_503_is_pre_delivery_refusal：503 capability_unavailable（IPC 能力关，登记前零投递）→ 错误底注 + 解锁，不进 unknown 核对臂", async () => {
    installFetch();
    // 后端 `!hub.ipc_available()` → 503（登记之前即拒绝、零投递）——不进
    // 「结果未知」臂（否则 fetchCodexAction 必 404 → 锁卡到 token 轮换）
    routes.respondStatus = 503;
    routes.respondBody = { error: "capability_unavailable" };
    render(<PendingRequestCard sessionId="s1" request={commandPending()} />);
    fireEvent.click(screen.getByTestId("codex-pending-approve"));
    const note = await screen.findByTestId("codex-pending-note");
    expect(note.textContent).toContain("应答失败");
    expect(note.textContent).not.toContain("投递结果未知");
    // ID 让位：按钮解锁（再按 = 新 actionId，零投递上重试安全）
    expect((screen.getByTestId("codex-pending-approve") as HTMLButtonElement).disabled).toBe(false);
    // 绝不进 unknown 核对臂：零次 GET /codex/action，且再等微任务也不补查
    expect(actionQueryCount()).toBe(0);
    await flushAsync();
    expect(actionQueryCount()).toBe(0);
    expect(postBodies()).toHaveLength(1);
  });
});

describe("CodexAvailabilityBanner：四档能力按服务端渲染", () => {
  it("availability_labels_zh：四档各自渲染服务端下发的档位文案", () => {
    const { container } = render(
      <>
        <CodexAvailabilityBanner availability="controllable" stale={false} />
        <CodexAvailabilityBanner availability="send_only" stale={false} />
        <CodexAvailabilityBanner availability="compat_only" stale={false} />
        <CodexAvailabilityBanner availability="unavailable" stale={false} />
      </>
    );
    expect(screen.getByText("可控制")).toBeInTheDocument();
    expect(screen.getByText("可发送但不支持交互")).toBeInTheDocument();
    expect(screen.getByText("仅可使用兼容模式")).toBeInTheDocument();
    expect(screen.getByText("当前不可控制")).toBeInTheDocument();
    expect(container).toBeTruthy();
  });

  it("stale_note：stale → 保留视图说明条（禁写提示）", () => {
    render(<CodexAvailabilityBanner availability="controllable" stale={true} />);
    expect(screen.getByTestId("codex-stale-note").textContent).toContain("不能");
  });
});

// ============================================================
// R12：提交 ID 生命周期与响应守卫
// - 提交生成的 actionId 保持到「结果核对完成」：网络异常不换 ID 不解锁重投，
//   改用 fetchCodexAction 只读核对原 action（防同 request 借新 ID 双投）；
// - {requestKey, contextToken} 代际守卫：token 轮换后旧 Promise 的 resolve 不再
//   回写 receipt、不清新草稿、不覆盖新卡状态。
// ============================================================
describe("PendingRequestCard：提交 ID 与响应守卫（R12）", () => {
  async function flushAsync() {
    for (let i = 0; i < 6; i += 1) {
      await act(async () => {
        await Promise.resolve();
      });
    }
  }

  function respondBodies(): Record<string, unknown>[] {
    return fetchMock.mock.calls
      .filter(([u]) => String(u).includes("/codex/pending-response"))
      .map(([, init]) => JSON.parse(String(init?.body)) as Record<string, unknown>);
  }

  function actionQueryUrls(): string[] {
    return fetchMock.mock.calls
      .filter(([u]) => String(u).includes("/codex/action"))
      .map(([u]) => String(u));
  }

  it("pending_stale_response_discarded_after_token_rotation：提交在飞时 token 轮换 → 旧 resolve 不回写、不清新草稿、新卡可操作", async () => {
    installFetch();
    routes.respondHang = true;
    routes.respond = receipt("accepted");
    const onReceipt = vi.fn();
    const { rerender } = render(
      <PendingRequestCard
        sessionId="s1"
        request={userInputPending([question({ id: "q1" })])}
        onReceipt={onReceipt}
      />
    );
    // 提交在飞（旧 token tok-3）
    fireEvent.click(screen.getByTestId(`codex-q-q1-opt-0`));
    fireEvent.click(screen.getByTestId("codex-pending-submit"));
    await waitFor(() => expect(releaseRespond).toBeTruthy());
    // owner 换代：同 requestKey 新 token（新投影），草稿重置属正常换代防线
    rerender(
      <PendingRequestCard
        sessionId="s1"
        request={userInputPending([question({ id: "q1" })], { contextToken: "tok-ROTATED" })}
        onReceipt={onReceipt}
      />
    );
    // 新上下文重新作答（新草稿）
    fireEvent.click(screen.getByTestId(`codex-q-q1-opt-0`));
    const submit = screen.getByTestId("codex-pending-submit") as HTMLButtonElement;
    expect(submit.disabled).toBe(false);
    // 旧 Promise 迟到（accepted 回执）
    await act(async () => {
      releaseRespond?.(new Response(JSON.stringify(receipt("accepted")), { status: 200 }));
      await Promise.resolve();
    });
    await flushAsync();
    // 旧响应不得回写新卡：无回执底注、新草稿未被清、按钮仍可操作、onReceipt 不上行
    expect(screen.queryByTestId("codex-pending-note")).toBeNull();
    expect((screen.getByTestId(`codex-q-q1-opt-0`) as HTMLInputElement).checked).toBe(true);
    expect((screen.getByTestId("codex-pending-submit") as HTMLButtonElement).disabled).toBe(false);
    expect(onReceipt).not.toHaveBeenCalled();
  });

  it("pending_network_failure_keeps_action_id_and_polls：POST 网络失败 → 保留同 ID 恰一次查询核对，不自动重投", async () => {
    installFetch();
    routes.respondReject = true;
    // 核对查询：未终态（queued）→ 如实显示「投递结果未知」
    routes.codexAction = { actionId: "act-9", state: "queued", position: null, receipt: null };
    render(<PendingRequestCard sessionId="s1" request={commandPending()} />);
    fireEvent.click(screen.getByTestId("codex-pending-approve"));
    await flushAsync();
    // 恰一次 POST、恰一次只读核对，且核对的是 POST 携带的同一个 actionId
    const bodies = respondBodies();
    expect(bodies).toHaveLength(1);
    const queries = actionQueryUrls();
    expect(queries).toHaveLength(1);
    expect(queries[0]).toContain(String(bodies[0].actionId));
    // 未终态 → 结果未知底注 + 动作锁定（绝不借新 ID 重投）
    const note = screen.getByTestId("codex-pending-note");
    expect(note.textContent).toContain("投递结果未知");
    expect((screen.getByTestId("codex-pending-approve") as HTMLButtonElement).disabled).toBe(true);
    expect((screen.getByTestId("codex-pending-reject") as HTMLButtonElement).disabled).toBe(true);
    // 再等若干微任务也绝不自动重投
    await flushAsync();
    expect(respondBodies()).toHaveLength(1);
  });

  it("proxy_504_keeps_same_id_and_reconciles_accepted：POST 504（代理在服务端处理后返回）→ 不解锁重按，沿同 ID 只读核对到 accepted", async () => {
    installFetch();
    // 504 + 非我方机器码错误体 → 不可判零投递（C5：与无响应同——结果未知）
    routes.respondStatus = 504;
    routes.respondBody = { error: "gateway_timeout" };
    // 核对：已投递（服务端确实处理了）
    routes.codexVerify = verifyView({
      actionId: "act-504",
      outcome: "delivered",
      turnId: "t1",
    });
    render(<PendingRequestCard sessionId="s1" request={commandPending()} />);
    fireEvent.click(screen.getByTestId("codex-pending-approve"));
    await flushAsync();
    // 恰一次 POST；沿同 ID 恰一次只读核对
    const bodies = respondBodies();
    expect(bodies).toHaveLength(1);
    const queries = actionQueryUrls();
    expect(queries).toHaveLength(1);
    expect(queries[0]).toContain(String(bodies[0].actionId));
    // 核对到 accepted → 按 accepted 呈现 + 锁卡（绝不借新 ID 重投——IPC write 恒 1）
    expect(screen.getByTestId("codex-pending-note").textContent).toContain("已受理");
    expect((screen.getByTestId("codex-pending-approve") as HTMLButtonElement).disabled).toBe(true);
    await flushAsync();
    expect(respondBodies()).toHaveLength(1);
  });

  it("reconciled_not_sent_unlocks_new_action：核对到 not_sent（明确零投递）→ 底注「未发送」、按钮恢复（再按=新 actionId），绝不按 rejected 锁死", async () => {
    installFetch();
    routes.respondStatus = 504;
    routes.respondBody = {};
    // 账本：明确零投递（stage=settled + not_delivered）
    routes.codexVerify = verifyView({
      actionId: "act-ns",
      outcome: "not_delivered",
      reasonCode: "stale_context",
    });
    const onReceipt = vi.fn();
    render(<PendingRequestCard sessionId="s1" request={commandPending()} onReceipt={onReceipt} />);
    fireEvent.click(screen.getByTestId("codex-pending-approve"));
    await flushAsync();
    expect(respondBodies()).toHaveLength(1);
    // C5：尊重原始回执语义——not_sent 不是 rejected，不锁卡
    expect(screen.getByTestId("codex-pending-note").textContent).toContain("未发送");
    const approve = screen.getByTestId("codex-pending-approve") as HTMLButtonElement;
    expect(approve.disabled).toBe(false);
    // 再按 = 新动作新 ID（零投递上重试安全）
    routes.respondStatus = undefined;
    routes.respondBody = undefined;
    routes.respond = receipt("accepted");
    fireEvent.click(approve);
    await flushAsync();
    const bodies = respondBodies();
    expect(bodies).toHaveLength(2);
    expect(String(bodies[1].actionId)).not.toBe(String(bodies[0].actionId));
  });
});

// ============================================================
// S5：schema 证据晚到原地出按钮 + 输入草稿不丢 + 可发送时横幅不宣称「暂不能远程发送」。
// - schema 晚到（S1 republish 推送新投影）：同 requestKey 同 token、actionable 翻转
//   true → 按钮原地出现（组件不重挂），已输入草稿原样保留；
// - 可发送判据唯一来源 = 发送静态门（send-info 单点，SessionDetail 下发）——横幅
//   不得在可尝试时宣称「暂不能远程发送」（设计 §3.4）。
// ============================================================
describe("PendingRequestCard：H13 答案计数与 H11 未投递保稿", () => {
  async function flushAsync() {
    for (let i = 0; i < 6; i += 1) {
      await act(async () => {
        await Promise.resolve();
      });
    }
  }
  function answersOf(body: Record<string, unknown>): Record<string, string[]> {
    return (body.answers ?? {}) as Record<string, string[]>;
  }

  it("h13_single_value_text_replaces_selected_option", async () => {
    installFetch();
    routes.respond = receipt("accepted");
    render(
      <PendingRequestCard
        sessionId="s1"
        request={userInputPending([
          question({
            id: "q1",
            options: [{ label: "A", description: "" }],
            isOther: true,
            answerPolicy: { allowMultiple: false, maxAnswers: 1, allowFreeText: true },
          }),
        ])}
      />
    );
    fireEvent.click(screen.getByTestId("codex-q-q1-opt-0"));
    fireEvent.change(screen.getByTestId("codex-q-q1-free"), { target: { value: "自由作答" } });
    fireEvent.click(screen.getByTestId("codex-pending-submit"));
    await flushAsync();
    expect(answersOf(respondBodies()[0]).q1).toEqual(["自由作答"]);
  });

  it("h13_multi_limit_counts_free_text", async () => {
    installFetch();
    routes.respond = receipt("accepted");
    render(
      <PendingRequestCard
        sessionId="s1"
        request={userInputPending([
          question({
            id: "q1",
            options: [
              { label: "A", description: "" },
              { label: "B", description: "" },
              { label: "C", description: "" },
            ],
            isOther: true,
            answerPolicy: { allowMultiple: true, maxAnswers: 2, allowFreeText: true },
          }),
        ])}
      />
    );
    fireEvent.click(screen.getByTestId("codex-q-q1-opt-0"));
    fireEvent.change(screen.getByTestId("codex-q-q1-free"), { target: { value: "补充" } });
    // 已满 2（选项 + 文本）→ 再勾第三个不得超额
    fireEvent.click(screen.getByTestId("codex-q-q1-opt-1"));
    fireEvent.click(screen.getByTestId("codex-pending-submit"));
    await flushAsync();
    const answers = answersOf(respondBodies()[0]).q1;
    expect(answers).toHaveLength(2);
    expect(answers).toEqual(["A", "补充"]);
  });

  it("h11_not_sent_keeps_the_whole_answer_table", async () => {
    installFetch();
    routes.respond = { ...receipt("not_sent"), reasonCode: "busy_before_write" };
    render(
      <PendingRequestCard
        sessionId="s1"
        request={userInputPending([
          question({ id: "q1", options: [{ label: "A", description: "" }] }),
        ])}
      />
    );
    fireEvent.click(screen.getByTestId("codex-q-q1-opt-0"));
    fireEvent.click(screen.getByTestId("codex-pending-submit"));
    await flushAsync();
    expect(screen.getByTestId("codex-pending-note").textContent).toContain("未发送");
    expect((screen.getByTestId("codex-q-q1-opt-0") as HTMLInputElement).checked).toBe(true);
  });
});

describe("PendingRequestCard：schema 晚到原地出按钮与草稿保留（S5）", () => {
  function freeTextInputPending(over: Partial<PendingView> = {}): PendingView {
    return userInputPending([question({ id: "q1", isOther: true, options: [] })], {
      actionable: true,
      disabledReasonCode: null,
      ...over,
    });
  }

  it("schema_late_unlock：未解锁（无作答入口）→ 原地出提交按钮（同 key 同 token 翻转，不重挂）", async () => {
    installFetch();
    const { rerender } = render(
      <PendingRequestCard
        sessionId="s1"
        request={freeTextInputPending({
          actionable: false,
          disabledReasonCode: "capability_unverified",
        })}
      />
    );
    // 未解锁：无提交按钮，有禁用原因
    expect(screen.queryByTestId("codex-pending-submit")).toBeNull();
    expect(screen.getByTestId("codex-pending-disabled").textContent).toContain("未验证");
    // schema 证据晚到（S1 republish）：同 key 同 token，actionable 翻转 → 原地出按钮
    rerender(<PendingRequestCard sessionId="s1" request={freeTextInputPending()} />);
    expect(screen.getByTestId("codex-pending-submit")).toBeTruthy();
    expect(screen.queryByTestId("codex-pending-disabled")).toBeNull();
  });

  it("draft_survives_projection_update：投影更新（同 key 同 token）不丢已输入草稿；token 轮换才清草稿", async () => {
    installFetch();
    const { rerender } = render(
      <PendingRequestCard sessionId="s1" request={freeTextInputPending()} />
    );
    const draft = screen.getByTestId("codex-q-q1-free") as HTMLInputElement;
    fireEvent.change(draft, { target: { value: "采用方案 B 部署" } });
    expect(draft.value).toBe("采用方案 B 部署");
    // 投影例行更新（同 key 同 token，载荷细节变化）：草稿原样保留
    rerender(
      <PendingRequestCard
        sessionId="s1"
        request={freeTextInputPending({ reason: "clean build dir (updated)" })}
      />
    );
    expect((screen.getByTestId("codex-q-q1-free") as HTMLInputElement).value).toBe(
      "采用方案 B 部署"
    );
    // N14：token 轮换后旧非 secret 稿只作为「待确认」存在，不自动回填。
    rerender(
      <PendingRequestCard
        sessionId="s1"
        request={freeTextInputPending({ contextToken: "tok-ROTATED" })}
      />
    );
    expect((screen.getByTestId("codex-q-q1-free") as HTMLInputElement).value).toBe("");
    fireEvent.click(screen.getByTestId("codex-pending-restore-draft"));
    await waitFor(() =>
      expect((screen.getByTestId("codex-q-q1-free") as HTMLInputElement).value).toBe(
        "采用方案 B 部署"
      )
    );
  });
});

describe("CodexAvailabilityBanner：可发送时不宣称「暂不能远程发送」（S5）", () => {
  it("banner_send_attemptable：read_only + 静态门放行 → 横幅不再含「暂不能远程发送」；静态门拒绝或未取得 → 维持原文案", () => {
    // 静态门放行（send-info 单点判据下发）→ 不得出现「暂不能远程发送」
    const { unmount } = render(
      <CodexAvailabilityBanner availability="read_only" stale={false} sendAttemptable={true} />
    );
    const label = screen.getByTestId("codex-availability-label");
    expect(label.textContent).not.toContain("暂不能远程发送");
    expect(label.textContent).toContain("自动适配");
    unmount();

    // 静态门拒绝（capability_unverified/unavailable）→ 维持原「暂不能远程发送」文案
    render(
      <CodexAvailabilityBanner availability="read_only" stale={false} sendAttemptable={false} />
    );
    expect(screen.getByTestId("codex-availability-label").textContent).toContain("暂不能远程发送");
    cleanup();

    // 未取得（旧宿主/拉取失败）→ 维持原文案（不猜）
    render(<CodexAvailabilityBanner availability="read_only" stale={false} />);
    expect(screen.getByTestId("codex-availability-label").textContent).toContain("暂不能远程发送");
  });
});

// ============================================================
// R08：「拒绝并中止本轮」必须发 cancel（而非 reject）
// ============================================================

describe("R08 cancel 语义", () => {
  it("r08_cancel_button_sends_cancel_word", async () => {
    installFetch();
    const { fireEvent } = await import("@testing-library/react");
    render(
      <PendingRequestCard
        sessionId="s1"
        request={commandPending({ allowedDecisions: ["accept", "cancel"] })}
      />
    );
    const cancel = screen.getByTestId("codex-pending-cancel-turn");
    fireEvent.click(cancel);
    await waitFor(() => expect(respondBodies()[0]).toBeTruthy());
    expect(respondBodies()[0].action).toBe("cancel");
    expect(respondBodies()[0].action).not.toBe("reject");
  });

  it("r08_decline_and_cancel_are_distinct_when_both_offered", async () => {
    installFetch();
    // 明确零投递 → 卡片不锁定，可按第二个动作（观察两个真实请求体）
    routes.respond = receipt("not_sent");
    const { fireEvent } = await import("@testing-library/react");
    render(
      <PendingRequestCard
        sessionId="s1"
        request={commandPending({ allowedDecisions: ["decline", "cancel"] })}
      />
    );
    fireEvent.click(screen.getByTestId("codex-pending-reject"));
    await waitFor(() => expect(respondBodies().length).toBe(1));
    fireEvent.click(screen.getByTestId("codex-pending-cancel-turn"));
    await waitFor(() => expect(respondBodies().length).toBe(2));
    const words = respondBodies().map((body) => body.action);
    expect(words).toEqual(["reject", "cancel"]);
  });

  it("r08_no_cancel_button_without_the_word", () => {
    installFetch();
    render(
      <PendingRequestCard
        sessionId="s1"
        request={commandPending({ allowedDecisions: ["accept", "decline"] })}
      />
    );
    expect(screen.queryByTestId("codex-pending-cancel-turn")).toBeNull();
  });
});

// ============================================================
// R14：答策（单/多值、数量上限、自由文本）
// ============================================================

describe("R14 阻塞问答 answerPolicy", () => {
  const baseQuestions = [
    {
      id: "q1",
      header: "",
      question: "选一个/多个",
      isOther: true,
      isSecret: false,
      options: [
        { label: "A", description: "" },
        { label: "B", description: "" },
      ],
    },
  ];

  it("r14_single_value_policy_replaces_previous_choice", async () => {
    installFetch();
    const { fireEvent } = await import("@testing-library/react");
    const p = userInputPending([
      {
        ...baseQuestions[0],
        answerPolicy: { allowMultiple: false, maxAnswers: 1, allowFreeText: true },
      },
    ]);
    render(<PendingRequestCard sessionId="s1" request={p} />);
    const a = screen.getByTestId("codex-q-q1-opt-0") as HTMLInputElement;
    const b = screen.getByTestId("codex-q-q1-opt-1") as HTMLInputElement;
    expect(a.type).toBe("radio");
    fireEvent.click(a);
    fireEvent.click(b);
    expect((screen.getByTestId("codex-q-q1-opt-0") as HTMLInputElement).checked).toBe(false);
    expect((screen.getByTestId("codex-q-q1-opt-1") as HTMLInputElement).checked).toBe(true);
  });

  it("r14_multi_value_policy_respects_max_answers", async () => {
    installFetch();
    const { fireEvent } = await import("@testing-library/react");
    const p = userInputPending([
      {
        ...baseQuestions[0],
        answerPolicy: { allowMultiple: true, maxAnswers: 2, allowFreeText: false },
      },
    ]);
    render(<PendingRequestCard sessionId="s1" request={p} />);
    const [a, b] = ["0", "1"].map(
      (i) => screen.getByTestId(`codex-q-q1-opt-${i}`) as HTMLInputElement
    );
    expect(a.type).toBe("checkbox");
    fireEvent.click(a);
    fireEvent.click(b);
    expect((screen.getByTestId("codex-q-q1-opt-0") as HTMLInputElement).checked).toBe(true);
    expect((screen.getByTestId("codex-q-q1-opt-1") as HTMLInputElement).checked).toBe(true);
    // allowFreeText=false → 不渲染自由输入
    expect(screen.queryByTestId("codex-q-q1-free")).toBeNull();
  });

  it("r14_missing_policy_defaults_to_single_and_hides_free_text_when_unavailable", () => {
    installFetch();
    const p = userInputPending([{ ...baseQuestions[0], isOther: false }]);
    render(<PendingRequestCard sessionId="s1" request={p} />);
    expect((screen.getByTestId("codex-q-q1-opt-0") as HTMLInputElement).type).toBe("radio");
    expect(screen.queryByTestId("codex-q-q1-free")).toBeNull();
  });
});

describe("PendingRequestCard：H11 截止归属与核对出口", () => {
  async function flushAsync() {
    for (let i = 0; i < 6; i += 1) {
      await act(async () => {
        await Promise.resolve();
      });
    }
  }

  it("h11_definitive_refusal_clears_own_timer_and_does_not_relock", async () => {
    vi.useFakeTimers();
    installFetch();
    routes.respondStatus = 403;
    routes.respondBody = { error: "forbidden" };
    render(<PendingRequestCard sessionId="s1" request={commandPending()} />);
    fireEvent.click(screen.getByTestId("codex-pending-approve"));
    await act(async () => {
      await Promise.resolve();
    });
    expect(screen.getByTestId("codex-pending-note").textContent).toBeTruthy();
    // 30 秒后旧 timer 不得把卡反锁成 Unknown
    await act(async () => {
      vi.advanceTimersByTime(30_000);
    });
    expect(screen.queryByTestId("codex-pending-verify")).toBeNull();
    vi.useRealTimers();
  });

  it("h11_http_unknown_keeps_action_anchor_and_can_reverify", async () => {
    installFetch();
    routes.respond = receipt("unknown_delivery");
    routes.codexVerify = verifyView({ outcome: "unresolved" });
    render(<PendingRequestCard sessionId="s1" request={commandPending()} />);
    fireEvent.click(screen.getByTestId("codex-pending-approve"));
    await flushAsync();
    // 结果未知：保留 ID → 有再次核对入口
    const verify = await screen.findByTestId("codex-pending-verify");
    expect(respondBodies()).toHaveLength(1);
    // 核对到 delivered → 按 accepted 呈现，且不产生第二次 POST
    routes.codexVerify = verifyView({ outcome: "delivered", turnId: "t1" });
    fireEvent.click(verify);
    await flushAsync();
    expect(screen.getByTestId("codex-pending-note").textContent).toContain("已受理");
    expect(respondBodies()).toHaveLength(1);
  });

  it("h11_accepted_keeps_non_secret_drafts_for_review", async () => {
    installFetch();
    routes.respond = receipt("accepted");
    render(
      <PendingRequestCard
        sessionId="s1"
        request={userInputPending([question({ id: "q1", isSecret: false, isOther: true })])}
      />
    );
    fireEvent.change(screen.getByTestId("codex-q-q1-free"), { target: { value: "我的答案" } });
    fireEvent.click(screen.getByTestId("codex-pending-submit"));
    await flushAsync();
    expect((screen.getByTestId("codex-q-q1-free") as HTMLInputElement).value).toBe("我的答案");
  });
});

describe("PendingRequestCard：第四轮前端拥有者（N10/N11/N14）", () => {
  async function flushAsync() {
    for (let i = 0; i < 8; i += 1) {
      await act(async () => {
        await Promise.resolve();
      });
    }
  }

  it("n11_unconfirmed_acceptance_keeps_anchor_and_verify_entry", async () => {
    installFetch();
    routes.respond = { ...receipt("accepted"), reasonCode: "decision_unconfirmed" };
    render(<PendingRequestCard sessionId="s1" request={commandPending()} />);
    fireEvent.click(screen.getByTestId("codex-pending-approve"));
    await flushAsync();
    // 受理未确认：保留原动作锚 → 可再次只读核对（不新发）
    expect(screen.getByTestId("codex-pending-verify")).toBeTruthy();
    expect(respondBodies()).toHaveLength(1);
  });

  it("n11_anchor_survives_remount_without_new_post", async () => {
    installFetch();
    routes.respond = receipt("unknown_delivery");
    const { unmount } = render(<PendingRequestCard sessionId="s1" request={commandPending()} />);
    fireEvent.click(screen.getByTestId("codex-pending-approve"));
    await flushAsync();
    expect(screen.getByTestId("codex-pending-verify")).toBeTruthy();
    const posts = respondBodies().length;
    unmount();
    render(<PendingRequestCard sessionId="s1" request={commandPending()} />);
    // 重挂后仍能核对原动作（锚来自 sessionStorage），零新 POST
    expect(await screen.findByTestId("codex-pending-verify")).toBeTruthy();
    expect(respondBodies()).toHaveLength(posts);
  });

  it("n14_secret_draft_never_leaks_into_new_plain_question", async () => {
    installFetch();
    routes.respond = receipt("accepted");
    const { rerender } = render(
      <PendingRequestCard
        sessionId="s1"
        request={userInputPending([question({ id: "q1", isSecret: true, isOther: true })])}
      />
    );
    fireEvent.change(screen.getByTestId("codex-q-q1-free"), { target: { value: "旧密码" } });
    // 新 token 下同 qid 变成普通题：旧 secret 不得回填
    rerender(
      <PendingRequestCard
        sessionId="s1"
        request={userInputPending([question({ id: "q1", isSecret: false, isOther: true })], {
          contextToken: "tok-SECRET-ROTATED",
        })}
      />
    );
    expect((screen.getByTestId("codex-q-q1-free") as HTMLInputElement).value).toBe("");
    expect(screen.queryByTestId("codex-pending-restore-draft")).toBeNull();
  });

  it("n10_old_round_late_result_cannot_touch_new_round", async () => {
    installFetch();
    let releaseA: ((r: Response) => void) | null = null;
    let call = 0;
    fetchMock.mockImplementation(async (input: RequestInfo | URL) => {
      const url = String(input);
      if (url.includes("/codex/action/verify")) {
        return new Response(
          JSON.stringify(verifyView({ outcome: "not_delivered", reasonCode: "stale_context" })),
          { status: 200 }
        );
      }
      if (url.includes("/codex/pending-response")) {
        call += 1;
        if (call === 1) {
          return new Promise<Response>((resolve) => {
            releaseA = resolve;
          });
        }
        return new Response(JSON.stringify(receipt("unknown_delivery")), { status: 200 });
      }
      return new Response(JSON.stringify({ items: [] }), { status: 200 });
    });
    vi.useFakeTimers();
    render(<PendingRequestCard sessionId="s1" request={commandPending()} />);
    fireEvent.click(screen.getByTestId("codex-pending-approve")); // A 挂起
    await act(async () => {
      vi.advanceTimersByTime(30_000); // A 本地截止 → Unknown
    });
    // 核对确认 A 未投递 → 锚退休，用户开始 B 轮
    fireEvent.click(screen.getByTestId("codex-pending-verify"));
    await act(async () => {
      await Promise.resolve();
      await Promise.resolve();
    });
    fireEvent.click(screen.getByTestId("codex-pending-approve"));
    await act(async () => {
      await Promise.resolve();
    });
    expect(call).toBe(2);
    const anchorAfterB = screen.queryByTestId("codex-pending-verify");
    // A 的迟到 accepted 回执：不得把 B 改成已受理、不得清 B 的锚/稿
    await act(async () => {
      releaseA?.(new Response(JSON.stringify(receipt("accepted")), { status: 200 }));
      await Promise.resolve();
    });
    expect(screen.getByTestId("codex-pending-note").textContent).not.toContain("已受理");
    expect(screen.queryByTestId("codex-pending-verify") !== null).toBe(anchorAfterB !== null);
    vi.useRealTimers();
  });
});
