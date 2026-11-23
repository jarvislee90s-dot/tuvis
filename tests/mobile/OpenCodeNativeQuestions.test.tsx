// 任务 10 红测：OpenCode 原生整组问答与审批（组内草稿 / 必答与跳过 / 冻结 /
// 选项漂移待核对 / 409 只呈现原生结果 / 未支持表单只读 / 审批点击即确认）。
//
// 断言口径（与 shared-contract「任务 0 回填的合同修订」1—2 条同一份）：
// - 合法跳过 = **省略键**；空串绝不作为值发出（对 required 字段是 400）；
// - 必答 = `required === true`；`required` 缺席/false 都不是必答；
// - 提交的是原生 `value`，展示 `label` 永不参与提交；
// - `Unsupported` 字段 ⇒ 整组只读、零 POST，不挑可答字段先发一次；
// - 409 / settled 只呈现原生结果，**任何文案都不得归因「哪一端获胜/本端已成功」**。
//
// 「零 POST」的口径：本文件统计**所有** fetch POST（不只看 /opencode/actions），
// 这样任何旧终端写入口的误调用都会让用例变红。
import userEvent from "@testing-library/user-event";
import { act, cleanup, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { afterEach, afterAll, beforeAll, beforeEach, describe, expect, it, vi } from "vitest";
import OpenCodeQuestionGroupCard from "@/mobile/OpenCodeQuestionGroupCard";
import OpenCodeNativePanel from "@/mobile/OpenCodeNativePanel";
import { readActionAnchors, resetActionAnchorMemoryForTests } from "@/mobile/opencodeActionAnchors";
import {
  buildDraftSubmission,
  isFormAnswerable,
  type PendingForm,
} from "@/mobile/opencodeQuestionDraft";
import type { ActionView, PublicNativeView } from "@/mobile/opencodeNativeApi";
import i18n from "@/i18n";
import { pinZhNavigator } from "./pinZhNavigator";
import { MockEventSource } from "./eventSourceMock";

let unpinZh: () => void;

beforeAll(async () => {
  unpinZh = await pinZhNavigator();
});

afterAll(() => {
  unpinZh();
});

// ===== 夹具：与 Rust `PendingView::Form` 的 serde 形状逐字段对应（字段名 snake_case）=====

/** 三字段表单（与任务 0 金样的可答形态同构）：q0 选项（custom:true）、q1 必答自由文本、
 *  q2 **未声明** required（可选）。`required:null` 是「服务端没指定」，不是 false。 */
function formFixture(over: Partial<PendingForm> = {}): PendingForm {
  return {
    kind: "form",
    id: "frm_1",
    title: "部署前确认",
    context_token: "ctx_1",
    state: "unknown",
    fields: [
      {
        key: "q0",
        title: "目标环境",
        required: true,
        kind: {
          kind: "text",
          custom: true,
          options: [
            { value: "q0-red", label: "预发布" },
            { value: "q0-blue", label: "生产" },
          ],
        },
      },
      {
        key: "q1",
        title: "发布说明",
        required: true,
        kind: { kind: "text", custom: true, options: [] },
      },
      {
        key: "q2",
        title: "备注",
        required: null,
        kind: { kind: "text", custom: true, options: [] },
      },
    ],
    ...over,
  };
}

/** 权威视图夹具（与 Rust `PublicNativeView` 的 serde 形状对应）。 */
function nativeView(over: Partial<PublicNativeView> = {}): PublicNativeView {
  return {
    sessionId: "sess-1",
    bindingToken: "bind_1",
    routeVersion: 1,
    revision: 1,
    complete: true,
    capabilities: {
      answer_group: { allowed: true, evidence: "contract: form reply", reasonCode: null },
      permission_once: { allowed: true, evidence: "contract: once", reasonCode: null },
      permission_reject: { allowed: true, evidence: "contract: reject", reasonCode: null },
    },
    messages: [],
    pending: [],
    inbox: [],
    runtimeState: "ready",
    files: {},
    route: "opencode_native",
    routeReasonCode: null,
    ...over,
  };
}

interface Routes {
  view?: PublicNativeView;
  /** POST /opencode/actions 回执（缺省把请求的 actionId 回显成 confirmed） */
  action?: ActionView | ((body: Record<string, unknown>) => ActionView);
  actionStatus?: number;
  actionBody?: Record<string, unknown>;
  /** 挂起回执（迟到回执用例手动放行） */
  actionHang?: boolean;
  /** 网络层异常（结果不明） */
  actionReject?: boolean;
  verify?: ActionView;
  verifyStatus?: number;
  /** 核对错误体（缺省给传输形状；404 用例显式给 OC_ACTION_NOT_FOUND） */
  verifyBody?: Record<string, unknown>;
  /** 核对请求挂起，但**响应 abort 信号**（15s 超时用例：超时要真的中止请求） */
  verifyAbortable?: boolean;
}

let routes: Routes;
/** **所有** POST 请求（URL + 解析后的 body）：零 POST 断言与请求体断言共用同一份记录。 */
let posts: { url: string; body: Record<string, unknown> | null }[];
let viewCalls: number;
let legacyCalls: number;
let formResultGets: { url: string; binding: string | null }[];
let releaseAction: ((r: Response) => void) | null = null;

function json(body: unknown, status = 200) {
  return new Response(JSON.stringify(body), { status });
}

function installFetch() {
  vi.stubGlobal(
    "fetch",
    vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
      const url = String(input);
      const method = init?.method ?? "GET";
      if (method === "POST") {
        let parsed: Record<string, unknown> | null;
        try {
          parsed = JSON.parse(String(init?.body ?? "{}")) as Record<string, unknown>;
        } catch {
          parsed = null;
        }
        posts.push({ url, body: parsed });
      }
      if (url.includes("/opencode/route/preview?")) {
        const target = routes.view ?? nativeView();
        return json({
          sessionId: target.sessionId,
          candidate: {
            sessionId: target.sessionId,
            directory: "/project",
            serviceId: "service",
            pid: 1,
            startedAtMs: 1,
            bindingToken: target.bindingToken,
          },
          previewToken: "preview-token",
          expiresAtMs: 9999999999999,
        });
      }
      if (url.endsWith("/opencode/route/native") && method === "POST") {
        const target = routes.view ?? nativeView();
        return json({
          sessionId: target.sessionId,
          route: "opencode_native",
          bindingToken: target.bindingToken,
          routeVersion: target.routeVersion,
        });
      }
      if (url.includes("/opencode/actions/") && url.endsWith("/verify")) {
        if (routes.verifyAbortable) {
          // 真实 fetch 在 signal abort 时以 AbortError 拒绝；超时必须走同一条路径
          return new Promise<Response>((_resolve, reject) => {
            const signal = init?.signal;
            if (signal?.aborted) {
              reject(new DOMException("aborted", "AbortError"));
              return;
            }
            signal?.addEventListener("abort", () =>
              reject(new DOMException("aborted", "AbortError"))
            );
          });
        }
        if (routes.verifyStatus)
          return json(routes.verifyBody ?? { error: "OC_TRANSPORT_FAILED" }, routes.verifyStatus);
        return json(routes.verify ?? { actionId: "act_unknown", state: "confirmed" });
      }
      if (url.endsWith("/opencode/actions") && method === "POST") {
        const body = JSON.parse(String(init?.body ?? "{}")) as Record<string, unknown>;
        if (routes.actionReject) throw new TypeError("actions 网络断开（模拟网络层异常）");
        if (routes.actionStatus) {
          return json(routes.actionBody ?? { error: "internal" }, routes.actionStatus);
        }
        if (routes.actionHang) {
          return new Promise<Response>((resolve) => {
            releaseAction = resolve;
          });
        }
        if (typeof routes.action === "function") return json(routes.action(body));
        return json(routes.action ?? { actionId: body.actionId, state: "confirmed" });
      }
      if (url.includes("/opencode/forms/") && method === "GET") {
        formResultGets.push({
          url,
          binding: new Headers(init?.headers).get("X-OpenCode-Expected-Binding"),
        });
        return json({ formId: "frm_1", sessionId: "sess-1", status: "pending", answers: {} });
      }
      if (url.includes("/opencode/view")) {
        viewCalls += 1;
        return json(routes.view ?? nativeView());
      }
      if (url.includes("/channel")) return json({ via: "lan", limited: false });
      // 旧终端写链（断言零调用）
      if (url.includes("/session-")) {
        legacyCalls += 1;
        return json({});
      }
      throw new Error(`unexpected fetch: ${url}`);
    })
  );
  vi.stubGlobal("EventSource", MockEventSource as unknown as typeof EventSource);
}

