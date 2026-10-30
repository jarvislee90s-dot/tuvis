// T7.2 Slice B1a：ZCode 统一控制面板（手机）行为锁 —— 本文件先红后绿，组件尚未实现。
//
// 面板只做三件事：如实呈现连接/回执状态、把草稿交给 useZcodeControl.submit（官方
// `/m/api/v1/zcode/actions`）、控制失败时不吞草稿也不影响同页阅读面。
//
// 契约（实现按此提供，测试只走用户可见面）：
// - 根节点 `data-testid="zcode-control-panel"`，`data-connection-state` = ConnectionState
//   （未确认时 `unknown`；已保存链接绝不等于 `paired`）。
// - 草稿 textarea `zcode-draft`；发送按钮 `zcode-send`（不可发送时 disabled）。
// - 就地原因 `zcode-control-reason`（reason=null 时可不渲染），`data-reason-code` =
//   冻结错误码（不是文案），文本是本地化原因。
// - 回执状态 `zcode-action-status`（收到回执后渲染），`data-action-state` =
//   ActionState；`accepted` 文案恰为「已受理」，`unknown` 不得出现「已生效」。
//
// pending 卡（问答 / 审批，Slice B1b 契约；数据只来自新 SessionView，不进旧终端
// 按键 API）：
// - 卡片根 `zcode-pending-card`，`data-interaction-id` = PendingView.interactionId。
// - 题干文本照常可读（能力关闭只锁作答面，不锁阅读面）。
// - 问答选项 `zcode-pending-option-<index>`（0 起）；能力关闭时只读禁用（按钮
//   disabled 或 aria-disabled），点击零动作 POST。
// - 只读原因 `zcode-pending-reason`：`data-reason-code` = 冻结原因码，文本为按码
//   本地化的可见文案（不是码原文）。
// - 问答最终提交 `zcode-pending-submit`（不支持的形态下缺失或 disabled）。
// - `sensitive: true` 的自由作答 `zcode-pending-sensitive-input` 必须是
//   `type=password`，且不落任何草稿缓存（卸载/重挂后值为空）。
// - 审批选项 `zcode-permission-option-<optionId>`：label 用宿主原文，点击回传的
//   optionId 是宿主原值；fullAccess（及其他永久/未知 kind）不渲染或禁用。
//
// 打断 / 模型档位（Slice B1c 契约；数据只来自新 SessionView.control/selection）：
// - 打断按钮 `zcode-interrupt`：只认唯一 primaryTurn 的前台执行 id，提交 tagged stop
//   （`expectedForegroundExecutionId` 精确等于该 id），绝不夹带 sendText。
// - 模型选择 `zcode-model-select`（选项 = 当前 provider 的 models）、思考档位
//   `zcode-thought-select`（选项 = 所选模型的 thoughtLevels）；model/thoughtLevel
//   任一能力关闭 → 两个控件都禁用并给出可见原因，改值/点击零动作 POST。
//
// 网络层替身：GET connection / GET session / POST actions 全部为合成 DTO；不读真实
// 账号、DB 或链接。断言「零旧 session-send / 零无头通道」= 旧发送链从未被触碰。

import { act, cleanup, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { afterAll, afterEach, beforeAll, beforeEach, describe, expect, it, vi } from "vitest";
import i18n from "@/i18n";
import enLocale from "@/i18n/locales/en.json";
import zhLocale from "@/i18n/locales/zh.json";
import ZcodeControlPanel, { ZCODE_REASON_KEYS } from "@/mobile/ZcodeControlPanel";
import {
  ZCODE_ACTIONS_PATH,
  ZCODE_CONNECTION_PATH,
  ZCODE_ERROR_CODES,
  ZCODE_SESSION_PATH,
  type ActionReceipt,
  type ActionState,
  type CapabilityView,
  type ConnectionView,
  type ControlAction,
  type PendingView,
  type QuestionView,
  type SessionView,
} from "@/lib/api/zcodeControl";
import { pinZhNavigator } from "./pinZhNavigator";
import { ZCODE_TAKEOVER_PATH } from "@/mobile/api";

const SID = "session_0000000000000000000000000000000a";
const GEN = 3;
/** pending 卡合成 interaction id（会话 + 行/工具锚 + interaction + generation 四元组）。 */
const PENDING_ID = "int_00000000000000000000000000000001";

let unpinZh: () => void;

beforeAll(async () => {
  unpinZh = await pinZhNavigator();
});

afterAll(() => {
  unpinZh();
});

const flag = (enabled: boolean) => ({ enabled, reasonCode: enabled ? null : "needs_evidence" });

/** 发送能力全部打开：本文件的禁用原因只允许来自连接/回执面，不能赖到能力矩阵。 */
function capabilities(): CapabilityView {
  return {
    send: flag(true),
    singleSelect: flag(true),
    multiSelect: flag(true),
    freeText: flag(true),
    interrupt: flag(true),
    approvalReply: flag(true),
    model: flag(true),
    thoughtLevel: flag(true),
  };
}

function connection(overrides: Partial<ConnectionView> = {}): ConnectionView {
  return {
    state: "paired",
    generation: GEN,
    reasonCode: null,
    deviceLabel: "Synthetic Mac",
    ...overrides,
  };
}

function sessionView(overrides: Partial<SessionView> = {}): SessionView {
  return {
    sessionId: SID,
    generation: GEN,
    revision: 12,
    ready: true,
    pending: [],
    control: { activeWorks: [] },
    selection: null,
    capabilities: capabilities(),
    ...overrides,
  };
}

/** pending 卡夹具：全合成值（interaction/锚/tool 名均为假数据，不读真实账号）。 */
function pendingView(overrides: Partial<PendingView> = {}): PendingView {
  return {
    interactionId: PENDING_ID,
    kind: "userInput",
    anchorRowId: 7,
    toolCallId: "call_00000000000000000000000000000001",
    createdAt: 1760000000000,
    summary: null,
    toolName: "AskUserQuestion",
    schemaInteraction: "ask_user_question",
    sensitive: false,
    autoResolution: false,
    autoResolutionState: null,
    questions: [],
    options: [],
    detail: null,
    ...overrides,
  };
}

/** 单个问答问题夹具（单选；自由作答按用例覆盖）。 */
function questionView(overrides: Partial<QuestionView> = {}): QuestionView {
  return {
    header: "下一步",
    questionText: "这个任务接下来要做什么？",
    options: [
      { value: "opt-continue", label: "继续实现", description: null, preview: null },
      { value: "opt-test-first", label: "先写测试", description: null, preview: null },
    ],
    multiSelect: false,
    freeTextAllowed: false,
    ...overrides,
  };
}

function json(body: unknown, status = 200): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { "content-type": "application/json" },
  });
}

function deferred<T>() {
  let resolve!: (value: T) => void;
  const promise = new Promise<T>((res) => {
    resolve = res;
  });
  return { promise, resolve };
}

function receipt(actionId: string, state: ActionState, reasonCode: string | null = null) {
  return {
    actionId,
    sessionId: SID,
    generation: GEN,
    state,
    reasonCode,
  } satisfies ActionReceipt;
}

let fetchMock: ReturnType<typeof vi.fn>;

/** 安装确定性网络替身；未登记 URL 直接抛（防「少调/多调一个接口」变成假阴性）。 */
function installFetch(handler: (url: string, init?: RequestInit) => Response | Promise<Response>) {
  fetchMock = vi.fn(async (input: RequestInfo | URL, init?: RequestInit) =>
    handler(String(input), init)
  );
  vi.stubGlobal("fetch", fetchMock);
}

/** 每张待处理卡的可寻址插槽（多组并存时同名内层 testid 会重复，必须 within(slot)）。 */
function slotOf(interactionId: string = PENDING_ID): HTMLElement {
  return screen.getByTestId(`zcode-pending-slot-${interactionId}`);
}

/** 卡内控件的定位基座：只在本组插槽里查，避免命中隐藏卡。 */
function cardIn(interactionId: string = PENDING_ID) {
  return within(slotOf(interactionId));
}

function calledUrls(): string[] {
  return fetchMock.mock.calls.map(([input]) => String(input));
}

/** 官方动作 POST（唯一允许的写入口）。 */
function postedActions(): { actionId: string; action: ControlAction }[] {
  return fetchMock.mock.calls
    .filter(
      ([input, init]) =>
        String(input) === ZCODE_ACTIONS_PATH && (init as RequestInit | undefined)?.method === "POST"
    )
    .map(
      ([, init]) =>
        JSON.parse(String((init as RequestInit).body)) as {
          actionId: string;
          action: ControlAction;
        }
    );
}

/** 旧发送/无头入口计数：本任务任何路径都不得回退它们。 */
const legacySendCalls = () =>
  calledUrls().filter((url) => url.includes("/session-send") || /headless/i.test(url)).length;

/** 已 POST 的审批动作（按 kind 过滤：允许实现先发 snooze 前缀，不把实现细节当断言）。 */
function permissionActions(): Extract<ControlAction, { kind: "permission" }>[] {
  return postedActions()
    .map(({ action }) => action)
    .filter((action): action is Extract<ControlAction, { kind: "permission" }> => {
      return action.kind === "permission";
    });
}

/** 浏览器存储快照：敏感作答不得落任何草稿缓存（localStorage/sessionStorage）。 */
function storageDump(): string {
  return [window.localStorage, window.sessionStorage]
    .map((storage) => {
      const entries: Array<[string, string]> = [];
      for (let i = 0; i < storage.length; i += 1) {
        const key = storage.key(i);
        if (key !== null) entries.push([key, storage.getItem(key) ?? ""]);
      }
      return JSON.stringify(entries);
    })
    .join("\n");
}

beforeEach(() => {
  // jsdom 无 EventSource；显式置空让 hook 走「GET 首拍权威」路径（本文件只测面板）。
  vi.stubGlobal("EventSource", undefined);
});

afterEach(async () => {
  cleanup();
  vi.restoreAllMocks();
  vi.unstubAllGlobals();
  // 双语用例会临时切到 en：逐例还原 zh，避免污染后续以 zh 文案断言的文件内用例。
  if (i18n.language !== "zh") await i18n.changeLanguage("zh");
});

describe("ZCode 控制面板：连接态与发送门", () => {
  it("saved_never_implies_connected", async () => {
    // 链接已保存但官方一侧未授权：服务端返回 needs_auth（不是 paired）。
    // 会话投影本身完全可读且 send 能力为 true —— 发送仍必须被连接态挡住。
    installFetch((url) => {
      if (url === ZCODE_CONNECTION_PATH) {
        return json(connection({ state: "needs_auth", reasonCode: "needs_auth" }));
      }
      if (url.startsWith(`${ZCODE_SESSION_PATH}?`)) return json(sessionView());
      throw new Error(`unexpected fetch: ${url}`);
    });

    render(<ZcodeControlPanel sessionId={SID} />);

    const reason = await screen.findByTestId("zcode-control-reason");
    await waitFor(() => expect(reason.getAttribute("data-reason-code")).toBe("needs_auth"));

    // 已保存链接 ≠ 已连接：不出现任何「已连接/已配对/paired」的断言性文案
    expect(screen.queryByText(/已连接/)).toBeNull();
    expect(screen.queryByText(/已配对/)).toBeNull();
    expect(screen.queryByText(/paired/i)).toBeNull();
    expect(screen.getByTestId("zcode-control-panel").getAttribute("data-connection-state")).toBe(
      "needs_auth"
    );

    // 就地原因可见且是可读文案，不是错误码原文
    expect(reason).toBeVisible();
    expect(reason.textContent?.trim().length ?? 0).toBeGreaterThan(0);
    expect(reason.textContent).toMatch(/授权/);

    // 发送禁用；即便点击也不产生任何动作 POST（禁用是硬门，不是样式）
    const send = screen.getByTestId("zcode-send") as HTMLButtonElement;
    expect(send).toBeDisabled();
    fireEvent.click(send);
    await act(async () => {});
    expect(postedActions()).toHaveLength(0);
  });
});

