import i18n from "@/i18n";
import type { CodexInteractionAnswerSummary } from "./codexIpcApi";

export interface AnswerBubbleProps {
  answers: CodexInteractionAnswerSummary[];
  messageId: string;
}

/** 一条用户消息的正式答复按信封顺序显示在同一气泡。 */
export default function AnswerBubble({ answers, messageId }: AnswerBubbleProps) {
  return (
    <div
      data-testid={`codex-answer-${messageId}`}
      data-correlated={answers.every((answer) => answer.correlated) ? "true" : "false"}
      className="max-w-[85%] space-y-3 rounded-2xl border border-[var(--bd)] bg-[var(--cd)] px-3 py-2 text-sm"
    >
      {answers.map((answer, index) => (
        <div key={`${answer.questionKey}:${index}`}>
          <div className="text-xs text-[var(--mut)]">
            {answer.question || i18n.t("mobile.codex.async.question")}
          </div>
          <div className="mt-1 whitespace-pre-wrap text-[var(--tx)]">{answer.answer}</div>
          {!answer.correlated && (
            <div
              data-testid={`codex-answer-uncorrelated-${messageId}`}
              className="mt-1 text-xs text-amber-700 dark:text-amber-400"
            >
              {i18n.t("mobile.codex.async.uncorrelated")}
            </div>
          )}
        </div>
      ))}
    </div>
  );
}