beforeEach(() => {
  routes = {};
  posts = [];
  viewCalls = 0;
  legacyCalls = 0;
  formResultGets = [];
  releaseAction = null;
  // 恢复锚按会话写在 sessionStorage：用例之间必须隔离，否则上一个用例的未确认动作会
  // 在下一个用例的面板里出现（那会让「零 POST / 无恢复入口」的断言变成假绿）。
  resetActionAnchorMemoryForTests();
  sessionStorage.clear();
  MockEventSource.reset();
});

afterEach(() => {
  cleanup();
  vi.useRealTimers();
  vi.unstubAllGlobals();
  vi.restoreAllMocks();
});

/** 挂起的 actions 回执放行 */
async function finishAction(body: unknown) {
  const release = releaseAction;
  releaseAction = null;
  if (!release) throw new Error("没有挂起中的 actions 请求");
  await act(async () => {
    release(json(body));
    await Promise.resolve();
  });
}

function actionPosts() {
  return posts.filter((p) => p.url.endsWith("/opencode/actions"));
}

/** 当前题目里某个选项按钮（按原生 value 定位，不按展示 label）。 */
function optionButton(fieldKey: string, value: string): HTMLElement {
  const found = screen
    .getAllByTestId("opencode-q-option")
    .find((el) => el.dataset.key === fieldKey && el.dataset.value === value);
  if (!found) throw new Error(`没有选项 ${fieldKey}=${value}`);
  return found;
}

function renderCard(form: PendingForm = formFixture(), onChanged = vi.fn()) {
  installFetch();
  const view = render(
    <OpenCodeQuestionGroupCard
      sessionId="sess-1"
      bindingToken="bind_1"
      form={form}
      onChanged={onChanged}
    />
  );
  return { ...view, onChanged };
}

/** 填满两个必答字段（q0 选「预发布」= value `q0-red`；q1 输入自由文本）。 */
function fillRequired() {
  fireEvent.click(optionButton("q0", "q0-red"));
  fireEvent.click(screen.getByTestId("opencode-q-next"));
  fireEvent.change(screen.getByTestId("opencode-q-text"), {
    target: { value: "发布说明一段" },
  });
}

