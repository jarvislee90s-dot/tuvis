// R3：ZCode 待处理卡（问答 / 审批）行为锁 —— 本文件先红后绿。
//
// 契约（测试只走用户可见面，数据全为合成 DTO）：
// - 一个 interaction 的多题**一次只显示一题**：`zcode-pending-step` = 「第 k/N 题」，
//   选项容器 `zcode-answer-options` 纵向（`flex-col` + `data-layout="vertical"`），
//   每项整行可点（`w-full`）；`zcode-pending-prev` / `zcode-pending-next` 前后翻页。
// - 前进/后退保留已答；单选替换、多选 toggle、自由文本与选项互斥（语义与旧卡一致）。
// - 未进入确认页前**零 answer POST**：`zcode-pending-submit` 只在确认页可用，
//   确认页 `zcode-pending-confirm` 列出全部题目与答案（`zcode-pending-confirm-item-<i>`
//   / `zcode-pending-confirm-answer-<i>`）。
// - 任一无答案（无选中项且无自由文本）→ 提交禁用 + `zcode-pending-missing` 指名缺哪题；
//   最终提交恰一次 `ControlAction::Answer`，answers 按原始题序/原始题干/原始 value。
// - 无关 revision 变化不丢草稿、不解锁已受理的重复提交；interaction 替换/撤销或
//   generation 变化清空草稿；迟到回执不得落到新卡。
// - autoResolution 首次有效编辑只补发一次 snooze；sensitive 值只留内存（不落存储）。
// - 审批只渲染 allowOnce/deny（纵向），`fullAccess`/`allowAlways`/`allowSession` 一律
//   不开放；`detail`（Bash 命令正文）默认折叠、展开只读、不参与提交。
import type { ComponentProps } from "react";
import { cleanup, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { afterAll, afterEach, beforeAll, describe, expect, it, vi } from "vitest";
import i18n from "@/i18n";
import { ZcodePendingCard } from "@/mobile/ZcodePendingCard";
import type {
  ActionReceipt,
  ActionState,
  CapabilityView,
  ControlAction,
  PendingView,
  QuestionView,
} from "@/lib/api/zcodeControl";
import { pinZhNavigator } from "./pinZhNavigator";

const SID = "session_0000000000000000000000000000000a";
const GEN = 3;
const INTERACTION = "int_00000000000000000000000000000001";

let unpinZh: () => void;

beforeAll(async () => {
  unpinZh = await pinZhNavigator();
});

afterAll(() => {
  unpinZh();
});

afterEach(async () => {
  cleanup();
  vi.restoreAllMocks();
  if (i18n.language !== "zh") await i18n.changeLanguage("zh");
});

const flag = (enabled: boolean) => ({ enabled, reasonCode: enabled ? null : "needs_evidence" });

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

function receipt(actionId: string, state: ActionState): ActionReceipt {
  return { actionId, sessionId: SID, generation: GEN, state, reasonCode: null };
}

/** 记录全部提交动作；默认按 accepted 回执（snooze 同理）。 */
function makeSubmit(state: ActionState = "accepted") {
  const calls: ControlAction[] = [];
  const fn = vi.fn(async (action: ControlAction) => {
    calls.push(action);
    return receipt(`act-${calls.length}`, state);
  });
  return { fn, calls };
}

function questionView(overrides: Partial<QuestionView> = {}): QuestionView {
  return {
    header: "题头",
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

function pendingView(overrides: Partial<PendingView> = {}): PendingView {
  return {
    interactionId: INTERACTION,
    kind: "userInput",
    anchorRowId: 7,
    toolCallId: "call_00000000000000000000000000000001",
    createdAt: 1760000000000,
    summary: null,
    toolName: "AskUserQuestion",
    schemaInteraction: "AskUserQuestion",
    sensitive: false,
    autoResolution: false,
    autoResolutionState: null,
    questions: [],
    options: [],
    detail: null,
    ...overrides,
  };
}

/** 三题卡：单选 + 多选 + 自由文本，覆盖整条暂存/确认链。 */
function threeQuestions(): PendingView {
  return pendingView({
    questions: [
      questionView({ header: "第一步", questionText: "第一题？" }),
      questionView({
        header: "第二步",
        questionText: "第二题？",
        multiSelect: true,
        options: [
          { value: "q2-a", label: "第二题选项甲", description: null, preview: null },
          { value: "q2-b", label: "第二题选项乙", description: null, preview: null },
        ],
      }),
      questionView({
        header: "第三步",
        questionText: "第三题？",
        freeTextAllowed: true,
        options: [],
      }),
    ],
  });
}

type CardProps = ComponentProps<typeof ZcodePendingCard>;

/**
 * 浏览器存储写入探针（可证伪）。
 *
 * 为什么必须拦 `Storage.prototype` 而不是 storage 实例：jsdom 的 `localStorage` /
 * `sessionStorage` 是 Proxy 包装，实例级 `vi.spyOn(window.localStorage, "setItem")`
 * **装不上**——`Object.getOwnPropertyDescriptor(window.localStorage, "setItem")` 为
 * `undefined`、装上后 `window.localStorage.setItem !== spy`，于是**任何真实写入都
 * 不进计数**，「零写入」断言会永远是假绿。原型方法是两种存储共用的唯一入口，
 * 拦它才拦得住真实写入（写操作仍照常落库，探针只旁观）。
 *
 * 按 `this` 分桶：localStorage 与 sessionStorage 各自计数，便于断言「两种都为零」。
 */
function installStorageProbe(): {
  writes: Array<{ store: "local" | "session"; key: string; value: string }>;
} {
  const writes: Array<{ store: "local" | "session"; key: string; value: string }> = [];
  const original = Storage.prototype.setItem;
  vi.spyOn(Storage.prototype, "setItem").mockImplementation(function (
    this: Storage,
    key: string,
    value: string
  ) {
    writes.push({
      store: this === window.sessionStorage ? "session" : "local",
      key,
      value,
    });
    original.call(this, key, value);
  });
  return { writes };
}

/**
 * 浏览器存储的**白盒快照**（完全不依赖任何方法拦截）。
 *
 * 为什么需要它：`Storage.prototype.setItem` 探针只拦**方法调用**形式。jsdom 的
 * `Storage` 是 Proxy，`localStorage.foo = "bar"` 走 `ProxyHandler.set` →
 * `impl.setItem(...)`，**绕过原型方法**——探针观测不到，但值**真的落库**。
 * 于是再加一层「读真实存储状态」的兜底：动作前后对比 `length` 与键集合/条目，
 * 任何形式的写入都跑不掉。
 *
 * 与探针互为冗余，且**不依赖探针可用**：探针是「拦截写入」，快照是「读落库结果」，
 * 后者的成立不需要前者生效（读路径没有「装不上」这种失败模式）。
 *
 * 键集合用两种口径互校：`Object.keys`（Proxy 的 ownKeys）与 `key(i)` 索引枚举；
 * `entries` 额外覆盖「键集合没变但既有键被改写」的情况。
 */
function storageSnapshot(): {
  local: {
    length: number;
    keys: string[];
    indexedKeys: string[];
    entries: Array<[string, string]>;
  };
  session: {
    length: number;
    keys: string[];
    indexedKeys: string[];
    entries: Array<[string, string]>;
  };
} {
  const read = (storage: Storage) => ({
    length: storage.length,
    keys: Object.keys(storage).sort(),
    indexedKeys: Array.from(
      { length: storage.length },
      (_, index) => storage.key(index) ?? ""
    ).sort(),
    entries: Object.keys(storage)
      .sort()
      .map((key) => [key, storage.getItem(key) ?? ""] as [string, string]),
  });
  return { local: read(window.localStorage), session: read(window.sessionStorage) };
}

function baseProps(overrides: Partial<CardProps> = {}): CardProps {
  return {
    pending: pendingView(),
    sessionId: SID,
    generation: GEN,
    capabilities: capabilities(),
    gateReason: null,
    submitting: false,
    revision: 12,
    submit: makeSubmit().fn,
    ...overrides,
  };
}

describe("ZCode 待处理卡：逐题分页", () => {
  it("three_questions_show_one_at_a_time_with_vertical_options", () => {
    const { fn } = makeSubmit();
    render(<ZcodePendingCard {...baseProps({ pending: threeQuestions(), submit: fn })} />);

    // 一次只显示一题：题干计数器 + 只有一个题干可见
    expect(screen.getByTestId("zcode-pending-step").textContent).toBe("第 1/3 题");
    expect(screen.getByText("第一题？")).toBeVisible();
    expect(screen.queryByText("第二题？")).toBeNull();
    expect(screen.queryByText("第三题？")).toBeNull();

    // 窄屏纵向：容器 flex-col + 每项整行可点
    const options = screen.getByTestId("zcode-answer-options");
    expect(options.className).toContain("flex-col");
    expect(options.getAttribute("data-layout")).toBe("vertical");
    const buttons = within(options).getAllByRole("button");
    expect(buttons).toHaveLength(2);
    for (const button of buttons) expect(button.className).toContain("w-full");

    // 首题没有「上一题」
    expect(screen.getByTestId("zcode-pending-prev")).toBeDisabled();

    // 前进/后退保留已答
    fireEvent.click(screen.getByTestId("zcode-pending-option-0"));
    expect(screen.getByTestId("zcode-pending-option-0")).toHaveAttribute("aria-pressed", "true");
    fireEvent.click(screen.getByTestId("zcode-pending-next"));
    expect(screen.getByTestId("zcode-pending-step").textContent).toBe("第 2/3 题");
    expect(screen.getByText("第二题？")).toBeVisible();
    expect(screen.queryByText("第一题？")).toBeNull();
    fireEvent.click(screen.getByTestId("zcode-pending-prev"));
    expect(screen.getByTestId("zcode-pending-step").textContent).toBe("第 1/3 题");
    expect(screen.getByTestId("zcode-pending-option-0")).toHaveAttribute("aria-pressed", "true");

    // 未进入确认页：一次 answer 都没发出（本地暂存）
    expect(fn).not.toHaveBeenCalled();
  });

  it("single_select_replaces_multi_select_toggles_and_free_text_is_exclusive", () => {
    const { fn } = makeSubmit();
    const pending = pendingView({
      questions: [
        questionView({ questionText: "单选？" }),
        questionView({
          questionText: "多选？",
          multiSelect: true,
          freeTextAllowed: true,
          options: [
            { value: "m-a", label: "甲", description: null, preview: null },
            { value: "m-b", label: "乙", description: null, preview: null },
          ],
        }),
      ],
    });
    render(<ZcodePendingCard {...baseProps({ pending, submit: fn })} />);

    // 单选 = 替换
    fireEvent.click(screen.getByTestId("zcode-pending-option-0"));
    fireEvent.click(screen.getByTestId("zcode-pending-option-1"));
    expect(screen.getByTestId("zcode-pending-option-0")).toHaveAttribute("aria-pressed", "false");
    expect(screen.getByTestId("zcode-pending-option-1")).toHaveAttribute("aria-pressed", "true");

    fireEvent.click(screen.getByTestId("zcode-pending-next"));

    // 多选 = toggle
    fireEvent.click(screen.getByTestId("zcode-pending-option-0"));
    fireEvent.click(screen.getByTestId("zcode-pending-option-1"));
    expect(screen.getByTestId("zcode-pending-option-0")).toHaveAttribute("aria-pressed", "true");
    expect(screen.getByTestId("zcode-pending-option-1")).toHaveAttribute("aria-pressed", "true");
    fireEvent.click(screen.getByTestId("zcode-pending-option-0"));
    expect(screen.getByTestId("zcode-pending-option-0")).toHaveAttribute("aria-pressed", "false");
    expect(screen.getByTestId("zcode-pending-option-1")).toHaveAttribute("aria-pressed", "true");

    // 自由文本与选项互斥：打字清掉同题选中项，点选项清掉自由文本
    const freeText = within(screen.getByTestId("zcode-pending-card")).getByRole(
      "textbox"
    ) as HTMLInputElement;
    fireEvent.change(freeText, { target: { value: "自由补充" } });
    expect(screen.getByTestId("zcode-pending-option-1")).toHaveAttribute("aria-pressed", "false");
    expect(freeText.value).toBe("自由补充");
    fireEvent.click(screen.getByTestId("zcode-pending-option-0"));
    expect(freeText.value).toBe("");

    expect(fn).not.toHaveBeenCalled();
  });
});

describe("ZCode 待处理卡：确认页与一次性提交", () => {
  /** 依次作答三题并进入确认页。 */
  function answerAllAndReview() {
    // 第一题：单选
    fireEvent.click(screen.getByTestId("zcode-pending-option-1"));
    fireEvent.click(screen.getByTestId("zcode-pending-next"));
    // 第二题：多选两项
    fireEvent.click(screen.getByTestId("zcode-pending-option-0"));
    fireEvent.click(screen.getByTestId("zcode-pending-option-1"));
    fireEvent.click(screen.getByTestId("zcode-pending-next"));
    // 第三题：自由文本
    const freeText = screen.getByRole("textbox") as HTMLInputElement;
    fireEvent.change(freeText, { target: { value: "第三题的补充" } });
    fireEvent.click(screen.getByTestId("zcode-pending-next"));
  }

  it("confirm_lists_all_three_answers_before_any_post", () => {
    const { fn } = makeSubmit();
    render(<ZcodePendingCard {...baseProps({ pending: threeQuestions(), submit: fn })} />);

    answerAllAndReview();

    // 确认页：三题全部列出（题干 + 答案），且此刻仍零 POST
    expect(screen.getByTestId("zcode-pending-confirm")).toBeVisible();
    expect(screen.getByTestId("zcode-pending-confirm-item-0").textContent).toContain("第一题？");
    expect(screen.getByTestId("zcode-pending-confirm-item-1").textContent).toContain("第二题？");
    expect(screen.getByTestId("zcode-pending-confirm-item-2").textContent).toContain("第三题？");
    expect(screen.getByTestId("zcode-pending-confirm-answer-0").textContent).toBe("先写测试");
    // 多选答案按已入库分隔键拼接（zh「、」），不是硬编码 ", "
    expect(screen.getByTestId("zcode-pending-confirm-answer-1").textContent).toBe(
      "第二题选项甲、第二题选项乙"
    );
    expect(screen.getByTestId("zcode-pending-confirm-answer-2").textContent).toBe("第三题的补充");
    expect(fn).not.toHaveBeenCalled();

    // 返回修改：草稿仍在
    fireEvent.click(screen.getByTestId("zcode-pending-review-back"));
    expect(screen.getByTestId("zcode-pending-step").textContent).toBe("第 3/3 题");
    expect((screen.getByRole("textbox") as HTMLInputElement).value).toBe("第三题的补充");
  });

  it("final_submit_posts_exactly_one_answer_with_original_values", async () => {
    const { fn, calls } = makeSubmit();
    render(<ZcodePendingCard {...baseProps({ pending: threeQuestions(), submit: fn })} />);

    answerAllAndReview();

    const submit = screen.getByTestId("zcode-pending-submit") as HTMLButtonElement;
    await waitFor(() => expect(submit).toBeEnabled());
    // 同帧双击：只允许一个在途提交
    submit.click();
    submit.click();
    await waitFor(() => expect(calls).toHaveLength(1));

    expect(calls[0]).toEqual({
      kind: "answer",
      sessionId: SID,
      generation: GEN,
      interactionId: INTERACTION,
      answers: [
        { questionText: "第一题？", selectedValues: ["opt-test-first"], freeText: null },
        { questionText: "第二题？", selectedValues: ["q2-a", "q2-b"], freeText: null },
        { questionText: "第三题？", selectedValues: [], freeText: "第三题的补充" },
      ],
    });

    // 受理后锁定：再点不产生第二个 actionId
    await waitFor(() => expect(submit).toBeDisabled());
    submit.click();
    expect(calls).toHaveLength(1);
  });

  it("incomplete_answers_disable_submit_and_name_the_missing_question", () => {
    const { fn, calls } = makeSubmit();
    render(<ZcodePendingCard {...baseProps({ pending: threeQuestions(), submit: fn })} />);

    // 只答第一题，直接翻到确认页（允许前后浏览）
    fireEvent.click(screen.getByTestId("zcode-pending-option-0"));
    fireEvent.click(screen.getByTestId("zcode-pending-next"));
    fireEvent.click(screen.getByTestId("zcode-pending-next"));
    fireEvent.click(screen.getByTestId("zcode-pending-next"));

    const submit = screen.getByTestId("zcode-pending-submit") as HTMLButtonElement;
    expect(submit).toBeDisabled();
    const missing = screen.getByTestId("zcode-pending-missing");
    expect(missing.textContent).toContain("第 2 题");
    expect(missing.textContent).toContain("第 3 题");
    expect(screen.getByTestId("zcode-pending-confirm-answer-1").textContent).toBe("未作答");

    fireEvent.click(submit);
    expect(calls).toHaveLength(0);
  });
});

describe("ZCode 待处理卡：空题边界", () => {
  it("zero_question_card_never_posts_an_empty_answer", () => {
    // 旧合同的兜底：空答案列表 ≠ 用户同意继续。上游投影已经丢弃 0 题卡，
    // 但安全边界不外包给上游——卡片自己必须零 POST。
    const { fn, calls } = makeSubmit();
    render(
      <ZcodePendingCard {...baseProps({ pending: pendingView({ questions: [] }), submit: fn })} />
    );

    // 无条件断言（不写 `if (next !== null)` 之类的条件分支：那种写法在「下一题」
    // 按钮不再渲染时会整段跳过，护栏静默消失且用例照样 PASS）。
    // 0 题卡仍渲染导航与提交；提交按钮初始禁用（未进确认页）。
    const next = screen.getByTestId("zcode-pending-next") as HTMLButtonElement;
    expect(next).toBeEnabled();
    expect(screen.getByTestId("zcode-pending-submit")).toBeDisabled();
    // 题目页点提交：禁用 → 零 POST
    fireEvent.click(screen.getByTestId("zcode-pending-submit"));
    expect(calls).toHaveLength(0);
    // 进确认页后提交按钮变可用——此时点它也必须零 POST（真正走到提交路径又被空题闸拦住）
    fireEvent.click(next);
    expect(screen.getByTestId("zcode-pending-confirm")).toBeVisible();
    const confirmSubmit = screen.getByTestId("zcode-pending-submit") as HTMLButtonElement;
    expect(confirmSubmit).toBeEnabled();
    fireEvent.click(confirmSubmit);
    expect(calls).toHaveLength(0);
  });
});

describe("ZCode 待处理卡：草稿与受理锁按身份保持", () => {
  it("unrelated_revision_keeps_draft_and_does_not_unlock_accepted_submit", async () => {
    const { fn, calls } = makeSubmit();
    const pending = pendingView({ questions: [questionView({ freeTextAllowed: true })] });
    const props = baseProps({ pending, submit: fn });
    const { rerender } = render(<ZcodePendingCard {...props} />);

    const freeText = screen.getByRole("textbox") as HTMLInputElement;
    fireEvent.change(freeText, { target: { value: "暂存草稿" } });
    fireEvent.click(screen.getByTestId("zcode-pending-next"));
    const submit = screen.getByTestId("zcode-pending-submit") as HTMLButtonElement;
    await waitFor(() => expect(submit).toBeEnabled());
    fireEvent.click(submit);
    await waitFor(() => expect(calls).toHaveLength(1));
    await waitFor(() => expect(submit).toBeDisabled());

    // 宿主旁路推进（revision 变）但 interaction 未变：草稿与受理锁都必须保留
    rerender(
      <ZcodePendingCard {...props} revision={13} pending={{ ...pending, summary: "旁路变化" }} />
    );
    expect(screen.getByTestId("zcode-pending-notice").textContent).toBe("已受理");
    expect(screen.getByTestId("zcode-pending-submit")).toBeDisabled();
    expect(screen.getByTestId("zcode-pending-confirm-answer-0").textContent).toBe("暂存草稿");
    expect(calls).toHaveLength(1);

    // 回到题目页同样保留草稿
    fireEvent.click(screen.getByTestId("zcode-pending-review-back"));
    expect((screen.getByRole("textbox") as HTMLInputElement).value).toBe("暂存草稿");
  });

  it("interaction_replacement_and_generation_change_clear_the_draft", () => {
    const pending = pendingView({
      questions: [
        questionView({ freeTextAllowed: true }),
        questionView({ questionText: "第二题？" }),
      ],
    });
    const { rerender } = render(<ZcodePendingCard {...baseProps({ pending })} />);

    fireEvent.change(screen.getByRole("textbox"), { target: { value: "旧卡草稿" } });
    fireEvent.click(screen.getByTestId("zcode-pending-next"));
    expect(screen.getByTestId("zcode-pending-step").textContent).toBe("第 2/2 题");

    // interaction 被替换（同一挂载点、不同 id）：旧草稿与翻页位置都不得残留
    rerender(
      <ZcodePendingCard
        {...baseProps({ pending: { ...pending, interactionId: "int_replaced" } })}
      />
    );
    expect(screen.getByTestId("zcode-pending-step").textContent).toBe("第 1/2 题");
    expect((screen.getByRole("textbox") as HTMLInputElement).value).toBe("");

    // 填一点内容后换代：generation 变化同样清空
    fireEvent.change(screen.getByRole("textbox"), { target: { value: "换代前草稿" } });
    rerender(
      <ZcodePendingCard
        {...baseProps({
          pending: { ...pending, interactionId: "int_replaced" },
          generation: GEN + 1,
        })}
      />
    );
    expect((screen.getByRole("textbox") as HTMLInputElement).value).toBe("");
  });

  it("late_receipt_from_old_interaction_never_lands_on_new_card", async () => {
    let settle!: (value: ActionReceipt) => void;
    const inFlight = new Promise<ActionReceipt>((resolve) => {
      settle = resolve;
    });
    const submit = vi.fn((action: ControlAction) => {
      expect(action.kind).toBe("answer");
      return inFlight;
    });
    const pending = pendingView({ questions: [questionView({ freeTextAllowed: true })] });
    const { rerender } = render(<ZcodePendingCard {...baseProps({ pending, submit })} />);

    fireEvent.change(screen.getByRole("textbox"), { target: { value: "旧卡答案" } });
    fireEvent.click(screen.getByTestId("zcode-pending-next"));
    fireEvent.click(screen.getByTestId("zcode-pending-submit"));
    await waitFor(() => expect(submit).toHaveBeenCalledTimes(1));

    // 提交在途时宿主换卡：新卡不得被旧回执点亮成「已受理」
    rerender(
      <ZcodePendingCard
        {...baseProps({ pending: { ...pending, interactionId: "int_replacement" }, submit })}
      />
    );
    settle(receipt("act-old", "accepted"));
    await waitFor(() => expect(screen.queryByTestId("zcode-pending-notice")).toBeNull());
    expect(screen.queryByText("已受理")).toBeNull();
    expect(screen.getByTestId("zcode-pending-step").textContent).toBe("第 1/1 题");
  });
});

describe("ZCode 待处理卡：既有合同不回退", () => {
  it("auto_resolution_snoozes_once_on_first_edit", async () => {
    const { fn, calls } = makeSubmit();
    render(
      <ZcodePendingCard
        {...baseProps({
          pending: pendingView({ autoResolution: true, questions: [questionView()] }),
          submit: fn,
        })}
      />
    );

    fireEvent.click(screen.getByTestId("zcode-pending-option-0"));
    fireEvent.click(screen.getByTestId("zcode-pending-option-1"));
    fireEvent.click(screen.getByTestId("zcode-pending-next"));

    await waitFor(() => expect(calls).toHaveLength(1));
    expect(calls[0]).toEqual({
      kind: "snooze",
      sessionId: SID,
      generation: GEN,
      interactionId: INTERACTION,
    });
    // 首次编辑后不再补发第二个 snooze；受理满一次后才允许一次 answer
    fireEvent.click(screen.getByTestId("zcode-pending-review-back"));
    fireEvent.click(screen.getByTestId("zcode-pending-option-1"));
    expect(calls.filter((action) => action.kind === "snooze")).toHaveLength(1);
    fireEvent.click(screen.getByTestId("zcode-pending-next"));
    fireEvent.click(screen.getByTestId("zcode-pending-submit"));
    await waitFor(() => expect(calls.filter((action) => action.kind === "answer")).toHaveLength(1));
  });

  it("sensitive_answers_never_reach_browser_storage", async () => {
    // 探针装在 `Storage.prototype` 上（jsdom 的 Storage 实例是 Proxy，实例级 spy
    // 装不上、真实写入不会计数 → 断言会永远是假绿）。详见 installStorageProbe 注释。
    const probe = installStorageProbe();
    // 兜底基线：**不依赖任何方法拦截**的白盒快照。探针只拦方法调用形式，
    // `localStorage.foo = "bar"` 这类属性赋值写入会绕过它（jsdom 走 Proxy 的
    // set trap → impl.setItem），快照则直接读「有没有真的落库」，兜住所有形式。
    const before = storageSnapshot();
    const { fn, calls } = makeSubmit();
    const pending = pendingView({
      sensitive: true,
      questions: [questionView({ freeTextAllowed: true })],
    });
    render(<ZcodePendingCard {...baseProps({ pending, submit: fn })} />);

    const input = screen.getByTestId("zcode-pending-sensitive-input") as HTMLInputElement;
    expect(input.type).toBe("password");
    fireEvent.change(input, { target: { value: "合成敏感答案" } });
    expect(input.value).toBe("合成敏感答案");

    // 失焦清可见态，但确认页不显示明文（只显示「已填写」）
    fireEvent.blur(input);
    expect(input.value).toBe("");
    fireEvent.click(screen.getByTestId("zcode-pending-next"));
    expect(screen.getByTestId("zcode-pending-confirm-answer-0").textContent).toBe(
      "已填写（不显示）"
    );

    fireEvent.click(screen.getByTestId("zcode-pending-submit"));
    await waitFor(() => expect(calls).toHaveLength(1));
    // 载荷侧：提交带走的是**内存值**（「失焦清可见态」≠「丢弃答案」）。
    expect(calls[0]).toEqual({
      kind: "answer",
      sessionId: SID,
      generation: GEN,
      interactionId: INTERACTION,
      answers: [
        { questionText: "这个任务接下来要做什么？", selectedValues: [], freeText: "合成敏感答案" },
      ],
    });

    // 存储侧 ①：全流程（含提交）零浏览器存储写入 —— 两种存储分桶各为零。
    expect(probe.writes.filter((write) => write.store === "local")).toEqual([]);
    expect(probe.writes.filter((write) => write.store === "session")).toEqual([]);

    // 存储侧 ②（兜底，不依赖方法拦截）：动作前后快照必须**完全一致** ——
    // 无新增键、无新增条目、既有条目未被改写。属性赋值形式的写入也跑不掉
    // （它绕过探针的原型方法，但一定改变 length / 键集合 / 条目）。
    const after = storageSnapshot();
    expect(after.local.length).toBe(before.local.length);
    expect(after.local.keys).toEqual(before.local.keys);
    expect(after.local.indexedKeys).toEqual(before.local.indexedKeys);
    expect(after.local.entries).toEqual(before.local.entries);
    expect(after.session.length).toBe(before.session.length);
    expect(after.session.keys).toEqual(before.session.keys);
    expect(after.session.indexedKeys).toEqual(before.session.indexedKeys);
    expect(after.session.entries).toEqual(before.session.entries);

    // 自检（必须留在用例里）：同一个探针必须真的能观测到写入，否则上面两条
    // 「零写入」就是假绿。此处**故意**各写一次并断言被观测到；写完立即清理。
    window.localStorage.setItem("zcode-storage-probe", "probe-local");
    window.sessionStorage.setItem("zcode-storage-probe", "probe-session");
    expect(probe.writes).toEqual([
      { store: "local", key: "zcode-storage-probe", value: "probe-local" },
      { store: "session", key: "zcode-storage-probe", value: "probe-session" },
    ]);
    window.localStorage.removeItem("zcode-storage-probe");
    window.sessionStorage.removeItem("zcode-storage-probe");
    expect(window.localStorage.getItem("zcode-storage-probe")).toBeNull();
    expect(window.sessionStorage.getItem("zcode-storage-probe")).toBeNull();

    // 自检 ②：兜底快照同样必须能观测到写入——且**专挑探针可能抓不到的形式**
    // （属性赋值 `localStorage.foo = v`）。若快照恒等，它就是空转断言；此处证明它会变。
    //
    // 为什么专挑属性赋值：jsdom 的 `Storage` 是 Proxy，`localStorage.foo = v` 走
    // `ProxyHandler.set` → `impl.setItem(...)`，**绕过 `Storage.prototype.setItem`**
    // （实测：值真的落库，探针观测不到——故有兜底快照的必要性）。
    // 这里**不断言**探针是否观测到该形式（那是 jsdom 实现细节，未来版本可能改变，
    // 断言它只会造成无谓的脆弱失败）；本用例只保证**兜底快照**对它敏感。
    const probeBaseline = storageSnapshot();
    (window.localStorage as unknown as Record<string, string>).zcodeStorageProbeAssign =
      "probe-assign";
    const probeAfter = storageSnapshot();
    expect(probeAfter.local.length).toBe(probeBaseline.local.length + 1);
    expect(probeAfter.local.keys).toContain("zcodeStorageProbeAssign");
    expect(probeAfter.local.entries).toContainEqual(["zcodeStorageProbeAssign", "probe-assign"]);
    delete (window.localStorage as unknown as Record<string, string>).zcodeStorageProbeAssign;
    expect(storageSnapshot().local.length).toBe(probeBaseline.local.length);
    expect(storageSnapshot().local.keys).toEqual(probeBaseline.local.keys);
  });
});

describe("ZCode 待处理卡：审批只放行一次性选项并展示命令正文", () => {
  function permissionPending(detail: string | null = null): PendingView {
    return pendingView({
      kind: "permission",
      toolName: "Bash",
      summary: "执行合成命令",
      questions: [],
      detail,
      options: [
        { optionId: "host-allow-once-9", label: "允许一次", kind: "allowOnce" },
        { optionId: "host-deny-9", label: "拒绝", kind: "deny" },
        { optionId: "allowAlways", label: "始终允许（项目）", kind: "allowAlways" },
        { optionId: "allowSession", label: "始终允许（会话）", kind: "allowAlways" },
        { optionId: "fullAccess", label: "完全访问", kind: "custom" },
      ],
    });
  }

  it("renders_only_once_and_deny_vertically", async () => {
    const { fn, calls } = makeSubmit();
    render(<ZcodePendingCard {...baseProps({ pending: permissionPending(), submit: fn })} />);

    expect(screen.getByText("允许一次")).toBeVisible();
    expect(screen.getByText("拒绝")).toBeVisible();
    // 常驻授权与完全访问一律不渲染
    expect(screen.queryByTestId("zcode-permission-option-allowAlways")).toBeNull();
    expect(screen.queryByTestId("zcode-permission-option-allowSession")).toBeNull();
    expect(screen.queryByTestId("zcode-permission-option-fullAccess")).toBeNull();
    expect(screen.queryByText("始终允许（项目）")).toBeNull();
    expect(screen.queryByText("完全访问")).toBeNull();

    const options = screen.getByTestId("zcode-permission-options");
    expect(options.className).toContain("flex-col");

    // 问答蓝 / 审批红：tone 由卡片根上的 data-tone 暴露
    expect(screen.getByTestId("zcode-pending-card-body").getAttribute("data-tone")).toBe("approve");

    fireEvent.click(screen.getByTestId("zcode-permission-option-host-deny-9"));
    await waitFor(() => expect(calls).toHaveLength(1));
    expect(calls[0]).toEqual({
      kind: "permission",
      sessionId: SID,
      generation: GEN,
      interactionId: INTERACTION,
      optionId: "host-deny-9",
    });
  });

  it("question_card_uses_blue_tone", () => {
    render(
      <ZcodePendingCard {...baseProps({ pending: pendingView({ questions: [questionView()] }) })} />
    );
    expect(screen.getByTestId("zcode-pending-card-body").getAttribute("data-tone")).toBe(
      "question"
    );
  });

  it("detail_is_collapsed_read_only_and_never_submitted", async () => {
    const { fn, calls } = makeSubmit();
    render(
      <ZcodePendingCard
        {...baseProps({ pending: permissionPending("rm -rf /tmp/scratch"), submit: fn })}
      />
    );

    // 默认折叠：正文不占屏，且不得出现指向未渲染元素的悬空 aria-controls
    expect(screen.queryByTestId("zcode-pending-detail")).toBeNull();
    const toggle = screen.getByTestId("zcode-pending-detail-toggle") as HTMLButtonElement;
    expect(toggle.getAttribute("aria-expanded")).toBe("false");
    expect(toggle.getAttribute("aria-controls")).toBeNull();

    fireEvent.click(toggle);
    const body = screen.getByTestId("zcode-pending-detail");
    expect(body.textContent).toBe("rm -rf /tmp/scratch");
    expect(body.tagName).toBe("PRE");
    expect(toggle.getAttribute("aria-expanded")).toBe("true");

    // 折叠回去：正文消失；展开/折叠不产生任何提交
    fireEvent.click(toggle);
    expect(screen.queryByTestId("zcode-pending-detail")).toBeNull();
    expect(calls).toHaveLength(0);
  });

  it("missing_detail_renders_no_toggle", () => {
    render(<ZcodePendingCard {...baseProps({ pending: permissionPending(null) })} />);
    expect(screen.queryByTestId("zcode-pending-detail-toggle")).toBeNull();
  });
});
