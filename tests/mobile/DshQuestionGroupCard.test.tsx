// @vitest-environment jsdom
import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, beforeAll, describe, expect, it, vi } from "vitest";
import i18n from "@/i18n";
import DshQuestionGroupCard, {
  type DshQuestionGroupCardProps,
} from "@/mobile/DshQuestionGroupCard";
import type { DshQuestion } from "@/mobile/api";

beforeAll(async () => {
  await i18n.changeLanguage("zh");
});

afterEach(cleanup);

function question(id: string, multiSelect = false): DshQuestion {
  return { id, question: `题目 ${id}`, options: [{ label: "A" }, { label: "B" }], multiSelect };
}

function setup(questions = [question("q1")], overrides: Partial<DshQuestionGroupCardProps> = {}) {
  const onSubmit = vi.fn();
  const props = {
    group: { callId: "call-1", state: "continued", questions },
    draftKey: "draft-1",
    onSubmit,
    ...overrides,
  };
  return { ...render(<DshQuestionGroupCard {...props} />), props, onSubmit };
}

const control = (name: string) => screen.getByTestId(`dsh-question-${name}`);
const click = (name: string) => fireEvent.click(control(name));
const option = (index: number) => control(`option-${index}`);
const enterText = (value: string) => {
  if (!screen.queryByTestId("dsh-question-free")) click("free-toggle");
  fireEvent.change(control("free"), { target: { value } });
};

describe("DshQuestionGroupCard", () => {
  it("numbers the options from one on each question", () => {
    setup([question("q1"), question("q2")]);
    expect(option(0)).toHaveTextContent(/^1A$/);
    expect(option(1)).toHaveTextContent(/^2B$/);
    click("next");
    expect(option(0)).toHaveTextContent(/^1A$/);
  });

  it("single_select_allows_exactly_one_label", () => {
    setup();
    fireEvent.click(option(0));
    expect(option(0)).toHaveAttribute("aria-pressed", "true");
    fireEvent.click(option(1));
    expect(option(0)).toHaveAttribute("aria-pressed", "false");
    expect(option(1)).toHaveAttribute("aria-pressed", "true");
  });

  it("multi_select_toggles_labels", () => {
    setup([question("q1", true)]);
    fireEvent.click(option(0));
    fireEvent.click(option(1));
    expect(option(0)).toHaveAttribute("aria-pressed", "true");
    expect(option(1)).toHaveAttribute("aria-pressed", "true");
    fireEvent.click(option(0));
    expect(option(0)).toHaveAttribute("aria-pressed", "false");
    expect(option(1)).toHaveAttribute("aria-pressed", "true");
  });

  it("free_text_excludes_selected_labels", () => {
    setup();
    fireEvent.click(option(0));
    expect(option(0)).toHaveAttribute("aria-pressed", "true");
    enterText("自定义答案");
    expect(control("free")).toHaveValue("自定义答案");
    expect(option(0)).toHaveAttribute("aria-pressed", "false");
  });

  it("submit_is_disabled_until_every_question_is_answered", () => {
    setup([question("q1"), question("q2")]);
    fireEvent.click(option(0));
    expect(control("ready")).toHaveTextContent("已答 1 / 2");
    expect(control("submit")).toBeDisabled();
    click("next");
    click("skip");
    expect(control("ready")).toHaveTextContent("已答 2 / 2");
    expect(control("submit")).toBeEnabled();
  });

  it("submit_payload_follows_question_order_and_shapes", () => {
    const { onSubmit } = setup([question("q3"), question("q1"), question("q2")]);
    click("next");
    enterText("自定义答案");
    click("next");
    click("skip");
    click("prev");
    click("prev");
    fireEvent.click(option(1));
    click("submit");
    expect(onSubmit).toHaveBeenCalledExactlyOnceWith([
      { id: "q3", selected: ["B"] },
      { id: "q1", selected: [], custom: "自定义答案" },
      { id: "q2", selected: [] },
    ]);
  });

  it("editing_and_paging_never_submit", () => {
    const { onSubmit } = setup([question("q1"), question("q2")]);
    fireEvent.click(option(0));
    click("next");
    expect(control("progress")).toHaveTextContent("第 2 / 2 题");
    enterText("自定义答案");
    click("prev");
    fireEvent.click(option(1));
    expect(control("submit")).toBeEnabled();
    expect(onSubmit).not.toHaveBeenCalled();
  });

  it("draft_is_saved_and_restored_by_key", () => {
    const saveDraft = vi.fn<NonNullable<DshQuestionGroupCardProps["saveDraft"]>>();
    const { unmount, props } = setup([question("q1"), question("q2")], { saveDraft });
    fireEvent.click(option(0));
    click("next");
    enterText("第二题草稿");
    // 草稿以「分区信封」落盘：key + 题目 id 集合 + 逐题草稿
    expect(saveDraft).toHaveBeenLastCalledWith(props.draftKey, {
      key: props.draftKey,
      ids: "q1\u0000q2",
      drafts: {
        q1: { selected: ["A"], custom: "", skipped: false },
        q2: { selected: [], custom: "第二题草稿", skipped: false },
      },
    });
    const saved = saveDraft.mock.lastCall![1];
    unmount();

    const loadDraft = vi.fn(() => saved);
    const sameKey = render(<DshQuestionGroupCard {...props} loadDraft={loadDraft} />);
    expect(loadDraft).toHaveBeenLastCalledWith(props.draftKey);
    expect(option(0)).toHaveAttribute("aria-pressed", "true");
    click("next");
    expect(control("free")).toHaveValue("第二题草稿");
    sameKey.unmount();

    // 模拟加载器为新键误返旧分区数据，题目 ID 保持不变。
    render(<DshQuestionGroupCard {...props} draftKey="draft-2" loadDraft={loadDraft} />);
    expect(loadDraft).toHaveBeenLastCalledWith("draft-2");
    expect(control("progress")).toHaveTextContent("第 1 / 2 题");
    expect(control("state")).toHaveTextContent("未作答");
  });

  it("non_continued_group_is_not_answerable", () => {
    setup(undefined, { group: { callId: "call-1", state: "open", questions: [question("q1")] } });
    expect(screen.getByTestId("dsh-question-group")).toHaveAttribute(
      "data-state",
      "not-answerable"
    );
    expect(screen.queryByTestId("dsh-question-submit")).not.toBeInTheDocument();
  });

  it("submitting_disables_interactions", () => {
    const { props, rerender } = setup();
    fireEvent.click(option(0));
    expect(control("submit")).toBeEnabled();
    click("free-toggle");
    expect(control("free")).toBeEnabled();
    rerender(<DshQuestionGroupCard {...props} submitting />);
    expect(control("submit")).toBeDisabled();
    expect(control("submit")).toHaveTextContent("提交中…");
    expect(option(0)).toBeDisabled();
    expect(control("free")).toBeDisabled();
  });
});