describe("OpenCodeQuestionGroupCard：组内草稿与整组提交", () => {
  it("选择/输入/前后翻题/清空/放弃：全程零 POST（选中项回显 label）", async () => {
    const { onChanged } = renderCard();

    fireEvent.click(optionButton("q0", "q0-red"));
    // M-11：自由文本框回显用户点过的 **label**，不是原生 value（提交仍用 value，见下一条）
    expect((screen.getByTestId("opencode-q-text") as HTMLTextAreaElement).value).toBe("预发布");
    fireEvent.click(screen.getByTestId("opencode-q-next"));
    fireEvent.change(screen.getByTestId("opencode-q-text"), { target: { value: "草稿文字" } });
    fireEvent.click(screen.getByTestId("opencode-q-prev"));
    expect(screen.getByTestId("opencode-q-progress").textContent).toContain(
      i18n.t("mobile.openCodeNative.questions.progress", { current: 1, total: 3 })
    );
    fireEvent.click(screen.getByTestId("opencode-q-clear"));
    fireEvent.click(screen.getByTestId("opencode-q-next"));
    fireEvent.click(screen.getByTestId("opencode-q-next"));
    fireEvent.click(screen.getByTestId("opencode-q-abandon"));

    expect(await screen.findByTestId("opencode-q-abandoned")).toBeTruthy();
    // 零 POST：草稿动作一个字节都不上网络（也不碰旧终端端点）
    expect(posts).toHaveLength(0);
    expect(onChanged).not.toHaveBeenCalled();
  });

  it("缺必答：提交键不可用、点了也没有 POST，并指出缺哪一题", async () => {
    renderCard();
    fireEvent.click(optionButton("q0", "q0-red"));

    const missing = screen.getByTestId("opencode-q-missing");
    expect(missing.textContent).toContain("发布说明");
    const submit = screen.getByTestId("opencode-q-submit") as HTMLButtonElement;
    expect(submit).toBeDisabled();
    fireEvent.click(submit);
    expect(posts).toHaveLength(0);
  });

  it("合法可选跳过：整组只带必答键（跳过 = 省略键，绝不发空串）", async () => {
    renderCard();
    fillRequired();
    fireEvent.click(screen.getByTestId("opencode-q-next")); // 到可选 q2，什么都不填
    fireEvent.click(screen.getByTestId("opencode-q-submit"));

    await waitFor(() => expect(actionPosts()).toHaveLength(1));
    const body = actionPosts()[0].body as {
      actionId: string;
      sessionId: string;
      bindingToken: string;
      operation: Record<string, unknown>;
    };
    expect(body.sessionId).toBe("sess-1");
    expect(body.bindingToken).toBe("bind_1");
    expect(body.actionId).toMatch(/^act_[A-Za-z0-9_]+$/);
    // 一次确认 = 一份整组答案（原生 value，不是展示 label）；可选 q2 的键**不存在**
    expect(body.operation).toEqual({
      kind: "answer",
      formId: "frm_1",
      contextToken: "ctx_1",
      answers: {
        q0: { kind: "text", value: "q0-red" },
        q1: { kind: "text", value: "发布说明一段" },
      },
    });
    expect(Object.keys((body.operation as { answers: object }).answers)).not.toContain("q2");
    // 空串永远不是编码：请求体里不出现空的 text 值
    expect(JSON.stringify(body.operation)).not.toContain('"value":""');
  });

  it("两组各自留稿：新组的答案不污染旧组", () => {
    const onChanged = vi.fn();
    const formA = formFixture();
    const formB = formFixture({
      id: "frm_2",
      title: "另一组问题",
      context_token: "ctx_b",
      fields: [
        {
          key: "q0",
          title: "目标环境",
          required: true,
          kind: { kind: "text", custom: false, options: [{ value: "b-only", label: "仅 B 组" }] },
        },
      ],
    });
    installFetch();
    const { rerender } = render(
      <OpenCodeQuestionGroupCard
        sessionId="sess-1"
        bindingToken="bind_1"
        form={formA}
        onChanged={onChanged}
      />
    );
    fireEvent.click(optionButton("q0", "q0-red"));

    rerender(
      <OpenCodeQuestionGroupCard
        sessionId="sess-1"
        bindingToken="bind_1"
        form={formB}
        onChanged={onChanged}
      />
    );
    // 新组同一 key 上没有旧选择（旧值是 q0-red，B 组根本没有它）
    expect(optionButton("q0", "b-only").dataset.selected).toBe("false");
    fireEvent.click(optionButton("q0", "b-only"));

    rerender(
      <OpenCodeQuestionGroupCard
        sessionId="sess-1"
        bindingToken="bind_1"
        form={formA}
        onChanged={onChanged}
      />
    );
    // 回到旧组：旧稿还在，且没有被 B 组的选择覆盖
    expect(optionButton("q0", "q0-red").dataset.selected).toBe("true");
    expect(optionButton("q0", "q0-blue").dataset.selected).toBe("false");
    expect(posts).toHaveLength(0);
  });

  it("选项变了：旧答案标为待核对、不能偷偷提交（零 POST）", () => {
    const onChanged = vi.fn();
    installFetch();
    const { rerender } = render(
      <OpenCodeQuestionGroupCard
        sessionId="sess-1"
        bindingToken="bind_1"
        form={formFixture()}
        onChanged={onChanged}
      />
    );
    fireEvent.click(optionButton("q0", "q0-red"));

    // 原生侧重新投影：同一表单 id，内容变了 ⇒ context_token 轮换、选项集也换了
    const changed = formFixture({
      context_token: "ctx_2",
      fields: [
        {
          key: "q0",
          title: "目标环境",
          required: true,
          kind: {
            kind: "text",
            custom: false,
            options: [
              { value: "q0-blue", label: "生产" },
              { value: "q0-green", label: "灰度" },
            ],
          },
        },
      ],
    });
    rerender(
      <OpenCodeQuestionGroupCard
        sessionId="sess-1"
        bindingToken="bind_1"
        form={changed}
        onChanged={onChanged}
      />
    );

    expect(screen.getByTestId("opencode-q-stale").textContent).toBe(
      i18n.t("mobile.openCodeNative.questions.needsReview")
    );
    // M-3：逐字段标记（哪一题变了），不是只有一条整组黄条
    expect(screen.getByTestId("opencode-q-stale-field").textContent).toBe(
      i18n.t("mobile.openCodeNative.questions.staleField")
    );
    expect(screen.getByTestId("opencode-q-jump").dataset.stale).toBe("true");
    // 旧答案仍然可见（不是静默丢弃，也不是静默沿用）
    expect(screen.getByTestId("opencode-q-legacy").textContent).toContain("q0-red");
    const submit = screen.getByTestId("opencode-q-submit") as HTMLButtonElement;
    expect(submit).toBeDisabled();
    fireEvent.click(submit);
    expect(posts).toHaveLength(0);
  });

  it("title_change_marks_stale_even_when_options_match：题干变了（选项完全不变）旧答案 stale、提交键关闭、零 POST", () => {
    const onChanged = vi.fn();
    installFetch();
    const { rerender } = render(
      <OpenCodeQuestionGroupCard
        sessionId="sess-1"
        bindingToken="bind_1"
        form={formFixture()}
        onChanged={onChanged}
      />
    );
    fireEvent.click(optionButton("q0", "q0-red"));

    // 同一个 form.id / 同一组选项，只有**题干**改了：答案的含义可能已经不同
    const retitled = formFixture({
      context_token: "ctx_2",
      fields: formFixture().fields.map((field) =>
        field.key === "q0" ? { ...field, title: "目标环境（已更新）" } : field
      ),
    });
    rerender(
      <OpenCodeQuestionGroupCard
        sessionId="sess-1"
        bindingToken="bind_1"
        form={retitled}
        onChanged={onChanged}
      />
    );

    expect(screen.getByTestId("opencode-q-stale").textContent).toBe(
      i18n.t("mobile.openCodeNative.questions.needsReview")
    );
    expect(screen.getByTestId("opencode-q-stale-field").textContent).toBe(
      i18n.t("mobile.openCodeNative.questions.staleField")
    );
    const submit = screen.getByTestId("opencode-q-submit") as HTMLButtonElement;
    expect(submit).toBeDisabled();
    fireEvent.click(submit);
    expect(actionPosts()).toHaveLength(0);
  });

  it("reaffirm_allows_submit：显式核对后旧答案可提交（题干变化也认）", async () => {
    installFetch();
    const onChanged = vi.fn();
    const { rerender } = render(
      <OpenCodeQuestionGroupCard
        sessionId="sess-1"
        bindingToken="bind_1"
        form={formFixture()}
        onChanged={onChanged}
      />
    );
    fireEvent.click(optionButton("q0", "q0-red"));
    fireEvent.click(screen.getByTestId("opencode-q-next"));
    fireEvent.change(screen.getByTestId("opencode-q-text"), {
      target: { value: "发布说明一段" },
    });

    const retitled = formFixture({
      context_token: "ctx_2",
      fields: formFixture().fields.map((field) =>
        field.key === "q0" ? { ...field, title: "目标环境（已更新）" } : field
      ),
    });
    rerender(
      <OpenCodeQuestionGroupCard
        sessionId="sess-1"
        bindingToken="bind_1"
        form={retitled}
        onChanged={onChanged}
      />
    );
    expect(screen.getByTestId("opencode-q-submit")).toBeDisabled();

    fireEvent.click(screen.getByTestId("opencode-q-reaffirm"));
    expect(screen.queryByTestId("opencode-q-stale")).toBeNull();
    const submit = screen.getByTestId("opencode-q-submit") as HTMLButtonElement;
    expect(submit).not.toBeDisabled();

    fireEvent.click(submit);
    await waitFor(() => expect(actionPosts()).toHaveLength(1));
    const operation = actionPosts()[0].body?.operation as {
      contextToken: string;
      answers: Record<string, unknown>;
    };
    // 用户显式核对过：沿用原答案（仍是原生 value，不是 label）
    expect(operation.contextToken).toBe("ctx_2");
    expect(operation.answers).toEqual({
      q0: { kind: "text", value: "q0-red" },
      q1: { kind: "text", value: "发布说明一段" },
    });
  });

  /** 「删除测试库 → 生产库，选项仍是是/否」：**只有题干变**，选项集逐字不变。
   *  旧答案（是/否）在题干变化后不能再被静默提交，必须重新核对。 */
  function dbForm(fieldTitle: string): PendingForm {
    return {
      kind: "form",
      id: "frm_db",
      title: "数据库操作确认",
      context_token: `ctx_db_${fieldTitle}`,
      state: "unknown",
      fields: [
        {
          key: "q0",
          title: fieldTitle,
          required: true,
          kind: {
            kind: "text",
            custom: false,
            options: [
              { value: "yes", label: "是" },
              { value: "no", label: "否" },
            ],
          },
        },
      ],
    };
  }

  it("删除测试库→生产库、选项仍是是/否：题干变化使旧答案 stale，核对后才可提交", async () => {
    installFetch();
    const onChanged = vi.fn();
    const { rerender } = render(
      <OpenCodeQuestionGroupCard
        sessionId="sess-1"
        bindingToken="bind_1"
        form={dbForm("删除测试库")}
        onChanged={onChanged}
      />
    );
    // 选项 label 来自原生 value 映射（展示 label 不变，提交 value 不变）。
    // 选项行复用统一问答卡的编号徽标组件 ⇒ 文本前带行号（"1是"），按包含断言。
    expect(optionButton("q0", "yes").textContent).toContain("是");
    expect(optionButton("q0", "no").textContent).toContain("否");
    fireEvent.click(optionButton("q0", "yes"));

    rerender(
      <OpenCodeQuestionGroupCard
        sessionId="sess-1"
        bindingToken="bind_1"
        form={dbForm("删除生产库")}
        onChanged={onChanged}
      />
    );

    // 选项逐字相同也不得静默沿用：题干变了 ⇒ stale + 提交键关闭 + 零 POST
    expect(screen.getByTestId("opencode-q-stale-field")).toBeTruthy();
    const submit = screen.getByTestId("opencode-q-submit") as HTMLButtonElement;
    expect(submit).toBeDisabled();
    fireEvent.click(submit);
    expect(actionPosts()).toHaveLength(0);

    // 用户确认「要删的是生产库」之后才放行，且提交值仍是 yes（原生 value）
    fireEvent.click(screen.getByTestId("opencode-q-reaffirm"));
    expect(screen.getByTestId("opencode-q-submit")).not.toBeDisabled();
    fireEvent.click(screen.getByTestId("opencode-q-submit"));
    await waitFor(() => expect(actionPosts()).toHaveLength(1));
    const operation = actionPosts()[0].body?.operation as {
      contextToken: string;
      answers: Record<string, unknown>;
    };
    expect(operation.contextToken).toBe("ctx_db_删除生产库");
    expect(operation.answers).toEqual({ q0: { kind: "text", value: "yes" } });
  });

  /** I-2 场景：填满 q0/q1/q2 → 同一 form.id 重新投影成只有 q0/q1（q2 变孤儿）。
   *  两条用例共用这一个 setup，断言面各不相同。 */
  function renderShrunkForm() {
    const onChanged = vi.fn();
    installFetch();
    const { rerender } = render(
      <OpenCodeQuestionGroupCard
        sessionId="sess-1"
        bindingToken="bind_1"
        form={formFixture()}
        onChanged={onChanged}
      />
    );
    fireEvent.click(optionButton("q0", "q0-red"));
    fireEvent.click(screen.getByTestId("opencode-q-next"));
    fireEvent.change(screen.getByTestId("opencode-q-text"), { target: { value: "发布说明一段" } });
    fireEvent.click(screen.getByTestId("opencode-q-next"));
    fireEvent.change(screen.getByTestId("opencode-q-text"), { target: { value: "旧备注" } });

    // 同一 form.id 重新投影：q2 被移除（q0/q1 的类型与选项没变 ⇒ 逐字段版本未变）
    const shrunk = formFixture({
      context_token: "ctx_2",
      fields: formFixture().fields.filter((field) => field.key !== "q2"),
    });
    rerender(
      <OpenCodeQuestionGroupCard
        sessionId="sess-1"
        bindingToken="bind_1"
        form={shrunk}
        onChanged={onChanged}
      />
    );
    return { onChanged };
  }

  it("字段集缩小（a）：幸存题的答案照常提交，请求体省略孤儿键（I-2）", async () => {
    renderShrunkForm();

    fireEvent.click(screen.getByTestId("opencode-q-submit"));
    await waitFor(() => expect(actionPosts()).toHaveLength(1));
    const operation = actionPosts()[0].body?.operation as {
      contextToken: string;
      answers: Record<string, unknown>;
    };
    expect(operation.contextToken).toBe("ctx_2");
    expect(operation.answers).toEqual({
      q0: { kind: "text", value: "q0-red" },
      q1: { kind: "text", value: "发布说明一段" },
    });
    expect(Object.keys(operation.answers)).not.toContain("q2");
    // 没有把孤儿编码成 skip / 空串混进请求体
    expect(JSON.stringify(operation)).not.toContain('"q2"');
  });

  it("字段集缩小（b）：整组不再死锁——提交键可用，孤儿只做一行告知（I-2）", () => {
    renderShrunkForm();

    expect(screen.getByTestId("opencode-q-orphans").textContent).toBe(
      i18n.t("mobile.openCodeNative.questions.orphans", { count: 1 })
    );
    // 孤儿既不是「待核对」，也不是阻塞错误
    expect(screen.queryByTestId("opencode-q-blocked")).toBeNull();
    expect(screen.queryByTestId("opencode-q-stale")).toBeNull();
    expect(screen.queryByTestId("opencode-q-missing")).toBeNull();
    expect(screen.getByTestId("opencode-q-submit")).not.toBeDisabled();
  });

  it("派发前受控拒绝（OC_BINDING_STALE）：零写出、保留草稿，并重读视图（M-8）", async () => {
    routes.actionStatus = 409;
    routes.actionBody = { error: "OC_BINDING_STALE", reasonCode: "OC_BINDING_STALE" };
    const { onChanged } = renderCard();
    fillRequired();
    fireEvent.click(screen.getByTestId("opencode-q-submit"));

    const receipt = await screen.findByTestId("opencode-q-receipt");
    expect(receipt.dataset.kind).toBe("rejected");
    expect(receipt.textContent).toBe(i18n.t("mobile.openCodeNative.errors.bindingStale"));
    // 绑定已过期：必须重读视图，否则用户只会拿同一份旧凭据再失败一次
    expect(onChanged).toHaveBeenCalledTimes(1);
    // 零写出 ⇒ 草稿保留、提交键仍在（不是「已定案」那种撤下的情形）
    expect(screen.getByTestId("opencode-q-submit")).toBeTruthy();
    expect(actionPosts()).toHaveLength(1);
  });

  it("没有更细的确认：一次点击 = 恰好一个 answer POST（含整组）", async () => {
    renderCard();
    fillRequired();
    const submit = screen.getByTestId("opencode-q-submit");
    fireEvent.click(submit);
    fireEvent.click(submit); // 连点（第一个请求未落地前按钮已 disabled）
    await waitFor(() => expect(actionPosts()).toHaveLength(1));

    // 回执落地后写入口**不再渲染**（不是禁用）：没有第二个按钮可点，自然没有第二次写
    await screen.findByTestId("opencode-q-receipt");
    expect(screen.queryByTestId("opencode-q-submit")).toBeNull();
    expect(actionPosts()).toHaveLength(1);
    expect(legacyCalls).toBe(0);
  });

  it("409（OC_REQUEST_SETTLED）：呈现原生结果、触发重读，不重发也不说「另一端获胜」", async () => {
    routes.actionStatus = 409;
    routes.actionBody = { error: "OC_REQUEST_SETTLED", reasonCode: "OC_REQUEST_SETTLED" };
    const { onChanged } = renderCard();
    fillRequired();
    fireEvent.click(screen.getByTestId("opencode-q-submit"));

    const receipt = await screen.findByTestId("opencode-q-receipt");
    expect(receipt.dataset.kind).toBe("settled");
    expect(receipt.textContent).toBe(i18n.t("mobile.openCodeNative.questions.receipt.settled"));
    // 「呈现原生结果」= 让权威投影重读（不是本地猜一个结论）
    expect(onChanged).toHaveBeenCalled();
    // 不重发：写入口已撤下（不是禁用，是不渲染）
    expect(screen.queryByTestId("opencode-q-submit")).toBeNull();
    expect(actionPosts()).toHaveLength(1);
    expect(screen.queryByText(/获胜|另一端|哪端获胜|other side won/i)).toBeNull();
  });

  it("手机与桌面答案相同但来源未知：不宣称本端成功", async () => {
    routes.action = (body) => ({
      actionId: String(body.actionId),
      state: "settled",
    });
    renderCard();
    fillRequired();
    fireEvent.click(screen.getByTestId("opencode-q-submit"));

    const receipt = await screen.findByTestId("opencode-q-receipt");
    expect(receipt.dataset.kind).toBe("settled");
    expect(receipt.textContent).toBe(i18n.t("mobile.openCodeNative.questions.receipt.settled"));
    expect(receipt.textContent).not.toMatch(/本机|手机端|本地已|获胜/);

    // 文案面同样不许出现「哪一端成功」的说法（zh 与 en 都查）
    const claimWords =
      /本机已|手机端已|本端已成功|另一端获胜|哪一端获胜|已在本机生效|other side won|saved on this device|mobile succeeded/i;
    const bundles = ["zh", "en"] as const;
    for (const lng of bundles) {
      const questions = i18n.getResourceBundle(lng, "translation") as {
        mobile: { openCodeNative: { questions: Record<string, unknown> } };
      };
      for (const value of flatValues(questions.mobile.openCodeNative.questions)) {
        expect(value).not.toMatch(claimWords);
      }
    }
  });

  it("迟到确认不清掉「提交之后」才输入的文字（草稿在点击帧冻结）", async () => {
    routes.actionHang = true;
    const { onChanged } = renderCard();
    fillRequired();
    fireEvent.click(screen.getByTestId("opencode-q-submit"));
    await waitFor(() => expect(actionPosts()).toHaveLength(1));

    // 回执未到时继续输入（属于点击之后的新草稿）
    fireEvent.click(screen.getByTestId("opencode-q-next"));
    fireEvent.change(screen.getByTestId("opencode-q-text"), {
      target: { value: "提交后才写的备注" },
    });
    await finishAction({ actionId: "act_late", state: "confirmed" });

    expect(await screen.findByTestId("opencode-q-receipt")).toBeTruthy();
    expect((screen.getByTestId("opencode-q-text") as HTMLTextAreaElement).value).toBe(
      "提交后才写的备注"
    );
    expect(onChanged).toHaveBeenCalled();
  });

  it("verify_timeout_keeps_unknown_and_reenables：15s 中止后仍是未确认、按钮恢复可点", async () => {
    routes.action = (body) => ({ actionId: String(body.actionId), state: "unconfirmed" });
    renderCard();
    fillRequired();
    fireEvent.click(screen.getByTestId("opencode-q-submit"));
    const receipt = await screen.findByTestId("opencode-q-receipt");
    expect(receipt.dataset.kind).toBe("unknown");
    expect(actionPosts()).toHaveLength(1);

    // 核对请求挂住，但响应 abort 信号（真实 fetch 在超时中止时也是这样拒绝的）
    routes.verifyAbortable = true;
    vi.useFakeTimers();
    try {
      fireEvent.click(screen.getByTestId("opencode-q-verify"));
      await act(async () => {
        await vi.advanceTimersByTimeAsync(15_000);
      });

      // 超时不是动作结论：仍是 unknown（绝不能被折成 not_sent），也无第二次写
      const after = screen.getByTestId("opencode-q-receipt");
      expect(after.dataset.kind).toBe("unknown");
      expect(
        screen.queryByText(i18n.t("mobile.openCodeNative.questions.receipt.notSent"))
      ).toBeNull();
      expect(
        screen.queryByText(i18n.t("mobile.openCodeNative.questions.receipt.notAccepted"))
      ).toBeNull();
      // 在途闸复位：用户可以重试核对
      expect(screen.getByTestId("opencode-q-verify")).not.toBeDisabled();
      expect(actionPosts()).toHaveLength(1);
    } finally {
      vi.useRealTimers();
    }
  });

  it("verify_404_keeps_unknown_and_anchor：404 不是零写出", async () => {
    routes.action = (body) => ({ actionId: String(body.actionId), state: "unconfirmed" });
    renderCard();
    fillRequired();
    fireEvent.click(screen.getByTestId("opencode-q-submit"));
    const receipt = await screen.findByTestId("opencode-q-receipt");
    expect(receipt.dataset.kind).toBe("unknown");
    const sent = (actionPosts()[0].body as { actionId: string }).actionId;

    // 首次派发仍在 reserve 之前：核对只能拿到 404
    routes.verifyStatus = 404;
    routes.verifyBody = { error: "OC_ACTION_NOT_FOUND" };
    fireEvent.click(screen.getByTestId("opencode-q-verify"));
    await waitFor(() => expect(posts.filter((p) => p.url.endsWith("/verify"))).toHaveLength(1));
    await act(async () => {
      await Promise.resolve();
    });

    // 仍是未确认：不渲染提交键、不产生第二次写、核对键恢复可点
    const after = screen.getByTestId("opencode-q-receipt");
    expect(after.dataset.kind).toBe("unknown");
    expect(screen.queryByTestId("opencode-q-submit")).toBeNull();
    expect(screen.getByTestId("opencode-q-verify")).not.toBeDisabled();
    expect(actionPosts()).toHaveLength(1);
    expect(readActionAnchors("sess-1").map((a) => a.actionId)).toContain(sent);
    // 文案面不得把 404 说成零写出/未生效
    expect(
      screen.queryByText(i18n.t("mobile.openCodeNative.questions.receipt.notSent"))
    ).toBeNull();
    expect(
      screen.queryByText(i18n.t("mobile.openCodeNative.questions.receipt.notAccepted"))
    ).toBeNull();
  });

  it("Unsupported 表单：整组只读、给出具体原因、零 POST", () => {
    const form = formFixture({
      fields: [
        ...formFixture().fields,
        { key: "q9", title: "分支问题", required: true, kind: { kind: "unsupported" } },
      ],
    });
    const { onChanged } = renderCard(form);

    const reason = screen.getByTestId("opencode-q-unsupported");
    // M-9：用**题面标题**，不是内部 key（q9 是协议字段名，用户看不到）
    expect(reason.textContent).toContain("分支问题");
    expect(reason.textContent).not.toContain("q9");
    expect(reason.textContent).toContain(
      i18n.t("mobile.openCodeNative.questions.unsupportedReason")
    );
    expect(screen.queryByTestId("opencode-q-submit")).toBeNull();
    expect(screen.queryByTestId("opencode-q-option")).toBeNull();
    expect(posts).toHaveLength(0);
    expect(onChanged).not.toHaveBeenCalled();
  });
});