describe("ZCode 控制面板：发送只走官方动作接口", () => {
  it("send_uses_official_api_not_legacy_or_headless", async () => {
    installFetch((url, init) => {
      if (url === ZCODE_CONNECTION_PATH) return json(connection());
      if (url.startsWith(`${ZCODE_SESSION_PATH}?`)) return json(sessionView());
      if (url === ZCODE_ACTIONS_PATH && init?.method === "POST") {
        const body = JSON.parse(String(init.body)) as { actionId: string };
        return json(receipt(body.actionId, "accepted"));
      }
      throw new Error(`unexpected fetch: ${url}`);
    });

    render(<ZcodeControlPanel sessionId={SID} />);

    const draft = (await screen.findByTestId("zcode-draft")) as HTMLTextAreaElement;
    const send = screen.getByTestId("zcode-send") as HTMLButtonElement;
    await waitFor(() => expect(send).toBeEnabled());

    fireEvent.change(draft, { target: { value: "待发送草稿" } });
    fireEvent.click(send);

    await waitFor(() => expect(postedActions()).toHaveLength(1));

    // 唯一写入口 = 官方 POST /m/api/v1/zcode/actions，动作形状 = tagged send + 原文
    expect(ZCODE_ACTIONS_PATH).toBe("/m/api/v1/zcode/actions");
    const [posted] = postedActions();
    expect(posted.action).toEqual({
      kind: "send",
      sessionId: SID,
      generation: GEN,
      text: "待发送草稿",
    });
    // 零旧 session-send / 零无头 CLI：本任务不得把官方动作降级为终端按键
    expect(legacySendCalls()).toBe(0);
  });
});

describe("ZCode 控制面板：回执文案不越级", () => {
  it("accepted_and_unknown_keep_distinct_copy", async () => {
    let actionState: ActionState = "accepted";
    let reasonCode: string | null = null;
    installFetch((url, init) => {
      if (url === ZCODE_CONNECTION_PATH) return json(connection());
      if (url.startsWith(`${ZCODE_SESSION_PATH}?`)) return json(sessionView());
      if (url === ZCODE_ACTIONS_PATH && init?.method === "POST") {
        const body = JSON.parse(String(init.body)) as { actionId: string };
        return json(receipt(body.actionId, actionState, reasonCode));
      }
      throw new Error(`unexpected fetch: ${url}`);
    });

    render(<ZcodeControlPanel sessionId={SID} />);
    const draft = (await screen.findByTestId("zcode-draft")) as HTMLTextAreaElement;
    const send = screen.getByTestId("zcode-send") as HTMLButtonElement;
    await waitFor(() => expect(send).toBeEnabled());

    // accepted 只表示宿主已受理：文案恰好是「已受理」，绝不是「已生效」
    fireEvent.change(draft, { target: { value: "第一条草稿" } });
    fireEvent.click(send);
    const accepted = await screen.findByTestId("zcode-action-status");
    await waitFor(() => expect(accepted.getAttribute("data-action-state")).toBe("accepted"));
    expect(accepted.textContent?.trim()).toBe("已受理");
    expect(accepted.textContent).not.toContain("已生效");

    // unknown 是第三种语义：既不冒称已受理，也不冒称已生效
    actionState = "unknown";
    reasonCode = "unknown";
    fireEvent.change(draft, { target: { value: "第二条草稿" } });
    fireEvent.click(send);
    await waitFor(() =>
      expect(screen.getByTestId("zcode-action-status").getAttribute("data-action-state")).toBe(
        "unknown"
      )
    );
    const unknown = screen.getByTestId("zcode-action-status");
    expect(unknown.textContent?.trim().length ?? 0).toBeGreaterThan(0);
    expect(unknown.textContent).not.toContain("已生效");
    expect(unknown.textContent).not.toContain("已受理");
  });

  it("newly_typed_text_survives_late_accepted", async () => {
    // 发送 A 后在 accepted 回执到达前续写 B：迟到的 accepted 只能清掉「已提交的 A」，
    // 绝不能把回执到达时的新草稿 B 一并清掉（在途续写竞态）。
    const gate = deferred<Response>();
    installFetch((url, init) => {
      if (url === ZCODE_CONNECTION_PATH) return json(connection());
      if (url.startsWith(`${ZCODE_SESSION_PATH}?`)) return json(sessionView());
      if (url === ZCODE_ACTIONS_PATH && init?.method === "POST") {
        const body = JSON.parse(String(init.body)) as { actionId: string };
        return gate.promise.then(() => json(receipt(body.actionId, "accepted")));
      }
      throw new Error(`unexpected fetch: ${url}`);
    });

    render(<ZcodeControlPanel sessionId={SID} />);
    const draft = (await screen.findByTestId("zcode-draft")) as HTMLTextAreaElement;
    const send = screen.getByTestId("zcode-send") as HTMLButtonElement;
    await waitFor(() => expect(send).toBeEnabled());

    fireEvent.change(draft, { target: { value: "已提交的 A" } });
    fireEvent.click(send);
    await waitFor(() => expect(postedActions()).toHaveLength(1));
    expect(postedActions()[0].action).toMatchObject({ kind: "send", text: "已提交的 A" });

    // 回执尚未到达：用户继续写 B
    fireEvent.change(draft, { target: { value: "回执到达前续写的 B" } });
    await act(async () => {
      gate.resolve(json(receipt(postedActions()[0].actionId, "accepted")));
      await Promise.resolve();
    });
    await waitFor(() =>
      expect(screen.getByTestId("zcode-action-status").getAttribute("data-action-state")).toBe(
        "accepted"
      )
    );

    // 迟到的 accepted 不得清掉 B（草稿与已提交文本不同 → 保留）
    expect(draft).toHaveValue("回执到达前续写的 B");
  });

  it("previous_receipt_is_cleared_on_new_submit", async () => {
    let posts = 0;
    const secondGate = deferred<Response>();
    installFetch((url, init) => {
      if (url === ZCODE_CONNECTION_PATH) return json(connection());
      if (url.startsWith(`${ZCODE_SESSION_PATH}?`)) return json(sessionView());
      if (url === ZCODE_ACTIONS_PATH && init?.method === "POST") {
        posts += 1;
        const body = JSON.parse(String(init.body)) as { actionId: string };
        if (posts === 1) return json(receipt(body.actionId, "accepted"));
        return secondGate.promise;
      }
      throw new Error(`unexpected fetch: ${url}`);
    });

    render(<ZcodeControlPanel sessionId={SID} />);
    const draft = (await screen.findByTestId("zcode-draft")) as HTMLTextAreaElement;
    const send = screen.getByTestId("zcode-send") as HTMLButtonElement;
    await waitFor(() => expect(send).toBeEnabled());

    // 第一次：accepted（已受理）留在屏上
    fireEvent.change(draft, { target: { value: "第一条草稿" } });
    fireEvent.click(send);
    const accepted = await screen.findByTestId("zcode-action-status");
    await waitFor(() => expect(accepted.getAttribute("data-action-state")).toBe("accepted"));

    // 第二次：新提交一开始，旧回执必须消失——上一次的「已受理」不得贴在
    // 在途/失败/未知的新动作上（accepted 越级 = 本任务重点防的失真）。
    fireEvent.change(draft, { target: { value: "第二条草稿" } });
    await act(async () => {
      fireEvent.click(send);
      await Promise.resolve();
    });
    await waitFor(() => expect(postedActions()).toHaveLength(2));
    expect(screen.queryByTestId("zcode-action-status")).toBeNull();

    // 失败结果落地：只显示 send_failed，绝不与旧「已受理」并存
    await act(async () => {
      secondGate.resolve(json(receipt(postedActions()[1].actionId, "send_failed", "offline")));
      await Promise.resolve();
    });
    await waitFor(() =>
      expect(screen.getByTestId("zcode-action-status").getAttribute("data-action-state")).toBe(
        "send_failed"
      )
    );
    const status = screen.getByTestId("zcode-action-status");
    expect(status.textContent).not.toContain("已受理");
    expect(draft).toHaveValue("第二条草稿");
  });
});

describe("ZCode 控制面板：发送能力关闭时就地说明原因", () => {
  it("send_capability_disabled_shows_reason", async () => {
    // paired + ready，但 send 能力关闭（reasonCode = needs_evidence）：
    // 禁用按钮之外必须有一句可见解释，而不是让用户以为按钮坏了。
    const caps = capabilities();
    caps.send = flag(false);
    installFetch((url) => {
      if (url === ZCODE_CONNECTION_PATH) return json(connection());
      if (url.startsWith(`${ZCODE_SESSION_PATH}?`)) {
        return json(sessionView({ capabilities: caps }));
      }
      throw new Error(`unexpected fetch: ${url}`);
    });

    render(<ZcodeControlPanel sessionId={SID} />);
    const send = (await screen.findByTestId("zcode-send")) as HTMLButtonElement;
    await waitFor(() => expect(send).toBeDisabled());

    const reason = screen.getByTestId("zcode-send-reason");
    expect(reason).toBeVisible();
    expect(reason.getAttribute("data-reason-code")).toBe("needs_evidence");
    expect(reason.textContent?.trim().length ?? 0).toBeGreaterThan(0);
    expect(reason.textContent).not.toContain("needs_evidence");

    fireEvent.click(send);
    await act(async () => {});
    expect(postedActions()).toHaveLength(0);
  });
});

describe("ZCode 控制面板：失败不吞草稿、不拖垮阅读面", () => {
  it("failure_preserves_draft_reading_stays_visible", async () => {
    installFetch((url, init) => {
      if (url === ZCODE_CONNECTION_PATH) return json(connection());
      if (url.startsWith(`${ZCODE_SESSION_PATH}?`)) return json(sessionView());
      if (url === ZCODE_ACTIONS_PATH && init?.method === "POST") {
        const body = JSON.parse(String(init.body)) as { actionId: string };
        return json(receipt(body.actionId, "send_failed", "offline"));
      }
      throw new Error(`unexpected fetch: ${url}`);
    });

    render(
      <>
        <div data-testid="zcode-reading-transcript">合成正文乙</div>
        <ZcodeControlPanel sessionId={SID} />
      </>
    );

    const draft = (await screen.findByTestId("zcode-draft")) as HTMLTextAreaElement;
    const send = screen.getByTestId("zcode-send") as HTMLButtonElement;
    await waitFor(() => expect(send).toBeEnabled());

    fireEvent.change(draft, { target: { value: "待发送草稿" } });
    fireEvent.click(send);

    await waitFor(() =>
      expect(screen.getByTestId("zcode-action-status").getAttribute("data-action-state")).toBe(
        "send_failed"
      )
    );

    // 草稿必须原样保留（失败的是发送，不是用户输入）
    expect(draft).toHaveValue("待发送草稿");

    // 同页阅读面（正文/文件预览走的是既有读取链）不因控制失败消失
    const reading = screen.getByTestId("zcode-reading-transcript");
    expect(reading).toBeVisible();
    expect(reading.textContent).toContain("合成正文乙");

    // 就地原因如实呈现；失败不触发自动重发（等待一个事件循环窗后仍只有一次 POST）
    const reason = screen.getByTestId("zcode-control-reason");
    expect(reason.getAttribute("data-reason-code")).toBe("offline");
    await act(async () => {
      await new Promise((resolve) => setTimeout(resolve, 20));
    });
    expect(postedActions()).toHaveLength(1);
  });
});

