import { cleanup, render, screen } from "@testing-library/react";
import { afterEach, beforeAll, describe, expect, it } from "vitest";
import i18n from "@/i18n";
import AnswerBubble from "@/mobile/AnswerBubble";

beforeAll(async () => {
  await i18n.changeLanguage("zh");
});
afterEach(cleanup);

describe("F03/F06 R15 正式答复气泡", () => {
  it("renders question and answer even when the message text was stripped", () => {
    render(
      <AnswerBubble
        messageId="u-1"
        answers={[
          { questionKey: "q", question: "部署到哪？", answer: "Staging", correlated: true },
        ]}
      />
    );
    const bubble = screen.getByTestId("codex-answer-u-1");
    expect(bubble.textContent).toContain("部署到哪？");
    expect(bubble.textContent).toContain("Staging");
    expect(bubble.getAttribute("data-correlated")).toBe("true");
    // 不泄露内部信封标记
    expect(bubble.textContent).not.toContain("send_user_message_question_reply");
    expect(bubble.textContent).not.toContain("request_user_input_async");
  });

  it("renders all pairs in envelope order inside one bubble", () => {
    const answers = [3, 1, 2].map((n) => ({
      questionKey: `q-${n}`,
      question: `Question ${n}`,
      answer: `Answer ${n}`,
      correlated: n !== 1,
    }));
    render(<AnswerBubble messageId="group" answers={answers} />);
    const bubble = screen.getByTestId("codex-answer-group");
    const content = bubble.textContent ?? "";
    const positions = answers.flatMap(({ question, answer }) => [
      content.indexOf(question),
      content.indexOf(answer),
    ]);
    expect(positions[0]).toBeGreaterThanOrEqual(0);
    for (let i = 1; i < positions.length; i++) {
      expect(positions[i]).toBeGreaterThan(positions[i - 1]);
    }
    expect(screen.getAllByTestId("codex-answer-uncorrelated-group")).toHaveLength(1);
  });

  it("marks uncorrelated answers honestly", () => {
    render(
      <AnswerBubble
        messageId="u-2"
        answers={[{ questionKey: "q", question: "选一个", answer: "甲", correlated: false }]}
      />
    );
    expect(screen.getByTestId("codex-answer-u-2").getAttribute("data-correlated")).toBe("false");
    expect(screen.getByTestId("codex-answer-uncorrelated-u-2")).toBeTruthy();
  });
});