// ============================================================
// 字段题干（官方 Form.StringField.description）：官方契约里 `title` 只是短标题，
// `description` 才是实际题干。手机端必须在当前题标题下、选项上展示它；老载荷
// （无该键）与空白值不得凭空上屏，也不得改变草稿/提交语义。
// ============================================================
describe("OpenCodeQuestionGroupCard × 字段题干", () => {
  it("非空 description 显示在当前题 title 下、选项上；空白/缺省不渲染", () => {
    const base = formFixture();
    const form = formFixture({
      fields: [
        { ...base.fields[0], description: "请在预发布与生产之间选择" },
        { ...base.fields[1], description: "   " },
        { ...base.fields[2] },
      ],
    });
    renderCard(form);

    const title = screen.getByTestId("opencode-q-title");
    const description = screen.getByTestId("opencode-q-description");
    expect(description.textContent).toBe("请在预发布与生产之间选择");
    // 位置：题干在标题之后、第一个选项之前（不是替代标题，也不挤进选项行）
    expect(
      title.compareDocumentPosition(description) & Node.DOCUMENT_POSITION_FOLLOWING
    ).toBeTruthy();
    expect(
      description.compareDocumentPosition(optionButton("q0", "q0-red")) &
        Node.DOCUMENT_POSITION_FOLLOWING
    ).toBeTruthy();

    // 空白 description 视同没有：不产生空行
    fireEvent.click(screen.getByTestId("opencode-q-next"));
    expect(screen.queryByTestId("opencode-q-description")).toBeNull();
    // 老载荷缺键：正常渲染、不崩溃
    fireEvent.click(screen.getByTestId("opencode-q-next"));
    expect(screen.queryByTestId("opencode-q-description")).toBeNull();
    expect(screen.getByTestId("opencode-q-title").textContent).toContain("备注");
    expect(posts).toHaveLength(0);
  });

  it("description 变化不改变提交体、也不触发草稿漂移（本次只读展示）", async () => {
    installFetch();
    const onChanged = vi.fn();
    const { rerender } = render(
      <OpenCodeQuestionGroupCard
        sessionId="sess-1"
        bindingToken="bind_1"
        form={formFixture()}
        onChanged={onChanged}
      />
    );
    fireEvent.click(optionButton("q0", "q0-red"));
    fireEvent.click(screen.getByTestId("opencode-q-next"));
    fireEvent.change(screen.getByTestId("opencode-q-text"), {
      target: { value: "发布说明一段" },
    });

    // 同一 form.id / 同 title / 同选项，只有 description 变了：本次范围是只读展示，
    // 不把 description 纳入 fieldSignature，因此不得出现「待核对」或锁写。
    const described = formFixture({
      fields: formFixture().fields.map((field) =>
        field.key === "q0" ? { ...field, description: "部署窗口：周五晚" } : field
      ),
    });
    rerender(
      <OpenCodeQuestionGroupCard
        sessionId="sess-1"
        bindingToken="bind_1"
        form={described}
        onChanged={onChanged}
      />
    );
    expect(screen.queryByTestId("opencode-q-stale")).toBeNull();
    // 回到 q0 看新题干（改的是 q0，当前停在 q1）
    fireEvent.click(screen.getByTestId("opencode-q-prev"));
    expect(screen.getByTestId("opencode-q-description").textContent).toBe("部署窗口：周五晚");

    fireEvent.click(screen.getByTestId("opencode-q-submit"));
    await waitFor(() => expect(actionPosts()).toHaveLength(1));
    const operation = actionPosts()[0].body?.operation as {
      answers: Record<string, unknown>;
    };
    expect(operation.answers).toEqual({
      q0: { kind: "text", value: "q0-red" },
      q1: { kind: "text", value: "发布说明一段" },
    });
    expect(JSON.stringify(operation)).not.toContain("部署窗口");
  });
});