describe("ZCode 控制面板：不受支持的问答形态只读并说明原因", () => {
  it("unsupported_shape_is_disabled_with_reason", async () => {
    // singleSelect 能力关闭（reasonCode = needs_evidence）：题目仍可读，作答面只读。
    const caps = capabilities();
    caps.singleSelect = flag(false);
    installFetch((url) => {
      if (url === ZCODE_CONNECTION_PATH) return json(connection());
      if (url.startsWith(`${ZCODE_SESSION_PATH}?`)) {
        return json(
          sessionView({
            capabilities: caps,
            pending: [pendingView({ questions: [questionView()] })],
          })
        );
      }
      throw new Error(`unexpected fetch: ${url}`);
    });

    render(<ZcodeControlPanel sessionId={SID} />);

    // 阅读面照常：题干文本可见（不支持的是作答通道，不是题目本身）
    await screen.findByTestId(`zcode-pending-slot-${PENDING_ID}`);
    expect(cardIn().getByTestId("zcode-pending-card").getAttribute("data-interaction-id")).toBe(
      PENDING_ID
    );
    expect(screen.getByText(/这个任务接下来要做什么？/)).toBeVisible();

    // 选项只读：按钮禁用，点击零动作（不把官方 pending 喂给旧终端按键链）
    const option = cardIn().getByTestId("zcode-pending-option-0") as HTMLButtonElement;
    expect(option).toBeDisabled();
    fireEvent.click(option);
    await act(async () => {});
    expect(postedActions()).toHaveLength(0);
    expect(legacySendCalls()).toBe(0);

    // 就地原因：可见的本地化文案 + 冻结码属性（码本身不是给用户看的）
    const reason = cardIn().getByTestId("zcode-pending-reason");
    expect(reason).toBeVisible();
    expect(reason.getAttribute("data-reason-code")).toBe("needs_evidence");
    expect(reason.textContent?.trim().length ?? 0).toBeGreaterThan(0);
    expect(reason.textContent).not.toContain("needs_evidence");

    // 没有可用的最终提交钮（不支持的形态下缺失或 disabled——绝不假装能提交）
    const submit = cardIn().queryByTestId("zcode-pending-submit");
    if (submit !== null) expect(submit).toBeDisabled();
  });
});

describe("ZCode 控制面板：敏感作答用密码框且不留草稿", () => {
  it("sensitive_answer_is_password_and_not_cached", async () => {
    installFetch((url) => {
      if (url === ZCODE_CONNECTION_PATH) return json(connection());
      if (url.startsWith(`${ZCODE_SESSION_PATH}?`)) {
        return json(
          sessionView({
            pending: [
              pendingView({
                sensitive: true,
                questions: [questionView({ freeTextAllowed: true })],
              }),
            ],
          })
        );
      }
      throw new Error(`unexpected fetch: ${url}`);
    });

    const first = render(<ZcodeControlPanel sessionId={SID} />);
    await screen.findByTestId(`zcode-pending-slot-${PENDING_ID}`);
    const input = cardIn().getByTestId("zcode-pending-sensitive-input") as HTMLInputElement;
    // 敏感值只走密码框，不在明文输入面回显
    expect(input.type).toBe("password");

    fireEvent.change(input, { target: { value: "合成敏感答案" } });
    expect(input.value).toBe("合成敏感答案");
    // 敏感作答不落任何浏览器存储（普通 composer 的草稿缓存不得染指这里）
    expect(storageDump()).not.toContain("合成敏感答案");

    // 卸载重挂：没有草稿缓存，输入从空开始（事实源只在终端/宿主）
    first.unmount();
    render(<ZcodeControlPanel sessionId={SID} />);
    const remounted = (await screen.findByTestId(
      "zcode-pending-sensitive-input"
    )) as HTMLInputElement;
    expect(remounted.value).toBe("");
    expect(storageDump()).not.toContain("合成敏感答案");
  });
});

describe("ZCode 控制面板：审批选项回传宿主原值", () => {
  it("permission_options_use_host_values_and_hide_full_access", async () => {
    const pending = pendingView({
      kind: "permission",
      toolName: "Bash",
      summary: "执行合成命令",
      questions: [],
      options: [
        { optionId: "host-allow-once-9", label: "允许一次", kind: "allowOnce" },
        { optionId: "host-deny-9", label: "拒绝", kind: "deny" },
        { optionId: "host-full-access-9", label: "始终允许", kind: "fullAccess" },
      ],
    });
    installFetch((url, init) => {
      if (url === ZCODE_CONNECTION_PATH) return json(connection());
      if (url.startsWith(`${ZCODE_SESSION_PATH}?`)) {
        return json(sessionView({ pending: [pending] }));
      }
      if (url === ZCODE_ACTIONS_PATH && init?.method === "POST") {
        const body = JSON.parse(String(init.body)) as { actionId: string };
        return json(receipt(body.actionId, "accepted"));
      }
      throw new Error(`unexpected fetch: ${url}`);
    });

    render(<ZcodeControlPanel sessionId={SID} />);

    // 展示文案保留宿主原文；选项从卡渲染之后才可点
    expect(await screen.findByText("允许一次")).toBeVisible();
    expect(screen.getByText("拒绝")).toBeVisible();

    // fullAccess（永久授权）不可用：不渲染，或渲染为禁用按钮——绝不能可点
    const fullAccess = screen.queryByTestId("zcode-permission-option-host-full-access-9");
    if (fullAccess !== null) expect(fullAccess).toBeDisabled();
    const fullAccessLabel = screen.queryByText("始终允许");
    if (fullAccessLabel !== null) expect(fullAccessLabel.closest("button")).toBeDisabled();

    // 点击 allowOnce：回传宿主原 optionId（不是 kind 字面量，也不是 index）
    fireEvent.click(cardIn().getByTestId("zcode-permission-option-host-allow-once-9"));
    await waitFor(() => expect(permissionActions()).toHaveLength(1));
    expect(permissionActions()[0]).toEqual({
      kind: "permission",
      sessionId: SID,
      generation: GEN,
      interactionId: PENDING_ID,
      optionId: "host-allow-once-9",
    });
    expect(permissionActions()[0].optionId).not.toBe("allowOnce");
    expect(legacySendCalls()).toBe(0);
  });
});

describe("ZCode 控制面板：无可选审批项时就地说明原因", () => {
  it("permission_card_without_selectable_option_shows_reason", async () => {
    // 宿主只给了 fullAccess / 未知 kind：面板一律不回传永久授权，但也不能让卡片
    // 只剩摘要、无按钮也无原因（用户会以为卡坏了）。就地原因走冻结码属性。
    const pending = pendingView({
      kind: "permission",
      toolName: "Bash",
      summary: "执行合成命令",
      questions: [],
      options: [
        { optionId: "host-full-access-9", label: "始终允许", kind: "fullAccess" },
        { optionId: "host-unknown-9", label: "未知项", kind: "other" },
      ],
    });
    installFetch((url) => {
      if (url === ZCODE_CONNECTION_PATH) return json(connection());
      if (url.startsWith(`${ZCODE_SESSION_PATH}?`)) {
        return json(sessionView({ pending: [pending] }));
      }
      throw new Error(`unexpected fetch: ${url}`);
    });

    render(<ZcodeControlPanel sessionId={SID} />);
    expect(await screen.findByText("执行合成命令")).toBeVisible();

    // 不可回传的选项一个按钮都不渲染（永久授权绝不落成可点按钮）
    expect(screen.queryByTestId("zcode-permission-option-host-full-access-9")).toBeNull();
    expect(screen.queryByTestId("zcode-permission-option-host-unknown-9")).toBeNull();

    // 就地原因：可见的本地化文案 + 冻结码属性（码本身不是给用户看的）
    const reason = cardIn().getByTestId("zcode-pending-reason");
    expect(reason).toBeVisible();
    expect(reason.getAttribute("data-reason-code")).toBe("unsupported_protocol");
    expect(reason.textContent?.trim().length ?? 0).toBeGreaterThan(0);
    expect(reason.textContent).not.toContain("unsupported_protocol");
  });
});

describe("ZCode 控制面板：打断只针对唯一的既有前台执行", () => {
  it("interrupt_only_targets_user_selected_primary_execution", async () => {
    installFetch((url, init) => {
      if (url === ZCODE_CONNECTION_PATH) return json(connection());
      if (url.startsWith(`${ZCODE_SESSION_PATH}?`)) {
        return json(
          sessionView({
            control: { activeWorks: [{ kind: "primaryTurn", foregroundExecutionId: "E1" }] },
          })
        );
      }
      if (url === ZCODE_ACTIONS_PATH && init?.method === "POST") {
        const body = JSON.parse(String(init.body)) as { actionId: string };
        return json(receipt(body.actionId, "accepted"));
      }
      throw new Error(`unexpected fetch: ${url}`);
    });

    render(<ZcodeControlPanel sessionId={SID} />);

    const interrupt = (await screen.findByTestId("zcode-interrupt")) as HTMLButtonElement;
    await waitFor(() => expect(interrupt).toBeEnabled());
    fireEvent.click(interrupt);

    await waitFor(() => expect(postedActions()).toHaveLength(1));
    // 打断目标 = 新鲜 control 里唯一 primaryTurn 的执行 id（不是最新执行、不是猜）
    expect(postedActions()[0].action).toEqual({
      kind: "stop",
      sessionId: SID,
      generation: GEN,
      expectedForegroundExecutionId: "E1",
    });
    // 打断绝不夹带 sendText：不发任何 send 动作，也不回退旧发送链
    expect(postedActions().filter(({ action }) => action.kind === "send")).toHaveLength(0);
    expect(legacySendCalls()).toBe(0);
  });
});

