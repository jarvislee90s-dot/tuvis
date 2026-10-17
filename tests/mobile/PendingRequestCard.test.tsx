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
    const submit = screen.getByTestId("codex-pending-submit") as HTMLButtonElement;
    expect(submit.textContent).toContain("提交回答");
    // 未答全：提交禁用（防静默丢题）+ 未答题面可见
    expect(submit.disabled).toBe(true);
    expect(screen.getByTestId("codex-pending-incomplete").textContent).toContain("2");
    // 第一题点选项（点击即选）
    fireEvent.click(screen.getByTestId(`codex-q-q1-opt-0`));
    expect(submit.disabled).toBe(true); // 还差一题
    fireEvent.click(screen.getByTestId(`codex-q-q2-opt-1`));
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
    // isOther=false：不能自由文本
    expect(screen.queryByTestId("codex-q-qs-free")).toBeNull();
    // isOther=true：自由文本框（非 secret → text）
    const free = screen.getByTestId("codex-q-qf-free") as HTMLInputElement;
    expect(free.type).toBe("text");
    fireEvent.change(free, { target: { value: "先跑冒烟再全量" } });
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
    // 旧选择被清空：提交回到禁用态（旧草稿绝不灌进新上下文）
    expect((screen.getByTestId("codex-pending-submit") as HTMLButtonElement).disabled).toBe(true);
    expect(screen.getByTestId("codex-pending-incomplete")).toBeInTheDocument();
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
    // 核对：账本已 accepted（服务端确实处理了）
    routes.codexAction = {
      actionId: "act-504",
      state: "accepted",
      position: null,
      receipt: {
        actionId: "act-504",
        delivery: "accepted",
        route: "desktop_ipc",
        turnId: "t1",
        envelopeId: "e1",
        reasonCode: null,
      },
    };
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
    // 账本：state=rejected 但原始回执 delivery=not_sent（DAO 同状态承载两语义）
    routes.codexAction = {
      actionId: "act-ns",
      state: "rejected",
      position: null,
      receipt: {
        actionId: "act-ns",
        delivery: "not_sent",
        route: "desktop_ipc",
        turnId: null,
        envelopeId: null,
        reasonCode: "stale_context",
      },
    };
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