describe("OpenCodeNativePanel × 审批：点击决定本身即确认", () => {
  const permission = {
    kind: "permission" as const,
    id: "per_1",
    title: "允许运行 rm -rf build",
    // 后端只可能给 once/reject；这里刻意混入一个不该被展示成按钮的词
    decisions: ["once", "reject", "always"],
    context_token: "ctx_perm_1",
  };

  async function renderPanel(view: PublicNativeView) {
    installFetch();
    routes.view = view;
    render(<OpenCodeNativePanel sessionId="sess-1" />);
    await waitFor(() => expect(viewCalls).toBeGreaterThan(0));
  }

  it("once 点击即提交：一次 decide POST、只含后端列出的决定词、不弹二次确认", async () => {
    const confirmSpy = vi.spyOn(window, "confirm").mockReturnValue(true);
    await renderPanel(nativeView({ pending: [permission] }));

    const row = await screen.findByTestId("native-pending-permission");
    const buttons = within(row).getAllByTestId("native-decision");
    expect(buttons.map((b) => b.dataset.decision)).toEqual(["once", "reject"]);

    fireEvent.click(buttons[0]);
    await waitFor(() => expect(actionPosts()).toHaveLength(1));
    expect(actionPosts()[0].body?.operation).toEqual({
      kind: "decide",
      requestId: "per_1",
      contextToken: "ctx_perm_1",
      decision: "once",
    });
    // 点击即确认：不插对话框、不弹 confirm
    expect(confirmSpy).not.toHaveBeenCalled();
    expect(legacyCalls).toBe(0);

    // 已提交：写入口**不再渲染**（不是禁用），并呈现「以原生投影为准」的结果句
    await waitFor(() => expect(screen.getByTestId("native-decision-receipt")).toBeTruthy());
    expect(
      within(screen.getByTestId("native-pending-permission")).queryAllByTestId("native-decision")
    ).toHaveLength(0);
    expect(screen.getByTestId("native-decision-receipt").textContent).toBe(
      i18n.t("mobile.openCodeNative.pending.decisionSubmitted")
    );
  });

  it("服务端没列出可提交的决定：只给只读说明、零 POST（M-6：不再判重复的能力分支）", async () => {
    await renderPanel(
      nativeView({
        pending: [{ ...permission, decisions: [] as string[] }],
      })
    );
    const row = await screen.findByTestId("native-pending-permission");
    expect(within(row).queryAllByTestId("native-decision")).toHaveLength(0);
    expect(posts).toHaveLength(0);
    const note = within(row).getByTestId("native-pending-permission-readonly");
    expect(note.textContent).toBe(i18n.t("mobile.openCodeNative.pending.permissionReadOnly"));
  });

  it("表单能力未放行：不挂可写作答卡（只读说明）", async () => {
    await renderPanel(nativeView({ capabilities: {}, pending: [formFixture()] }));
    expect(await screen.findByTestId("native-pending-form")).toBeTruthy();
    expect(screen.queryByTestId("opencode-q-submit")).toBeNull();
    expect(posts).toHaveLength(0);
  });

  it("表单能力放行：待答表单挂成可写作答卡（面板不复制草稿逻辑）", async () => {
    await renderPanel(nativeView({ pending: [formFixture()] }));
    expect(await screen.findByTestId("opencode-question-card")).toBeTruthy();
    expect(screen.getByTestId("opencode-q-submit")).toBeTruthy();
  });

  it("同会话同表单换绑定后核对 A 仅收敛 A，B 题卡和草稿独立", async () => {
    vi.spyOn(window, "confirm").mockReturnValue(true);
    routes.action = (body) => ({ actionId: String(body.actionId), state: "unconfirmed" });
    const form = formFixture();
    await renderPanel(nativeView({ pending: [form] }));
    await screen.findByTestId("opencode-question-card");
    fillRequired();
    fireEvent.click(screen.getByTestId("opencode-q-submit"));
    expect((await screen.findByTestId("opencode-q-receipt")).dataset.kind).toBe("unknown");
    const actionId = String(actionPosts()[0].body?.actionId);
    expect(actionPosts()[0].body?.bindingToken).toBe("bind_1");
    expect(readActionAnchors("sess-1").map((anchor) => anchor.actionId)).toContain(actionId);

    const formB = { ...form, title: "B 的部署确认" };
    routes.view = nativeView({
      bindingToken: "bind_2",
      routeVersion: 2,
      revision: 2,
      pending: [formB],
    });
    act(() => MockEventSource.latest().emit("revision", "2"));
    await screen.findByText("B 的部署确认");
    expect(screen.queryByTestId("native-reselect")).toBeNull();
    // B 与 A 是不同实例，即使会话、表单 id 和题面都相同，也不得继承 A 的答案。
    expect.soft(screen.getByTestId("opencode-q-text")).toHaveValue("");
    fireEvent.change(screen.getByTestId("opencode-q-text"), { target: { value: "B 的草稿" } });
    routes.verify = { actionId, state: "confirmed" };
    const verify =
      screen.queryByTestId("opencode-q-verify") ?? screen.getByTestId("native-action-verify");
    fireEvent.click(verify);
    await waitFor(() =>
      expect(readActionAnchors("sess-1").map((a) => a.actionId)).not.toContain(actionId)
    );

    expect.soft(screen.queryByTestId("opencode-q-receipt")).toBeNull();
    expect.soft(screen.queryByTestId("opencode-q-submit")).toBeInTheDocument();
    expect.soft(screen.getByTestId("opencode-q-text")).toHaveValue("B 的草稿");
    expect.soft(formResultGets.filter((call) => call.binding === "bind_2")).toHaveLength(0);
    expect.soft(actionPosts()).toHaveLength(1);
    expect.soft(posts.filter((call) => call.url.endsWith("/verify"))).toHaveLength(1);
    expect.soft(legacyCalls).toBe(0);
  });

  it("审批核对 15s 超时：保持未确认、核对键恢复可点、不重发", async () => {
    routes.action = (body) => ({ actionId: String(body.actionId), state: "unconfirmed" });
    await renderPanel(nativeView({ pending: [permission] }));

    const buttons = await screen.findAllByTestId("native-decision");
    fireEvent.click(buttons[0]); // once
    const note = await screen.findByTestId("native-decision-receipt");
    expect(note.dataset.kind).toBe("unknown");
    expect(actionPosts()).toHaveLength(1);

    // 核对请求挂住但响应 abort 信号：15s 后必须中止、按钮恢复、note 仍是未确认
    routes.verifyAbortable = true;
    vi.useFakeTimers();
    try {
      fireEvent.click(screen.getByTestId("native-decision-verify"));
      await act(async () => {
        await vi.advanceTimersByTimeAsync(15_000);
      });

      const after = screen.getByTestId("native-decision-receipt");
      expect(after.dataset.kind).toBe("unknown");
      expect(screen.getByTestId("native-decision-verify")).not.toBeDisabled();
      expect(
        screen.queryByText(i18n.t("mobile.openCodeNative.pending.decisionNotSent"))
      ).toBeNull();
      expect(actionPosts()).toHaveLength(1);
    } finally {
      vi.useRealTimers();
    }
  });

  it("decision_verify_404_keeps_unknown_and_anchor：404 不是零写出", async () => {
    routes.action = (body) => ({ actionId: String(body.actionId), state: "unconfirmed" });
    await renderPanel(nativeView({ pending: [permission] }));

    const buttons = await screen.findAllByTestId("native-decision");
    fireEvent.click(buttons[0]); // once
    const note = await screen.findByTestId("native-decision-receipt");
    expect(note.dataset.kind).toBe("unknown");
    const sent = (actionPosts()[0].body as { actionId: string }).actionId;
    expect(readActionAnchors("sess-1").map((a) => a.actionId)).toContain(sent);

    // 首次派发仍在 reserve 之前：核对只能拿到 404
    routes.verifyStatus = 404;
    routes.verifyBody = { error: "OC_ACTION_NOT_FOUND" };
    fireEvent.click(screen.getByTestId("native-decision-verify"));
    await waitFor(() => expect(posts.filter((p) => p.url.endsWith("/verify"))).toHaveLength(1));
    await act(async () => {
      await Promise.resolve();
    });

    // 仍是未确认：锚保留、决定键不复活、没有第二次写、核对键恢复可点
    const after = screen.getByTestId("native-decision-receipt");
    expect(after.dataset.kind).toBe("unknown");
    expect(after.textContent).toBe(i18n.t("mobile.openCodeNative.pending.decisionUnconfirmed"));
    expect(screen.getByTestId("native-decision-verify")).not.toBeDisabled();
    expect(
      within(screen.getByTestId("native-pending-permission")).queryAllByTestId("native-decision")
    ).toHaveLength(0);
    expect(actionPosts()).toHaveLength(1);
    expect(readActionAnchors("sess-1").map((a) => a.actionId)).toContain(sent);
    expect(screen.queryByText(i18n.t("mobile.openCodeNative.pending.decisionNotSent"))).toBeNull();
  });

  it("只读行说的是「本端契约未放行」，不再宣称本批未接入（I-1）", async () => {
    await renderPanel(
      nativeView({
        capabilities: {
          answer_group: { allowed: false, evidence: "x", reasonCode: "OC_CONTRACT_MISSING" },
        },
        pending: [formFixture()],
      })
    );
    const row = await screen.findByTestId("native-pending-form");
    // 两句同时在屏上：只读说明 + 具体的受控原因——但它们不能互相矛盾
    const copy = within(row).getByText(i18n.t("mobile.openCodeNative.pending.formReadOnly"));
    expect(copy.textContent).not.toMatch(/尚未接入|未接入|not wired|not available in this batch/i);
    expect(copy.textContent).toMatch(/桌面端|终端|desktop|terminal/i);
    expect(within(row).getByTestId("native-pending-form-blocked").textContent).toContain(
      "OC_CONTRACT_MISSING"
    );

    // 文案面：这两条只读说明在 zh/en 都不得再声称本批没有作答入口
    for (const lng of ["zh", "en"] as const) {
      const n = i18n.getResourceBundle(lng, "translation") as {
        mobile: { openCodeNative: { pending: Record<string, string> } };
      };
      for (const key of ["formReadOnly", "permissionReadOnly"]) {
        const value = n.mobile.openCodeNative.pending[key];
        expect(value).not.toMatch(/尚未接入|未接入|not wired|not available in this batch/i);
        expect(value).toMatch(/桌面端|终端|desktop|terminal/i);
      }
    }
  });
});