describe("ZCode 控制面板：写面门禁（paired + ready）", () => {
  it("stale_projection_actions_are_disabled_when_not_ready", async () => {
    // 连接仍 paired，但服务端把同一投影置 ready=false（relay 掉线/`stale_session`）：
    // 陈旧投影一律不可点——作答/审批/模型档位/打断零动作 POST。
    const selection = {
      provider: "zai",
      model: "model-a",
      thought: "high",
      models: [{ modelId: "model-a", thoughtLevels: ["low", "high"] }],
    };
    installFetch((url) => {
      if (url === ZCODE_CONNECTION_PATH) return json(connection());
      if (url.startsWith(`${ZCODE_SESSION_PATH}?`)) {
        return json(
          sessionView({
            ready: false,
            pending: [
              pendingView({
                kind: "permission",
                toolName: "Bash",
                summary: "执行合成命令",
                options: [{ optionId: "host-allow-once-9", label: "允许一次", kind: "allowOnce" }],
              }),
              pendingView({
                interactionId: "int_00000000000000000000000000000002",
                questions: [questionView({ freeTextAllowed: true })],
              }),
            ],
            control: { activeWorks: [{ kind: "primaryTurn", foregroundExecutionId: "E1" }] },
            selection,
          })
        );
      }
      throw new Error(`unexpected fetch: ${url}`);
    });

    render(<ZcodeControlPanel sessionId={SID} />);

    // R3：多组并存只展示一张「当前交互卡」。首张 = 审批卡，第二张（问答）由切组
    // 小标签切换过去；两张卡的阅读面都照常可读，作答面一律禁用。
    const tabs = await screen.findByTestId("zcode-pending-tabs");
    expect(tabs).toBeVisible();
    expect(
      screen.getByTestId("zcode-pending-tab-int_00000000000000000000000000000001")
    ).toBeVisible();
    expect(
      screen.getByTestId("zcode-pending-tab-int_00000000000000000000000000000002")
    ).toBeVisible();
    expect(screen.getByText("执行合成命令")).toBeVisible();

    // 非当前组：容器 hidden + inert，但仍在册。
    // 口径（务必如实）：jsdom **未实现 inert 的交互语义**（其 focusing.js 里 inert
    // 只有 TODO：不排除焦点、不阻断点击），所以下面的 inert 断言只是**属性级**——
    // 它能证伪「属性被漏掉」，但**不能**证明真实浏览器里的 Tab/读屏阻断。
    // 自动回环里真正可证伪的隔离护栏是 hidden（见 not.toBeVisible()）+ 组内控件
    // 显式禁用断言；浏览器侧语义列入实机待验。
    const permissionSlot = slotOf("int_00000000000000000000000000000001");
    const questionSlot = slotOf("int_00000000000000000000000000000002");
    expect(permissionSlot.hasAttribute("hidden")).toBe(false);
    expect(permissionSlot.hasAttribute("inert")).toBe(false);
    expect(questionSlot.hasAttribute("hidden")).toBe(true);
    expect(questionSlot.hasAttribute("inert")).toBe(true);

    // 可见组内的按钮**确实**被写面门禁禁用（不是靠「隐藏所以点不到」过关）。
    const permission = cardIn("int_00000000000000000000000000000001").getByTestId(
      "zcode-permission-option-host-allow-once-9"
    ) as HTMLButtonElement;
    expect(permission).toBeVisible();
    expect(permission).toBeDisabled();
    fireEvent.click(permission);
    expect(postedActions()).toHaveLength(0);

    // 未切换的问答卡不显示（但仍在册，不是被丢弃）
    expect(screen.getByText("这个任务接下来要做什么？")).not.toBeVisible();

    fireEvent.click(screen.getByTestId("zcode-pending-tab-int_00000000000000000000000000000002"));
    expect(screen.getByText("这个任务接下来要做什么？")).toBeVisible();
    expect(screen.getByText("执行合成命令")).not.toBeVisible();
    expect(questionSlot.hasAttribute("inert")).toBe(false);
    const option = cardIn("int_00000000000000000000000000000002").getByTestId(
      "zcode-pending-option-0"
    ) as HTMLButtonElement;
    const submit = cardIn("int_00000000000000000000000000000002").getByTestId(
      "zcode-pending-submit"
    ) as HTMLButtonElement;
    const model = screen.getByTestId("zcode-model-select") as HTMLSelectElement;
    const thought = screen.getByTestId("zcode-thought-select") as HTMLSelectElement;
    const interrupt = screen.getByTestId("zcode-interrupt") as HTMLButtonElement;
    for (const control of [option, submit, model, thought, interrupt]) {
      expect(control).toBeVisible();
      expect(control).toBeDisabled();
    }

    // 可见原因统一为 not_ready（冻结码只进 data-reason-code）。
    for (const reason of screen.getAllByTestId("zcode-pending-reason")) {
      expect(reason.getAttribute("data-reason-code")).toBe("not_ready");
      expect(reason.textContent?.trim().length ?? 0).toBeGreaterThan(0);
      expect(reason.textContent).not.toContain("not_ready");
    }
    expect(screen.getByTestId("zcode-selection-reason").getAttribute("data-reason-code")).toBe(
      "not_ready"
    );
    expect(screen.getByTestId("zcode-interrupt-reason").getAttribute("data-reason-code")).toBe(
      "not_ready"
    );

    // 可见组内的点击/改值均零动作 POST（隐藏组不参与本断言）。
    fireEvent.click(option);
    fireEvent.click(submit);
    fireEvent.click(interrupt);
    fireEvent.change(model, { target: { value: "model-a" } });
    fireEvent.change(thought, { target: { value: "low" } });
    await act(async () => {});
    expect(postedActions()).toHaveLength(0);
    expect(legacySendCalls()).toBe(0);
  });
});

describe("ZCode 控制面板：官方页面是显式辅助动作", () => {
  const OFFICIAL_URL = "https://zcode.z.ai/remote/v4?sid=synthetic-panel-official-0001";

  it("official_page_is_an_explicit_auxiliary_action", async () => {
    const openSpy = vi.spyOn(window, "open").mockImplementation(() => null);
    installFetch((url) => {
      if (url.includes("/official-control")) {
        return json({ configState: "saved", url: OFFICIAL_URL });
      }
      if (url === ZCODE_CONNECTION_PATH) {
        return json(connection({ state: "needs_auth", reasonCode: "needs_auth" }));
      }
      if (url.startsWith(`${ZCODE_SESSION_PATH}?`)) return json(sessionView());
      throw new Error(`unexpected fetch: ${url}`);
    });

    render(<ZcodeControlPanel sessionId={SID} />);

    // 挂载不自动外跳：入口必须由用户显式点击。
    const link = await screen.findByTestId("zcode-open-official");
    expect(openSpy).not.toHaveBeenCalled();

    // 点击：用**已保存**链接在新标签页打开，且不代理任何控制/读取请求。
    const callsBefore = fetchMock.mock.calls.length;
    fireEvent.click(link);
    expect(openSpy).toHaveBeenCalledWith(OFFICIAL_URL, "_blank", "noopener,noreferrer");
    expect(fetchMock.mock.calls).toHaveLength(callsBefore);
    expect(postedActions()).toHaveLength(0);

    // 无已存链接（未配置/坏值）→ 没有可开的入口。
    cleanup();
    installFetch((url) => {
      if (url.includes("/official-control")) {
        return json({ configState: "missing", url: null });
      }
      if (url === ZCODE_CONNECTION_PATH) {
        return json(connection({ state: "needs_config", reasonCode: "needs_config" }));
      }
      if (url.startsWith(`${ZCODE_SESSION_PATH}?`)) return json(sessionView());
      throw new Error(`unexpected fetch: ${url}`);
    });
    render(<ZcodeControlPanel sessionId={SID} />);
    await screen.findByTestId("zcode-control-panel");
    await act(async () => {});
    expect(screen.queryByTestId("zcode-open-official")).toBeNull();
  });
});

describe("ZCode 控制面板：同帧双击与敏感输入清理", () => {
  it("double_click_sends_one_action", async () => {
    installFetch((url, init) => {
      if (url === ZCODE_CONNECTION_PATH) return json(connection());
      if (url.startsWith(`${ZCODE_SESSION_PATH}?`)) return json(sessionView());
      if (url === ZCODE_ACTIONS_PATH && init?.method === "POST") {
        const body = JSON.parse(String(init.body)) as { actionId: string };
        return json(receipt(body.actionId, "accepted"));
      }
      throw new Error(`unexpected fetch: ${url}`);
    });

    render(<ZcodeControlPanel sessionId={SID} />);
    const draft = (await screen.findByTestId("zcode-draft")) as HTMLTextAreaElement;
    const send = screen.getByTestId("zcode-send") as HTMLButtonElement;
    await waitFor(() => expect(send).toBeEnabled());
    fireEvent.change(draft, { target: { value: "只发一次" } });

    // 同一渲染帧双击（两次点击之间没有重渲染）：同步闸必须只放行一次提交。
    await act(async () => {
      send.click();
      send.click();
    });
    await waitFor(() => expect(postedActions()).toHaveLength(1));
    expect(postedActions()).toHaveLength(1);
  });

  it("sensitive_input_clears_on_unmount_or_blur", async () => {
    installFetch((url) => {
      if (url === ZCODE_CONNECTION_PATH) return json(connection());
      if (url.startsWith(`${ZCODE_SESSION_PATH}?`)) {
        return json(
          sessionView({
            pending: [
              pendingView({
                sensitive: true,
                questions: [questionView({ freeTextAllowed: true })],
              }),
            ],
          })
        );
      }
      throw new Error(`unexpected fetch: ${url}`);
    });

    const first = render(<ZcodeControlPanel sessionId={SID} />);
    await screen.findByTestId(`zcode-pending-slot-${PENDING_ID}`);
    const input = cardIn().getByTestId("zcode-pending-sensitive-input") as HTMLInputElement;
    expect(input.type).toBe("password");
    fireEvent.change(input, { target: { value: "合成敏感答案" } });
    expect(input.value).toBe("合成敏感答案");

    // 失焦即清空可见值，且不落任何浏览器存储。
    fireEvent.blur(input);
    expect(input.value).toBe("");
    expect(storageDump()).not.toContain("合成敏感答案");

    // 重挂同样从空开始（事实源只在终端/宿主）。
    first.unmount();
    render(<ZcodeControlPanel sessionId={SID} />);
    const remounted = (await screen.findByTestId(
      "zcode-pending-sensitive-input"
    )) as HTMLInputElement;
    expect(remounted.value).toBe("");
    expect(storageDump()).not.toContain("合成敏感答案");
  });
});

