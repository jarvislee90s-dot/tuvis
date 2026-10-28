/**
 * F03/F06／R15：**正式答复气泡**（"问题／答案"）。
 *
 * 后端把官方信封正文剥掉后只下发 `interactionAnswer`（questionKey/question/
 * answer/correlated）。消息列表必须消费它——否则纯信封消息会变成一片空白，
 * 换设备/重进会话看不到正式答复。`correlated=false` 如实标注"未关联"，不假装
 * 已确认送达。
 */
import i18n from "@/i18n";
import type { CodexInteractionAnswerSummary } from "./codexIpcApi";

export interface AnswerBubbleProps {
  answer: CodexInteractionAnswerSummary;
  messageId: string;
}

export default function AnswerBubble({ answer, messageId }: AnswerBubbleProps) {
  return (
    <div
      data-testid={`codex-answer-${messageId}`}
      data-correlated={answer.correlated ? "true" : "false"}
      className="max-w-[85%] rounded-2xl border border-[var(--bd)] bg-[var(--cd)] px-3 py-2 text-sm"
    >
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
  );
}