/** 把嵌套文案对象摊平成字符串数组（文案面断言共用）。 */
function flatValues(obj: unknown): string[] {
  if (typeof obj === "string") return [obj];
  if (obj === null || typeof obj !== "object") return [];
  return Object.values(obj as Record<string, unknown>).flatMap(flatValues);
}

it("T10 条件题：不适用草稿保留本地且省略，整组只写一次", async () => {
  const form = formFixture();
  form.fields = form.fields.slice(0, 2);
  Object.assign(form.fields[1], { when: { key: "q0", value: "yes" } });
  renderCard(form);
  fireEvent.change(screen.getByTestId("opencode-q-text"), { target: { value: "yes" } });
  fireEvent.click(screen.getByTestId("opencode-q-next"));
  fireEvent.change(screen.getByTestId("opencode-q-text"), { target: { value: "local" } });
  fireEvent.click(screen.getByTestId("opencode-q-prev"));
  fireEvent.change(screen.getByTestId("opencode-q-text"), { target: { value: "no" } });
  expect(screen.getAllByTestId("opencode-q-jump")).toHaveLength(1);
  expect(posts).toHaveLength(0);
  fireEvent.click(screen.getByTestId("opencode-q-submit"));
  await waitFor(() => expect(posts).toHaveLength(1));
  expect(posts[0].body?.operation).toMatchObject({
    answers: { q0: { kind: "text", value: "no" } },
  });
  expect((posts[0].body?.operation as { answers: object }).answers).not.toHaveProperty("q1");
  expect(legacyCalls).toBe(0);
});