describe("ZCode 控制面板：pending 卡提交防重与选中态", () => {
  it("pending_cannot_be_resubmitted_with_new_action_id", async () => {
    // 作答卡首次 POST 挂起：同帧双击只发一次；受理后卡面锁死，新点击绝不生成
    // 第二个 actionId 重发同一份答案/审批（宿主 pending/session 刷新后才解锁）。
    const firstGate = deferred<Response>();
    let posts = 0;
    installFetch((url, init) => {
      if (url === ZCODE_CONNECTION_PATH) return json(connection());
      if (url.startsWith(`${ZCODE_SESSION_PATH}?`)) {
        return json(
          sessionView({
            pending: [
              pendingView({ questions: [questionView()] }),
              pendingView({
                interactionId: "int_00000000000000000000000000000002",
                kind: "permission",
                toolName: "Bash",
                summary: "执行合成命令",
                options: [{ optionId: "host-allow-once-9", label: "允许一次", kind: "allowOnce" }],
              }),
            ],
          })
        );
      }
      if (url === ZCODE_ACTIONS_PATH && init?.method === "POST") {
        posts += 1;
        const body = JSON.parse(String(init.body)) as { actionId: string };
        if (posts === 1) return firstGate.promise;
        return json(receipt(body.actionId, "accepted"));
      }
      throw new Error(`unexpected fetch: ${url}`);
    });

    render(<ZcodeControlPanel sessionId={SID} />);
    // R3：提交按钮只在确认页可用 —— 先选中 → 进入确认页，再双击提交。
    await screen.findByTestId(`zcode-pending-slot-${PENDING_ID}`);
    const submit = cardIn().getByTestId("zcode-pending-submit") as HTMLButtonElement;
    const option = cardIn().getByTestId("zcode-pending-option-0") as HTMLButtonElement;
    await waitFor(() => expect(option).toBeEnabled());
    expect(submit).toBeDisabled();
    fireEvent.click(option);
    fireEvent.click(cardIn().getByTestId("zcode-pending-next"));
    await waitFor(() => expect(submit).toBeEnabled());

    // 同帧双击「提交」：一个在途槽只放行一次 POST。
    await act(async () => {
      submit.click();
      submit.click();
    });
    await waitFor(() => expect(postedActions()).toHaveLength(1));

    // 受理回执落地：卡面进入「已受理」锁定态，按钮不可再点。
    await act(async () => {
      firstGate.resolve(json(receipt(postedActions()[0].actionId, "accepted")));
      await Promise.resolve();
    });
    await waitFor(() => expect(submit).toBeDisabled());
    expect(cardIn().getByTestId("zcode-pending-notice").textContent).toBe("已受理");

    fireEvent.click(submit);
    await act(async () => {});
    expect(postedActions()).toHaveLength(1);
    expect(postedActions()[0].action.kind).toBe("answer");

    // 审批卡同理：切到第二组后受理一次，再次点击不得产生新的 actionId。
    fireEvent.click(screen.getByTestId("zcode-pending-tab-int_00000000000000000000000000000002"));
    const allow = screen.getByTestId(
      "zcode-permission-option-host-allow-once-9"
    ) as HTMLButtonElement;
    await waitFor(() => expect(allow).toBeEnabled());
    fireEvent.click(allow);
    await waitFor(() => expect(postedActions()).toHaveLength(2));
    await waitFor(() => expect(allow).toBeDisabled());
    fireEvent.click(allow);
    await act(async () => {});
    expect(postedActions()).toHaveLength(2);
    expect(postedActions()[1].action.kind).toBe("permission");

    // 两次受理各用各的 actionId（同一次提交绝不换 ID 重发）。
    const ids = postedActions().map(({ actionId }) => actionId);
    expect(new Set(ids).size).toBe(2);
  });

  it("snooze_inflight_blocks_submit_with_reason", async () => {
    // autoResolution：首次编辑先发 snooze；在其在途期间用户点「提交」不得被静默
    // 吞掉——按钮禁用且给出可见原因，snooze 受理后最终提交仍可发出。
    const snoozeGate = deferred<Response>();
    installFetch((url, init) => {
      if (url === ZCODE_CONNECTION_PATH) return json(connection());
      if (url.startsWith(`${ZCODE_SESSION_PATH}?`)) {
        return json(
          sessionView({
            pending: [pendingView({ autoResolution: true, questions: [questionView()] })],
          })
        );
      }
      if (url === ZCODE_ACTIONS_PATH && init?.method === "POST") {
        const body = JSON.parse(String(init.body)) as {
          actionId: string;
          action: ControlAction;
        };
        if (body.action.kind === "snooze") return snoozeGate.promise;
        return json(receipt(body.actionId, "accepted"));
      }
      throw new Error(`unexpected fetch: ${url}`);
    });

    render(<ZcodeControlPanel sessionId={SID} />);
    await screen.findByTestId(`zcode-pending-slot-${PENDING_ID}`);
    const option = cardIn().getByTestId("zcode-pending-option-0") as HTMLButtonElement;
    const submit = cardIn().getByTestId("zcode-pending-submit") as HTMLButtonElement;
    await waitFor(() => expect(option).toBeEnabled());
    // R3：未进确认页时提交恒禁用（本地暂存，不提前上送）
    expect(submit).toBeDisabled();

    fireEvent.click(option);
    await waitFor(() =>
      expect(postedActions().some(({ action }) => action.kind === "snooze")).toBe(true)
    );

    // snooze 在途：提交按钮禁用 + 可见「正在暂停计时」原因（不是吞点击）。
    await waitFor(() => expect(submit).toBeDisabled());
    const notice = cardIn().getByTestId("zcode-pending-notice");
    expect(notice).toBeVisible();
    expect(notice.textContent).toContain("正在暂停计时");
    fireEvent.click(submit);
    await act(async () => {});
    expect(postedActions().filter(({ action }) => action.kind === "answer")).toHaveLength(0);

    // snooze 受理后计时提示消失；确认页（R3 唯一提交入口）才放行最终提交。
    await act(async () => {
      snoozeGate.resolve(json(receipt(postedActions()[0].actionId, "accepted")));
      await Promise.resolve();
    });
    await waitFor(() => expect(cardIn().queryByTestId("zcode-pending-notice")).toBeNull());
    // 题目页仍不可直接提交：必须先进入确认页（本地暂存不提前上送）。
    expect(submit).toBeDisabled();
    fireEvent.click(cardIn().getByTestId("zcode-pending-next"));
    await waitFor(() => expect(submit).toBeEnabled());

    fireEvent.click(submit);
    await waitFor(() =>
      expect(postedActions().filter(({ action }) => action.kind === "answer")).toHaveLength(1)
    );
    const answer = postedActions().find(({ action }) => action.kind === "answer");
    expect(answer?.action).toEqual({
      kind: "answer",
      sessionId: SID,
      generation: GEN,
      interactionId: PENDING_ID,
      answers: [
        {
          questionText: "这个任务接下来要做什么？",
          selectedValues: ["opt-continue"],
          freeText: null,
        },
      ],
    });
  });

  it("multi_select_submits_multiple_host_values", async () => {
    // multiSelect=true：点选是 toggle（多值），提交上行 selectedValues 数组
    // （宿主原值，不是 kind/index）；全部取消后提交零动作（空数组绝不上行）。
    installFetch((url, init) => {
      if (url === ZCODE_CONNECTION_PATH) return json(connection());
      if (url.startsWith(`${ZCODE_SESSION_PATH}?`)) {
        return json(
          sessionView({
            pending: [pendingView({ questions: [questionView({ multiSelect: true })] })],
          })
        );
      }
      if (url === ZCODE_ACTIONS_PATH && init?.method === "POST") {
        const body = JSON.parse(String(init.body)) as { actionId: string };
        return json(receipt(body.actionId, "accepted"));
      }
      throw new Error(`unexpected fetch: ${url}`);
    });

    render(<ZcodeControlPanel sessionId={SID} />);
    await screen.findByTestId(`zcode-pending-slot-${PENDING_ID}`);
    const first = cardIn().getByTestId("zcode-pending-option-0") as HTMLButtonElement;
    const second = cardIn().getByTestId("zcode-pending-option-1") as HTMLButtonElement;
    const submit = cardIn().getByTestId("zcode-pending-submit") as HTMLButtonElement;

    // 选中再取消 → 空选择：提交零动作（空 selectedValues 不得冒充作答）
    fireEvent.click(first);
    await waitFor(() => expect(first).toHaveAttribute("aria-pressed", "true"));
    fireEvent.click(first);
    await waitFor(() => expect(first).toHaveAttribute("aria-pressed", "false"));
    fireEvent.click(submit);
    await act(async () => {});
    expect(postedActions()).toHaveLength(0);

    // 多选是累加（不是替换）：两项同时选中，提交带上两项宿主原值
    fireEvent.click(first);
    fireEvent.click(second);
    await waitFor(() => expect(first).toHaveAttribute("aria-pressed", "true"));
    expect(second).toHaveAttribute("aria-pressed", "true");

    // R3：先进入确认页（确认页显示两题全部答案）再提交，只发一次 answer。
    fireEvent.click(cardIn().getByTestId("zcode-pending-next"));
    expect(cardIn().getByTestId("zcode-pending-confirm-answer-0").textContent).toContain(
      "继续实现"
    );
    fireEvent.click(submit);
    await waitFor(() => expect(postedActions()).toHaveLength(1));
    expect(postedActions()[0].action).toEqual({
      kind: "answer",
      sessionId: SID,
      generation: GEN,
      interactionId: PENDING_ID,
      answers: [
        {
          questionText: "这个任务接下来要做什么？",
          selectedValues: ["opt-continue", "opt-test-first"],
          freeText: null,
        },
      ],
    });
  });

  it("option_selection_exposes_aria_pressed", async () => {
    // 选中态必须对辅助技术可见：单选按钮用 aria-pressed 表达选中，而不是只写不渲染。
    installFetch((url) => {
      if (url === ZCODE_CONNECTION_PATH) return json(connection());
      if (url.startsWith(`${ZCODE_SESSION_PATH}?`)) {
        return json(sessionView({ pending: [pendingView({ questions: [questionView()] })] }));
      }
      throw new Error(`unexpected fetch: ${url}`);
    });

    render(<ZcodeControlPanel sessionId={SID} />);
    await screen.findByTestId(`zcode-pending-slot-${PENDING_ID}`);
    const first = cardIn().getByTestId("zcode-pending-option-0") as HTMLButtonElement;
    const second = cardIn().getByTestId("zcode-pending-option-1") as HTMLButtonElement;
    expect(first).toHaveAttribute("aria-pressed", "false");
    expect(second).toHaveAttribute("aria-pressed", "false");

    fireEvent.click(second);
    await waitFor(() => expect(second).toHaveAttribute("aria-pressed", "true"));
    expect(first).toHaveAttribute("aria-pressed", "false");

    // 改选另一项：选中态跟随，旧项回到未选中。
    fireEvent.click(first);
    await waitFor(() => expect(first).toHaveAttribute("aria-pressed", "true"));
    expect(second).toHaveAttribute("aria-pressed", "false");
  });
});

describe("ZCode 控制面板：选项与自由文本互斥", () => {
  /** 单题（freeTextAllowed）作答卡网络替身：POST 一律 accepted，便于断言上行载荷。 */
  function installAnswerBackend(options: { sensitive?: boolean } = {}) {
    installFetch((url, init) => {
      if (url === ZCODE_CONNECTION_PATH) return json(connection());
      if (url.startsWith(`${ZCODE_SESSION_PATH}?`)) {
        return json(
          sessionView({
            pending: [
              pendingView({
                sensitive: options.sensitive ?? false,
                questions: [questionView({ freeTextAllowed: true })],
              }),
            ],
          })
        );
      }
      if (url === ZCODE_ACTIONS_PATH && init?.method === "POST") {
        const body = JSON.parse(String(init.body)) as { actionId: string };
        return json(receipt(body.actionId, "accepted"));
      }
      throw new Error(`unexpected fetch: ${url}`);
    });
  }

  /** 已 POST 的 answer 动作（唯一允许的作答出口）里的 answers 数组。 */
  function submittedAnswers() {
    const answer = postedActions().find(({ action }) => action.kind === "answer");
    expect(answer).toBeDefined();
    return (answer!.action as Extract<ControlAction, { kind: "answer" }>).answers;
  }

  it("typing_free_text_replaces_selected_option", async () => {
    // 先点选项再打字：自由文本 = 本题唯一答案，选中项必须被清掉（不能两类同时上行，
    // 后端会以 invalid_answer 拒绝整次提交）。
    installAnswerBackend();
    render(<ZcodeControlPanel sessionId={SID} />);

    await screen.findByTestId(`zcode-pending-slot-${PENDING_ID}`);
    const freeText = cardIn().getByRole("textbox") as HTMLInputElement;
    const option = cardIn().getByTestId("zcode-pending-option-0") as HTMLButtonElement;

    fireEvent.click(option);
    await waitFor(() => expect(option).toHaveAttribute("aria-pressed", "true"));
    fireEvent.change(freeText, { target: { value: "自由补充说明" } });
    // 自由文本生效后同题选中项立即失效（不是只在提交时兜底）
    await waitFor(() => expect(option).toHaveAttribute("aria-pressed", "false"));

    // R3：进入确认页后才提交（未确认前零 answer POST）。
    fireEvent.click(cardIn().getByTestId("zcode-pending-next"));
    fireEvent.click(cardIn().getByTestId("zcode-pending-submit"));
    await waitFor(() => expect(postedActions()).toHaveLength(1));
    expect(submittedAnswers()).toEqual([
      {
        questionText: "这个任务接下来要做什么？",
        selectedValues: [],
        freeText: "自由补充说明",
      },
    ]);
  });

  it("selecting_option_replaces_typed_free_text", async () => {
    // 先打字再点选项：选中项 = 本题唯一答案，自由文本必须被清空（含可见输入框）。
    installAnswerBackend();
    render(<ZcodeControlPanel sessionId={SID} />);

    await screen.findByTestId(`zcode-pending-slot-${PENDING_ID}`);
    const freeText = cardIn().getByRole("textbox") as HTMLInputElement;
    const option = cardIn().getByTestId("zcode-pending-option-1") as HTMLButtonElement;

    fireEvent.change(freeText, { target: { value: "自由补充说明" } });
    fireEvent.click(option);
    await waitFor(() => expect(option).toHaveAttribute("aria-pressed", "true"));
    await waitFor(() => expect(freeText.value).toBe(""));

    // R3：进入确认页后才提交（未确认前零 answer POST）。
    fireEvent.click(cardIn().getByTestId("zcode-pending-next"));
    fireEvent.click(cardIn().getByTestId("zcode-pending-submit"));
    await waitFor(() => expect(postedActions()).toHaveLength(1));
    expect(submittedAnswers()).toEqual([
      {
        questionText: "这个任务接下来要做什么？",
        selectedValues: ["opt-test-first"],
        freeText: null,
      },
    ]);
  });

  it("sensitive_free_text_is_dropped_after_option_switch", async () => {
    // 敏感输入后改点选项：敏感值既不能留在可见输入框，也不能随答案上行。
    installAnswerBackend({ sensitive: true });
    render(<ZcodeControlPanel sessionId={SID} />);

    const sensitive = (await screen.findByTestId(
      "zcode-pending-sensitive-input"
    )) as HTMLInputElement;
    fireEvent.change(sensitive, { target: { value: "合成敏感答案" } });
    expect(sensitive.value).toBe("合成敏感答案");

    fireEvent.click(cardIn().getByTestId("zcode-pending-option-0"));
    await waitFor(() =>
      expect(cardIn().getByTestId("zcode-pending-option-0")).toHaveAttribute("aria-pressed", "true")
    );
    expect(sensitive.value).toBe("");

    // R3：进入确认页后才提交（未确认前零 answer POST）。
    fireEvent.click(cardIn().getByTestId("zcode-pending-next"));
    fireEvent.click(cardIn().getByTestId("zcode-pending-submit"));
    await waitFor(() => expect(postedActions()).toHaveLength(1));
    expect(submittedAnswers()).toEqual([
      {
        questionText: "这个任务接下来要做什么？",
        selectedValues: ["opt-continue"],
        freeText: null,
      },
    ]);
    expect(JSON.stringify(postedActions()[0].action)).not.toContain("合成敏感答案");
  });
});