it("T10 前端独立复核适用必答与未知条件", () => {
  const form = formFixture();
  form.fields = form.fields.slice(0, 2);
  form.fields[1].when = { key: "q0", value: "yes" };
  expect(() => buildDraftSubmission(form, { q0: { kind: "text", value: "yes" } })).toThrow(
    "missing_required:q1"
  );
  expect(buildDraftSubmission(form, { q0: { kind: "text", value: "no" } })).toEqual({
    q0: { kind: "text", value: "no" },
  });
  Object.assign(form.fields[1].when, { op: "neq" });
  expect(isFormAnswerable(form)).toBe(false);
});

it("T10 首题自定义 5、次题选项 1/2/3/4，只有最终整组写", async () => {
  const form = formFixture();
  form.fields = form.fields.slice(0, 2);
  form.fields[1].kind = {
    kind: "text",
    custom: false,
    options: ["1", "2", "3", "4"].map((value) => ({ value, label: value })),
  };
  renderCard(form);
  fireEvent.change(screen.getByTestId("opencode-q-text"), { target: { value: "5" } });
  fireEvent.click(screen.getByTestId("opencode-q-next"));
  fireEvent.click(optionButton("q1", "2"));
  fireEvent.click(screen.getByTestId("opencode-q-clear"));
  fireEvent.click(optionButton("q1", "3"));
  fireEvent.click(screen.getByTestId("opencode-q-prev"));
  expect(screen.getByTestId("opencode-q-text")).toHaveValue("5");
  expect(posts).toHaveLength(0);
  fireEvent.click(screen.getByTestId("opencode-q-submit"));
  await waitFor(() => expect(posts).toHaveLength(1));
  expect(posts[0].body?.operation).toMatchObject({
    answers: { q0: { kind: "text", value: "5" }, q1: { kind: "text", value: "3" } },
  });
  expect(legacyCalls).toBe(0);
});

it("T10 畸形条件保持只读且不崩溃", () => {
  const form = formFixture();
  Object.assign(form.fields[1], { when: null });
  renderCard(form);
  expect(screen.getByTestId("opencode-q-unsupported")).toBeInTheDocument();
  expect(posts).toHaveLength(0);
});

it.each(["title", "condition"])("T10 R1 隐藏题 %s 变化不阻塞适用题且不能被全局确认", (change) => {
  const form = formFixture();
  form.fields = form.fields.slice(0, 2);
  form.fields[1].when = { key: "q0", value: "yes" };
  const { rerender } = renderCard(form);
  fireEvent.change(screen.getByTestId("opencode-q-text"), { target: { value: "yes" } });
  fireEvent.click(screen.getByTestId("opencode-q-next"));
  fireEvent.change(screen.getByTestId("opencode-q-text"), { target: { value: "old answer" } });
  fireEvent.click(screen.getByTestId("opencode-q-prev"));
  fireEvent.change(screen.getByTestId("opencode-q-text"), { target: { value: "no" } });
  const changed = structuredClone(form);
  changed.context_token = "ctx_hidden_changed";
  if (change === "title") changed.fields[1].title = "新题干：确认生产环境";
  else changed.fields[1].when = { key: "q0", value: "new yes" };
  const refresh = () =>
    rerender(
      <OpenCodeQuestionGroupCard
        sessionId="sess-1"
        bindingToken="bind_1"
        form={structuredClone(changed)}
        onChanged={vi.fn()}
      />
    );
  refresh();
  expect(screen.getByTestId("opencode-q-submit")).toBeEnabled();
  expect(screen.queryByTestId("opencode-q-reaffirm")).not.toBeInTheDocument();
  // 让可见题也发生变化，确认它不能顺带确认隐藏题。
  changed.context_token = "ctx_visible_changed";
  changed.fields[0].title = "新的前置问题";
  refresh();
  fireEvent.click(screen.getByTestId("opencode-q-reaffirm"));
  expect(screen.getByTestId("opencode-q-submit")).toBeEnabled();
  fireEvent.change(screen.getByTestId("opencode-q-text"), {
    target: { value: change === "title" ? "yes" : "new yes" },
  });
  fireEvent.click(screen.getByTestId("opencode-q-next"));
  expect(screen.getByTestId("opencode-q-text")).toHaveValue("old answer");
  expect(screen.getByTestId("opencode-q-stale-field")).toBeInTheDocument();
  expect(screen.getByTestId("opencode-q-submit")).toBeDisabled();
  expect(posts).toHaveLength(0);
  fireEvent.click(screen.getByTestId("opencode-q-reaffirm"));
  expect(screen.getByTestId("opencode-q-submit")).toBeEnabled();
  expect(posts).toHaveLength(0);
});

it.each([true, false])(
  "T10 R2 多选自定义 custom=%s 编辑清空跨题全本地，最终整组一次提交",
  async (custom) => {
    const form = formFixture();
    form.fields = form.fields.slice(0, 2);
    form.fields[0].kind = {
      kind: "multi",
      custom,
      options: custom ? [{ value: "red", label: "红色" }] : [],
    };
    renderCard(form);
    if (custom) fireEvent.click(optionButton("q0", "red"));
    fireEvent.change(screen.getAllByTestId("opencode-q-multi-custom")[0], {
      target: { value: "five" },
    });
    fireEvent.change(screen.getAllByTestId("opencode-q-multi-custom")[1], {
      target: { value: "six" },
    });
    fireEvent.click(screen.getByTestId("opencode-q-next"));
    fireEvent.change(screen.getByTestId("opencode-q-text"), { target: { value: "next answer" } });
    fireEvent.click(screen.getByTestId("opencode-q-prev"));
    expect(screen.getAllByTestId("opencode-q-multi-custom")[0]).toHaveValue("five");
    fireEvent.change(screen.getAllByTestId("opencode-q-multi-custom")[0], {
      target: { value: "5" },
    });
    fireEvent.change(screen.getAllByTestId("opencode-q-multi-custom")[1], {
      target: { value: "" },
    });
    expect(posts).toHaveLength(0);
    fireEvent.click(screen.getByTestId("opencode-q-clear"));
    expect(screen.getAllByTestId("opencode-q-multi-custom")).toHaveLength(1);
    expect(screen.getByTestId("opencode-q-submit")).toBeDisabled();
    fireEvent.change(screen.getAllByTestId("opencode-q-multi-custom")[0], {
      target: { value: "final" },
    });
    if (custom) fireEvent.click(optionButton("q0", "red"));
    expect(posts).toHaveLength(0);
    fireEvent.click(screen.getByTestId("opencode-q-submit"));
    await waitFor(() => expect(posts).toHaveLength(1));
    const operation = posts[0].body?.operation as {
      answers: Record<string, { kind: string; value: unknown }>;
    };
    expect(operation.answers.q0.kind).toBe("multi");
    expect(operation.answers.q0.value).toEqual(custom ? ["final", "red"] : ["final"]);
    expect(operation.answers.q1).toEqual({ kind: "text", value: "next answer" });
    expect(legacyCalls).toBe(0);
  }
);

it("T10 R3 逐键输入选项前缀保持焦点与整项，编辑清空后整组一次提交", async () => {
  const user = userEvent.setup();
  const form = formFixture();
  form.fields = form.fields.slice(0, 2);
  form.fields[0].kind = { kind: "multi", custom: true, options: [{ value: "red", label: "红色" }] };
  renderCard(form);
  const first = screen.getAllByTestId("opencode-q-multi-custom")[0];
  await user.type(first, "red");
  expect(first).toHaveFocus();
  expect(first).toHaveValue("red");
  await user.type(first, "irect");
  expect(first).toHaveValue("redirect");
  const second = screen.getAllByTestId("opencode-q-multi-custom")[1];
  await user.type(second, "other");
  await user.clear(first);
  await user.type(first, "redirection");
  expect(first).toHaveFocus();
  expect(first).toHaveValue("redirection");
  expect(second).toHaveValue("other");
  await user.clear(second);
  await user.click(screen.getByTestId("opencode-q-next"));
  await user.type(screen.getByTestId("opencode-q-text"), "second question");
  await user.click(screen.getByTestId("opencode-q-prev"));
  expect(screen.getAllByTestId("opencode-q-multi-custom")[0]).toHaveValue("redirection");
  expect(posts).toHaveLength(0);
  await user.click(screen.getByTestId("opencode-q-submit"));
  await waitFor(() => expect(posts).toHaveLength(1));
  expect(posts[0].body?.operation).toMatchObject({
    answers: {
      q0: { kind: "multi", value: ["redirection"] },
      q1: { kind: "text", value: "second question" },
    },
  });
  expect(legacyCalls).toBe(0);
});

// ============================================================
// R3：只读题卡准确区分【连接受阻 | 题型不支持 | 状态未知】，
// 且单项状态只落在题卡本地（不冒充会话级能力问题，也不重发）。
// ============================================================
it("unsupported_form_and_unknown_outcome_stay_local：题型不支持只在题卡说明并指向桌面；未知回执保留核对入口且不写成能力缺失", async () => {
  // ① 单题型不受支持：只在该题卡提示 + 指向桌面端（不归因为能力缺失，零提交）
  const form = formFixture({
    fields: [
      formFixture().fields[0],
      { key: "q9", title: "分支问题", required: true, kind: { kind: "unsupported" } },
    ],
  });
  const first = renderCard(form);
  const note = screen.getByTestId("opencode-q-unsupported");
  expect(note.textContent).toContain("分支问题");
  expect(note.textContent).toContain(i18n.t("mobile.openCodeNative.questions.unsupportedReason"));
  // 指向桌面端：用户知道去哪里处理，而不是以为手机坏了
  expect(note.textContent).toMatch(/桌面端|终端/);
  // 不写成能力缺失：题卡的阻碍说明是「题型不支持」，不是「能力未放行」
  expect(
    screen.queryByText(i18n.t("mobile.openCodeNative.questions.capabilityBlockedGeneric"))
  ).toBeNull();
  expect(screen.queryByTestId("opencode-q-submit")).toBeNull();
  expect(actionPosts()).toHaveLength(0);
  first.unmount();

  // ② 未知回执：保留对应动作的核对入口；文案不写成能力缺失；不产生第二次提交
  const writable = formFixture();
  installFetch();
  routes.actionReject = true; // 网络层异常 ⇒ 结果未知（不是没写出，也不是能力问题）
  render(
    <OpenCodeQuestionGroupCard
      sessionId="sess-1"
      bindingToken="bind_1"
      form={writable}
      onChanged={vi.fn()}
    />
  );
  fireEvent.click(optionButton("q0", "q0-red"));
  fireEvent.click(screen.getByTestId("opencode-q-next"));
  fireEvent.change(screen.getByTestId("opencode-q-text"), { target: { value: "发布说明一段" } });
  fireEvent.click(screen.getByTestId("opencode-q-submit"));

  const receipt = await screen.findByTestId("opencode-q-receipt");
  expect(receipt.dataset.kind).toBe("unknown");
  expect(receipt.textContent).toBe(i18n.t("mobile.openCodeNative.questions.receipt.unconfirmed"));
  // 未知 ≠ 能力缺失：核对入口在场，文案是「未确认」，不是「能力未放行」
  expect(screen.getByTestId("opencode-q-verify")).not.toBeDisabled();
  expect(
    screen.queryByText(i18n.t("mobile.openCodeNative.questions.capabilityBlockedGeneric"))
  ).toBeNull();
  // 只有一次提交 POST；核对是只读 verify，不重发
  expect(actionPosts()).toHaveLength(1);
  fireEvent.click(screen.getByTestId("opencode-q-verify"));
  await waitFor(() => expect(posts.filter((p) => p.url.endsWith("/verify"))).toHaveLength(1));
  expect(actionPosts()).toHaveLength(1);
});