describe("ZCode 控制面板：模型/档位按能力开放", () => {
  it("model_thought_controls_are_capability_gated", async () => {
    const selection = {
      provider: "zai",
      model: "model-a",
      thought: "high",
      models: [
        { modelId: "model-a", thoughtLevels: ["low", "high"] },
        { modelId: "model-b", thoughtLevels: ["medium"] },
      ],
    };
    let caps = capabilities();
    installFetch((url, init) => {
      if (url === ZCODE_CONNECTION_PATH) return json(connection());
      if (url.startsWith(`${ZCODE_SESSION_PATH}?`)) {
        return json(sessionView({ capabilities: caps, selection }));
      }
      if (url === ZCODE_ACTIONS_PATH && init?.method === "POST") {
        const body = JSON.parse(String(init.body)) as { actionId: string };
        return json(receipt(body.actionId, "accepted"));
      }
      throw new Error(`unexpected fetch: ${url}`);
    });

    const first = render(<ZcodeControlPanel sessionId={SID} />);
    const model = (await screen.findByTestId("zcode-model-select")) as HTMLSelectElement;
    const thought = screen.getByTestId("zcode-thought-select") as HTMLSelectElement;
    // 模型目录只含当前 provider；档位目录只含当前模型的合法值
    expect([...model.options].map((option) => option.value)).toEqual(["model-a", "model-b"]);
    expect(model.value).toBe("model-a");
    expect([...thought.options].map((option) => option.value)).toEqual(["low", "high"]);
    expect(thought.value).toBe("high");

    fireEvent.change(thought, { target: { value: "low" } });
    await waitFor(() => expect(postedActions()).toHaveLength(1));
    expect(postedActions()[0].action).toEqual({
      kind: "selection",
      sessionId: SID,
      generation: GEN,
      model: "model-a",
      thought: "low",
    });
    first.unmount();

    // model 能力关闭：两个控件都禁用并给出可见原因，改值/点击零动作 POST
    caps = capabilities();
    caps.model = flag(false);
    const gatedModel = render(<ZcodeControlPanel sessionId={SID} />);
    const modelSelect = (await screen.findByTestId("zcode-model-select")) as HTMLSelectElement;
    const thoughtSelect = screen.getByTestId("zcode-thought-select") as HTMLSelectElement;
    expect(modelSelect).toBeDisabled();
    expect(thoughtSelect).toBeDisabled();
    const reason = screen.getByTestId("zcode-selection-reason");
    expect(reason).toBeVisible();
    expect(reason.textContent).not.toContain("needs_evidence");
    // 按码如实呈现：needs_evidence 有自己的 zh 文案，不再与其他原因共用同一句
    expect(reason.textContent).toBe("该操作还需终端侧确认，请在终端处理");
    fireEvent.change(modelSelect, { target: { value: "model-b" } });
    fireEvent.change(thoughtSelect, { target: { value: "low" } });
    await act(async () => {});
    expect(postedActions()).toHaveLength(1);
    gatedModel.unmount();

    // thoughtLevel 能力关闭：同样的禁用 + 原因，零动作
    caps = capabilities();
    caps.thoughtLevel = flag(false);
    render(<ZcodeControlPanel sessionId={SID} />);
    expect(await screen.findByTestId("zcode-model-select")).toBeDisabled();
    expect(screen.getByTestId("zcode-thought-select")).toBeDisabled();
    expect(screen.getByTestId("zcode-selection-reason")).toBeVisible();
    fireEvent.change(screen.getByTestId("zcode-thought-select"), { target: { value: "low" } });
    await act(async () => {});
    expect(postedActions()).toHaveLength(1);
  });
});

/** 按点路径读取语言包字符串（缺键 → undefined）。 */
function localeText(bundle: unknown, path: string): string | undefined {
  let node: unknown = bundle;
  for (const segment of path.split(".")) {
    if (typeof node !== "object" || node === null) return undefined;
    node = (node as Record<string, unknown>)[segment];
  }
  return typeof node === "string" ? node : undefined;
}

describe("ZCode 控制面板：原因码 zh/en 全覆盖", () => {
  it("all_reason_codes_have_zh_en_copy", () => {
    // 冻结码表（21 码）每一码都必须映射到 zh/en 都存在的文案键（键集 = 码集）。
    expect(Object.keys(ZCODE_REASON_KEYS).sort()).toEqual([...ZCODE_ERROR_CODES].sort());
    for (const code of ZCODE_ERROR_CODES) {
      const key = ZCODE_REASON_KEYS[code];
      const zhText = localeText(zhLocale, key);
      const enText = localeText(enLocale, key);
      expect(zhText, `zh copy missing for ${code}`).toBeTruthy();
      expect(enText, `en copy missing for ${code}`).toBeTruthy();
      expect(i18n.exists(key, { lng: "zh" }), `zh key missing for ${code}`).toBe(true);
      expect(i18n.exists(key, { lng: "en" }), `en key missing for ${code}`).toBe(true);
    }
  });

  it("zh_and_en_reason_copy_differ", () => {
    for (const code of ZCODE_ERROR_CODES) {
      const key = ZCODE_REASON_KEYS[code];
      const zhText = localeText(zhLocale, key) ?? "";
      const enText = localeText(enLocale, key) ?? "";
      expect(zhText.trim().length, `zh copy empty for ${code}`).toBeGreaterThan(0);
      expect(enText.trim().length, `en copy empty for ${code}`).toBeGreaterThan(0);
      // 两侧各有独立文案（不是把中文抄到英文侧，也不是同一句泛化兜底）
      expect(zhText, `zh/en copy identical for ${code}`).not.toBe(enText);
      // 英文侧不得混入中文，否则英文界面会漏出中文原因
      expect(enText, `en copy contains chinese for ${code}`).not.toMatch(/[\u4e00-\u9fff]/);
    }
  });

  it("unsupported_reason_code_shows_dedicated_copy_not_neutral_fallback", async () => {
    // 冻结码 `unsupported`（Rust codes::UNSUPPORTED）= 能力被静态知识/账本明确判死：
    // **永久**不支持。就地原因必须是专文案（对齐手册示例「暂不支持该操作」），
    // 不得落入中性兜底「控制暂时不可用，请稍后重试」——那会让用户对一个永不开放
    // 的能力空等重试。en 侧同码同专文案（词表守卫不在码内 → 兜底，两侧都不许）。
    expect(localeText(zhLocale, "mobile.zcodeControl.reasonUnsupported")).toBe("暂不支持该操作");
    expect(localeText(enLocale, "mobile.zcodeControl.reasonUnsupported")).toBe(
      "This action is not supported yet"
    );

    const caps = capabilities();
    caps.singleSelect = { enabled: false, reasonCode: "unsupported" };
    installFetch((url) => {
      if (url === ZCODE_CONNECTION_PATH) return json(connection());
      if (url.startsWith(`${ZCODE_SESSION_PATH}?`)) {
        return json(
          sessionView({
            capabilities: caps,
            pending: [pendingView({ questions: [questionView()] })],
          })
        );
      }
      throw new Error(`unexpected fetch: ${url}`);
    });

    render(<ZcodeControlPanel sessionId={SID} />);
    await screen.findByTestId(`zcode-pending-slot-${PENDING_ID}`);
    const reason = cardIn().getByTestId("zcode-pending-reason");
    expect(reason.getAttribute("data-reason-code")).toBe("unsupported");
    // 专文案如实呈现；中性兜底一句话都不出现
    expect(reason.textContent).toBe("暂不支持该操作");
    expect(reason.textContent).not.toContain("暂时不可用");
  });

  it("en_panel_has_no_hardcoded_chinese", async () => {
    await i18n.changeLanguage("en");

    // 第一面：连接需授权 → 顶部原因、发送/草稿等控件文案全部英文。
    installFetch((url) => {
      if (url === ZCODE_CONNECTION_PATH) {
        return json(connection({ state: "needs_auth", reasonCode: "needs_auth" }));
      }
      if (url.startsWith(`${ZCODE_SESSION_PATH}?`)) return json(sessionView());
      throw new Error(`unexpected fetch: ${url}`);
    });
    const first = render(<ZcodeControlPanel sessionId={SID} />);
    const topReason = await screen.findByTestId("zcode-control-reason");
    await waitFor(() => expect(topReason.getAttribute("data-reason-code")).toBe("needs_auth"));
    expect(topReason.textContent).toBe("Complete ZCode official authorization on your computer");
    expect(screen.getByTestId("zcode-send").textContent).toBe("Send");
    expect(screen.getByTestId("zcode-draft").getAttribute("placeholder")).toBe(
      "Type a message to send to ZCode"
    );
    expect(screen.getByTestId("zcode-control-panel").textContent ?? "").not.toMatch(
      /[\u4e00-\u9fff]/
    );
    first.unmount();

    // 第二面：已配对 + ready → pending/selection/interrupt 的就地原因按码取英文文案。
    const caps = capabilities();
    caps.singleSelect = flag(false);
    caps.model = flag(false);
    installFetch((url) => {
      if (url === ZCODE_CONNECTION_PATH) return json(connection());
      if (url.startsWith(`${ZCODE_SESSION_PATH}?`)) {
        return json(
          sessionView({
            capabilities: caps,
            pending: [
              pendingView({
                questions: [
                  questionView({
                    header: "Next step",
                    questionText: "What should the task do next?",
                    options: [
                      {
                        value: "opt-continue",
                        label: "Continue",
                        description: null,
                        preview: null,
                      },
                    ],
                  }),
                ],
              }),
            ],
            control: {
              activeWorks: [
                { kind: "primaryTurn", foregroundExecutionId: "E1" },
                { kind: "primaryTurn", foregroundExecutionId: "E2" },
              ],
            },
            selection: {
              provider: "zai",
              model: "model-a",
              thought: "high",
              models: [{ modelId: "model-a", thoughtLevels: ["low", "high"] }],
            },
          })
        );
      }
      throw new Error(`unexpected fetch: ${url}`);
    });
    render(<ZcodeControlPanel sessionId={SID} />);
    await screen.findByTestId(`zcode-pending-slot-${PENDING_ID}`);

    const pendingReason = cardIn().getByTestId("zcode-pending-reason");
    expect(pendingReason.getAttribute("data-reason-code")).toBe("needs_evidence");
    expect(pendingReason.textContent).toBe(
      "This action still needs confirmation in the terminal — continue there"
    );
    const selectionReason = screen.getByTestId("zcode-selection-reason");
    expect(selectionReason.getAttribute("data-reason-code")).toBe("needs_evidence");
    expect(selectionReason.textContent).toBe(
      "This action still needs confirmation in the terminal — continue there"
    );
    const interruptReason = screen.getByTestId("zcode-interrupt-reason");
    expect(interruptReason.getAttribute("data-reason-code")).toBe("target_ambiguous");
    expect(interruptReason.textContent).toBe(
      "Multiple tasks can be interrupted — choose one in the terminal"
    );

    // 控件/无障碍文案同样走英文
    expect(cardIn().getByTestId("zcode-pending-submit").textContent).toBe("Submit");
    expect(screen.getByTestId("zcode-interrupt").textContent).toBe("Interrupt");
    expect(screen.getByTestId("zcode-model-select").getAttribute("aria-label")).toBe("Model");
    expect(screen.getByTestId("zcode-thought-select").getAttribute("aria-label")).toBe(
      "Thinking level"
    );

    // 英文界面整屏无中文；常见中文短语一个都不应出现（宿主数据本身为英文）。
    const panel = screen.getByTestId("zcode-control-panel");
    expect(panel.textContent ?? "").not.toMatch(/[\u4e00-\u9fff]/);
    for (const phrase of [
      "发送",
      "提交",
      "打断",
      "已受理",
      "已生效",
      "暂时不可用",
      "请在电脑端",
      "请在终端",
      "模型",
      "思考强度",
    ]) {
      expect(panel.textContent ?? "", phrase).not.toContain(phrase);
    }
  });
});

describe("ZCode 控制面板：能力呈现合规（spec R5/§7.6）", () => {
  it("no probe UI and no approval card without pending", async () => {
    // paired + ready + 无 pending + 无 selection + 无运行回合：屏幕上只允许
    // 出现「如实呈现 + 发送面」。钉住三条呈现纪律：
    // - 无探测页/按钮/徽标（build 兼容态绝不外显成任何探测 UI）；
    // - 无 pending 不渲染审批/问答卡，无 selection 目录不渲染模型/档位控件；
    // - 只有 capabilities.send.enabled 才显示可用（开启 → 发送面可见、无原因行）。
    installFetch((url) => {
      if (url.includes("/official-control")) {
        return json({ configState: "missing", url: null });
      }
      if (url === ZCODE_CONNECTION_PATH) return json(connection());
      if (url.startsWith(`${ZCODE_SESSION_PATH}?`)) return json(sessionView());
      throw new Error(`unexpected fetch: ${url}`);
    });

    render(<ZcodeControlPanel sessionId={SID} />);
    await screen.findByTestId("zcode-send");
    // 等全部权威 GET 落定（连接/会话/官方链接），避免异步面晚到误判「缺失」。
    await act(async () => {});

    // 探测 UI 一律不存在：无测试 id、无「探测/probe」字样的任何元素。
    expect(screen.queryByTestId("zcode-probe")).toBeNull();
    expect(screen.queryByText(/探测/)).toBeNull();
    expect(screen.queryByText(/probe/i)).toBeNull();

    // 无 pending：审批/问答卡一张都不渲染（作答面只来自宿主真实 pending）。
    expect(screen.queryByTestId("zcode-pending-card")).toBeNull();
    expect(screen.queryByTestId("zcode-pending-submit")).toBeNull();

    // 无 selection 目录/档位：模型/档位控件不渲染（不伪造目录）。
    expect(screen.queryByTestId("zcode-model-select")).toBeNull();
    expect(screen.queryByTestId("zcode-thought-select")).toBeNull();

    // 无运行回合（activeWorks 空）：打断按钮不渲染。
    expect(screen.queryByTestId("zcode-interrupt")).toBeNull();

    // send 能力开启：发送面可用可见；能力开启时不得带「不可用原因」行。
    expect(screen.getByTestId("zcode-send")).toBeVisible();
    expect(screen.getByTestId("zcode-send")).toBeEnabled();
    expect(screen.queryByTestId("zcode-send-reason")).toBeNull();
  });
});

describe("ZCode 控制面板：冲突态断开并抢回", () => {
  /** 冲突态替身：POST 返回的视图即其后权威 GET 的连接面（抢回后现读口径）。 */
  function conflictBackend(takeoverView: ConnectionView) {
    let takeoverDone = false;
    return (url: string, init?: RequestInit) => {
      if (url.includes("/official-control")) {
        return json({ configState: "missing", url: null });
      }
      if (url === ZCODE_CONNECTION_PATH) {
        return json(
          takeoverDone ? takeoverView : connection({ state: "conflict", reasonCode: "conflict" })
        );
      }
      if (url.startsWith(`${ZCODE_SESSION_PATH}?`)) return json(sessionView());
      if (url === ZCODE_TAKEOVER_PATH && init?.method === "POST") {
        takeoverDone = true;
        return json(takeoverView);
      }
      throw new Error(`unexpected fetch: ${url}`);
    };
  }

  const takeoverCalls = () =>
    fetchMock.mock.calls.filter(([url]) => String(url).endsWith(ZCODE_TAKEOVER_PATH));

  it("conflict shows takeover and posts endpoint once", async () => {
    installFetch(conflictBackend(connection({ state: "paired", generation: GEN + 1 })));
    render(<ZcodeControlPanel sessionId={SID} />);
    fireEvent.click(await screen.findByTestId("zcode-takeover"));
    await waitFor(() => expect(takeoverCalls()).toHaveLength(1));
    // 抢回成功后权威 GET 刷新连接面：conflict → paired（不依赖 SSE）。
    await waitFor(() =>
      expect(screen.getByTestId("zcode-control-panel").getAttribute("data-connection-state")).toBe(
        "paired"
      )
    );
  });

  it("still_conflict_only_hints_without_second_request", async () => {
    installFetch(
      conflictBackend(
        connection({ state: "conflict", generation: GEN + 1, reasonCode: "conflict" })
      )
    );
    render(<ZcodeControlPanel sessionId={SID} />);
    fireEvent.click(await screen.findByTestId("zcode-takeover"));
    // 服务端一次抢回后仍 conflict：只显示「先关官方页面」的提示。
    const notice = await screen.findByTestId("zcode-takeover-still-conflict");
    expect(notice.textContent ?? "").toMatch(/官方/);
    await act(async () => {});
    // 不循环：无论等多久，抢回端点只被 POST 过这一次。
    expect(takeoverCalls()).toHaveLength(1);
  });

  it("takeover_failure_shows_frozen_reason_code", async () => {
    installFetch((url, init) => {
      if (url.includes("/official-control")) {
        return json({ configState: "missing", url: null });
      }
      if (url === ZCODE_CONNECTION_PATH) {
        return json(connection({ state: "conflict", reasonCode: "conflict" }));
      }
      if (url.startsWith(`${ZCODE_SESSION_PATH}?`)) return json(sessionView());
      if (url === ZCODE_TAKEOVER_PATH && init?.method === "POST") {
        return json({ error: "needs_auth" }, 503);
      }
      throw new Error(`unexpected fetch: ${url}`);
    });
    render(<ZcodeControlPanel sessionId={SID} />);
    fireEvent.click(await screen.findByTestId("zcode-takeover"));
    const error = await screen.findByTestId("zcode-takeover-error");
    expect(error.getAttribute("data-reason-code")).toBe("needs_auth");
    // 文案是本地化原因，不是码原文。
    expect(error.textContent ?? "").toMatch(/授权/);
    expect(error.textContent ?? "").not.toContain("needs_auth");
  });

  it("no_takeover_button_when_not_conflict", async () => {
    installFetch((url) => {
      if (url.includes("/official-control")) {
        return json({ configState: "missing", url: null });
      }
      if (url === ZCODE_CONNECTION_PATH) return json(connection());
      if (url.startsWith(`${ZCODE_SESSION_PATH}?`)) return json(sessionView());
      throw new Error(`unexpected fetch: ${url}`);
    });
    render(<ZcodeControlPanel sessionId={SID} />);
    await screen.findByTestId("zcode-draft");
    await act(async () => {});
    expect(screen.queryByTestId("zcode-takeover")).toBeNull();
  });
});

// R2：官方输入队列在面板里的落点（只读投影，不建自建队列、不给写入口）。
describe("ZCode 控制面板：多组待处理并存", () => {
  const PERMISSION_ID = "int_00000000000000000000000000000002";
  const SECOND_QUESTION_ID = "int_00000000000000000000000000000003";

  it("shows_one_card_by_default_and_switches_without_dropping_others", async () => {
    // 同一会话三张 pending（问答 / 审批 / 问答）：默认只展示第一张，其余用紧凑
    // 切组入口切换；三张卡都在册（不静默丢弃），各组草稿互不串、也不丢。
    installFetch((url) => {
      if (url.includes("/official-control")) return json({ configState: "missing", url: null });
      if (url === ZCODE_CONNECTION_PATH) return json(connection());
      if (url.startsWith(`${ZCODE_SESSION_PATH}?`)) {
        return json(
          sessionView({
            pending: [
              pendingView({ questions: [questionView({ freeTextAllowed: true })] }),
              pendingView({
                interactionId: PERMISSION_ID,
                kind: "permission",
                toolName: "Bash",
                summary: "执行合成命令",
                options: [{ optionId: "host-allow-once-9", label: "允许一次", kind: "allowOnce" }],
              }),
              pendingView({
                interactionId: SECOND_QUESTION_ID,
                questions: [questionView({ questionText: "第三组问题？", freeTextAllowed: true })],
              }),
            ],
          })
        );
      }
      throw new Error(`unexpected fetch: ${url}`);
    });

    render(<ZcodeControlPanel sessionId={SID} />);

    const tabA = await screen.findByTestId(`zcode-pending-tab-${PENDING_ID}`);
    expect(tabA.textContent).toBe("问答 1/3");
    expect(screen.getByTestId(`zcode-pending-tab-${PERMISSION_ID}`).textContent).toBe("审批 2/3");
    expect(screen.getByTestId(`zcode-pending-tab-${SECOND_QUESTION_ID}`).textContent).toBe(
      "问答 3/3"
    );

    // 默认只展示第一张：另外两张在册但不可见
    const slotA = screen.getByTestId(`zcode-pending-slot-${PENDING_ID}`);
    const slotB = screen.getByTestId(`zcode-pending-slot-${PERMISSION_ID}`);
    const slotC = screen.getByTestId(`zcode-pending-slot-${SECOND_QUESTION_ID}`);
    expect(slotA.getAttribute("data-active")).toBe("true");
    expect(slotA.hasAttribute("hidden")).toBe(false);
    expect(slotB.hasAttribute("hidden")).toBe(true);
    expect(slotC.hasAttribute("hidden")).toBe(true);
    expect(tabA.getAttribute("aria-selected")).toBe("true");

    // 第一组草稿
    const draftA = within(slotA).getByRole("textbox") as HTMLInputElement;
    fireEvent.change(draftA, { target: { value: "第一组草稿" } });

    // 切到审批组：审批选项出现，问答组隐藏（但没被丢弃）
    fireEvent.click(screen.getByTestId(`zcode-pending-tab-${PERMISSION_ID}`));
    expect(slotB.hasAttribute("hidden")).toBe(false);
    expect(slotA.hasAttribute("hidden")).toBe(true);
    expect(within(slotB).getByTestId("zcode-permission-option-host-allow-once-9")).toBeVisible();

    // 切到第三组写草稿，再切回第一组：两组草稿各自保留
    fireEvent.click(screen.getByTestId(`zcode-pending-tab-${SECOND_QUESTION_ID}`));
    const draftC = within(slotC).getByRole("textbox") as HTMLInputElement;
    fireEvent.change(draftC, { target: { value: "第三组草稿" } });
    fireEvent.click(screen.getByTestId(`zcode-pending-tab-${PENDING_ID}`));
    expect((within(slotA).getByRole("textbox") as HTMLInputElement).value).toBe("第一组草稿");
    fireEvent.click(screen.getByTestId(`zcode-pending-tab-${SECOND_QUESTION_ID}`));
    expect((within(slotC).getByRole("textbox") as HTMLInputElement).value).toBe("第三组草稿");

    // 三张卡一张不少
    expect(screen.getAllByTestId("zcode-pending-card")).toHaveLength(3);
  });

  it("two_question_cards_stay_addressable_per_slot_with_independent_drafts", async () => {
    // 两张**同类**卡并存是重复 testid 最容易炸的形状：全局查询会 found multiple，
    // 因此必须按 slot 定位；同时验证两张卡的草稿完全独立、ARIA 关系成对。
    const SECOND_QUESTION_ID = "int_00000000000000000000000000000003";
    installFetch((url) => {
      if (url.includes("/official-control")) return json({ configState: "missing", url: null });
      if (url === ZCODE_CONNECTION_PATH) return json(connection());
      if (url.startsWith(`${ZCODE_SESSION_PATH}?`)) {
        return json(
          sessionView({
            pending: [
              pendingView({ questions: [questionView({ freeTextAllowed: true })] }),
              pendingView({
                interactionId: SECOND_QUESTION_ID,
                questions: [questionView({ questionText: "第二组问题？", freeTextAllowed: true })],
              }),
            ],
          })
        );
      }
      throw new Error(`unexpected fetch: ${url}`);
    });

    render(<ZcodeControlPanel sessionId={SID} />);
    const slotA = await screen.findByTestId(`zcode-pending-slot-${PENDING_ID}`);
    const slotB = screen.getByTestId(`zcode-pending-slot-${SECOND_QUESTION_ID}`);

    // 卡根带 interaction 身份（按卡归属可核对）
    expect(
      cardIn(PENDING_ID).getByTestId("zcode-pending-card").getAttribute("data-interaction-id")
    ).toBe(PENDING_ID);
    expect(
      cardIn(SECOND_QUESTION_ID)
        .getByTestId("zcode-pending-card")
        .getAttribute("data-interaction-id")
    ).toBe(SECOND_QUESTION_ID);

    // 同名 testid 在全局确实重复（这正是「必须 within(slot)」的证据）
    expect(screen.getAllByTestId("zcode-pending-submit")).toHaveLength(2);
    expect(() => screen.getByTestId("zcode-pending-submit")).toThrow(/multiple/i);
    // 按 slot 分别取控件：不抛，且取到的是**本组**那一个
    expect(cardIn(PENDING_ID).getByTestId("zcode-pending-submit")).toBeVisible();
    // 隐藏组同样能按 slot 取到（testid 查询**不**过滤可见性——这正是必须 within(slot)
    // 的原因）。注意：`getByTestId` 没有 `hidden` 选项（那是 role 查询才有的），旧写法
    // `getByTestId(..., { hidden: true })` + `toBeDefined()` 只证明「不抛异常」，近乎恒真。
    // 这里改成**可证伪**的真实断言：隐藏组控件必须不可见 + 被禁用 + 点击零动作。
    const hiddenSubmit = cardIn(SECOND_QUESTION_ID).getByTestId(
      "zcode-pending-submit"
    ) as HTMLButtonElement;
    expect(hiddenSubmit).not.toBeVisible();
    expect(hiddenSubmit).toBeDisabled();
    fireEvent.click(hiddenSubmit);
    expect(postedActions()).toHaveLength(0);

    // tab ↔ tabpanel 关系成对（用实际 DOM id 互指，不断言 id 的字面格式）
    const tabA = screen.getByTestId(`zcode-pending-tab-${PENDING_ID}`);
    const tabB = screen.getByTestId(`zcode-pending-tab-${SECOND_QUESTION_ID}`);
    expect(tabA.id).not.toBe("");
    expect(tabA.getAttribute("aria-controls")).toBe(slotA.id);
    expect(slotA.getAttribute("role")).toBe("tabpanel");
    expect(slotA.getAttribute("aria-labelledby")).toBe(tabA.id);
    expect(tabB.getAttribute("aria-controls")).toBe(slotB.id);
    expect(slotB.getAttribute("role")).toBe("tabpanel");
    expect(slotB.getAttribute("aria-labelledby")).toBe(tabB.id);
    expect(slotB.hasAttribute("hidden")).toBe(true);
    expect(slotB.hasAttribute("inert")).toBe(true);

    // 草稿互不串、也不丢
    fireEvent.change(cardIn(PENDING_ID).getByRole("textbox"), { target: { value: "A 组草稿" } });
    fireEvent.click(tabB);
    fireEvent.change(cardIn(SECOND_QUESTION_ID).getByRole("textbox"), {
      target: { value: "B 组草稿" },
    });
    fireEvent.click(tabA);
    expect((cardIn(PENDING_ID).getByRole("textbox") as HTMLInputElement).value).toBe("A 组草稿");
    fireEvent.click(tabB);
    expect((cardIn(SECOND_QUESTION_ID).getByRole("textbox") as HTMLInputElement).value).toBe(
      "B 组草稿"
    );
  });

  it("host_interaction_ids_with_special_chars_are_encoded_for_dom_ids", async () => {
    // 宿主 interactionId 是**外部输入**：含空格 / # / . 时，直接内插进 DOM id 会让
    // IDREF（aria-controls / aria-labelledby）按空格断词失效，读屏器的 tab↔tabpanel
    // 关系随之断开。这里要求：DOM id 只含安全字符、IDREF 双向可解析、slug 相同的
    // 两个宿主 id 不产生同一个 DOM id，而数据面仍保留宿主原值。
    const RAW_A = "int bad/#1";
    // 与 RAW_A 非法字符替换后得到同一 slug（"int-bad--1"）→ 必须靠短哈希区分，不能相撞。
    const RAW_B = "int bad-#1";
    installFetch((url) => {
      if (url.includes("/official-control")) return json({ configState: "missing", url: null });
      if (url === ZCODE_CONNECTION_PATH) return json(connection());
      if (url.startsWith(`${ZCODE_SESSION_PATH}?`)) {
        return json(
          sessionView({
            pending: [
              pendingView({ interactionId: RAW_A, questions: [questionView()] }),
              pendingView({ interactionId: RAW_B, questions: [questionView()] }),
            ],
          })
        );
      }
      throw new Error(`unexpected fetch: ${url}`);
    });

    render(<ZcodeControlPanel sessionId={SID} />);
    const slotA = await screen.findByTestId(`zcode-pending-slot-${RAW_A}`);
    const slotB = screen.getByTestId(`zcode-pending-slot-${RAW_B}`);
    const tabA = screen.getByTestId(`zcode-pending-tab-${RAW_A}`);
    const tabB = screen.getByTestId(`zcode-pending-tab-${RAW_B}`);

    // DOM id 只含安全字符，且在文档里唯一可寻址
    for (const id of [slotA.id, slotB.id, tabA.id, tabB.id]) {
      expect(id).not.toBe("");
      expect(id).toMatch(/^[A-Za-z0-9_-]+$/);
      expect(document.getElementById(id)).not.toBeNull();
    }
    // IDREF 双向可解析（读屏器据此建立 tab ↔ tabpanel 关系）
    expect(document.getElementById(tabA.getAttribute("aria-controls") ?? "")).toBe(slotA);
    expect(document.getElementById(slotA.getAttribute("aria-labelledby") ?? "")).toBe(tabA);
    expect(document.getElementById(tabB.getAttribute("aria-controls") ?? "")).toBe(slotB);
    expect(document.getElementById(slotB.getAttribute("aria-labelledby") ?? "")).toBe(tabB);

    // slug 相同的两个宿主 id 不得产生相同 DOM id（哈希后缀生效）
    expect(slotA.id).not.toBe(slotB.id);
    expect(tabA.id).not.toBe(tabB.id);

    // 数据面仍保留宿主原值（编码只用于 DOM id）
    expect(
      cardIn(RAW_A).getByTestId("zcode-pending-card").getAttribute("data-interaction-id")
    ).toBe(RAW_A);
    expect(
      cardIn(RAW_B).getByTestId("zcode-pending-card").getAttribute("data-interaction-id")
    ).toBe(RAW_B);
  });

  it("single_pending_renders_without_tab_strip", async () => {
    // 只剩一张时不出现切组入口（不占屏），卡照常展示——宿主撤掉其他请求后不会空屏。
    let dropFirst = false;
    installFetch((url) => {
      if (url.includes("/official-control")) return json({ configState: "missing", url: null });
      if (url === ZCODE_CONNECTION_PATH) return json(connection());
      if (url.startsWith(`${ZCODE_SESSION_PATH}?`)) {
        const pending = [
          pendingView({ questions: [questionView()] }),
          pendingView({
            interactionId: PERMISSION_ID,
            kind: "permission",
            toolName: "Bash",
            summary: "执行合成命令",
            options: [{ optionId: "host-allow-once-9", label: "允许一次", kind: "allowOnce" }],
          }),
        ];
        return json(sessionView({ pending: dropFirst ? pending.slice(1) : pending }));
      }
      throw new Error(`unexpected fetch: ${url}`);
    });

    const { unmount } = render(<ZcodeControlPanel sessionId={SID} />);
    await screen.findByTestId(`zcode-pending-tab-${PENDING_ID}`);
    unmount();

    // 直接以「只剩一张」的投影重挂：无切组入口，剩余那张照常展示
    dropFirst = true;
    render(<ZcodeControlPanel sessionId={SID} />);
    expect(await screen.findByText("执行合成命令")).toBeVisible();
    expect(screen.queryByTestId("zcode-pending-tabs")).toBeNull();
    expect(screen.getAllByTestId("zcode-pending-card")).toHaveLength(1);
    // 单张卡没有 tab 指向它，因此不冒充 tabpanel 语义
    const slot = slotOf(PERMISSION_ID);
    expect(slot.getAttribute("role")).toBeNull();
    expect(slot.getAttribute("aria-labelledby")).toBeNull();
  });
});

describe("ZCode 控制面板：官方队列投影", () => {
  it("renders_official_queue_from_session_view", async () => {
    installFetch((url) => {
      if (url.includes("/official-control")) {
        return json({ configState: "missing", url: null });
      }
      if (url === ZCODE_CONNECTION_PATH) return json(connection());
      if (url.startsWith(`${ZCODE_SESSION_PATH}?`)) {
        return json(
          sessionView({
            queue: {
              known: true,
              paused: true,
              items: [{ inputId: "queue_cmd-running", text: "运行中发的那条", position: 0 }],
            },
          })
        );
      }
      throw new Error(`unexpected fetch: ${url}`);
    });
    render(<ZcodeControlPanel sessionId={SID} />);
    const root = await screen.findByTestId("zcode-queue");
    // 默认一行：数量 + 暂停标志，正文不占首屏。
    expect(root.dataset.state).toBe("paused");
    expect(screen.getByTestId("zcode-queue-count").textContent).toContain("1");
    expect(screen.queryByText("运行中发的那条")).toBeNull();
    fireEvent.click(screen.getByTestId("zcode-queue-toggle"));
    expect(screen.getByText("运行中发的那条")).toBeTruthy();
  });

  it("omitted_queue_stays_unknown_without_panel", async () => {
    installFetch((url) => {
      if (url.includes("/official-control")) {
        return json({ configState: "missing", url: null });
      }
      if (url === ZCODE_CONNECTION_PATH) return json(connection());
      if (url.startsWith(`${ZCODE_SESSION_PATH}?`)) {
        // 旧服务端载荷：没有 queue 字段。
        return json(
          sessionView({
            pending: [
              pendingView({
                kind: "permission",
                options: [{ optionId: "allowOnce", label: "Allow once", kind: "allowOnce" }],
              }),
            ],
          })
        );
      }
      throw new Error(`unexpected fetch: ${url}`);
    });
    render(<ZcodeControlPanel sessionId={SID} />);
    // 投影已落地（pending 卡可读）但仍无队列面板：缺字段 ≠ 空队列。
    await screen.findByTestId(`zcode-pending-slot-${PENDING_ID}`);
    expect(screen.queryByTestId("zcode-queue")).toBeNull();
  });
});
