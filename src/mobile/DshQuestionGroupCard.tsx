// DSH 原生题组作答卡（T04）：一份官方 `userQuestions` 题组 → 逐题作答 → 整组提交。
//
// **状态权威源**：宿主投影。本卡只在拿到**已核验**的题组（`verified: true`）时
// 出现；草稿是本地乐观状态，提交后以宿主回执与再查询收口。
//
// **草稿纪律**（验收要求）：
// - 草稿按 `设备 + 实例 + 会话 + 题组(callId) + 修订(revisionToken)` 分区保存；
//   换题组或换修订 → 旧稿不串入新题；
// - 切题只改草稿，**零宿主写入**；只有「提交全部」发一次整组动作；
// - 每题保存：所选项（`selected[]`）、自由文本（`custom`）、显式跳过（空 selected
//   且无 custom）。这三态互斥关系由后端 `validate_answer_batch` 再校验一次。
import { useCallback, useEffect, useMemo, useState } from "react";
import { Pencil } from "lucide-react";
import { Button } from "@/components/ui/button";
import { useAppTranslation } from "@/hooks/use-app-translation";
import QuestionChoiceRow from "./QuestionChoiceRow";
import type { DshQuestionAnswerItem, DshQuestionGroup } from "./api";

/** 单题草稿。 */
interface QuestionDraft {
  selected: string[];
  custom: string;
  /** 用户显式点了「跳过」（与「还没答」区分）。 */
  skipped: boolean;
}

const emptyDraft = (): QuestionDraft => ({ selected: [], custom: "", skipped: false });

export interface DshQuestionGroupCardProps {
  group: DshQuestionGroup;
  live?: boolean;
  readOnly?: boolean;
  /** 草稿分区键：设备 + 实例 + 会话 + callId + revisionToken。 */
  draftKey: string;
  /** 提交中（禁用交互，防双击）。 */
  submitting?: boolean;
  disabled?: boolean;
  /** 宿主已拒/已答等收口提示（null = 无）。 */
  settledReason?: string | null;
  onSubmit: (answers: DshQuestionAnswerItem[]) => void;
  /** 本卡读取本地草稿（由父层注入，负责跨刷新持久化）。 */
  loadDraft?: (key: string) => QuestionDraft | null;
  saveDraft?: (key: string, draft: QuestionDraft) => void;
}

export default function DshQuestionGroupCard({
  group,
  live = false,
  readOnly = false,
  draftKey,
  submitting = false,
  disabled = false,
  settledReason = null,
  onSubmit,
  loadDraft,
  saveDraft,
}: DshQuestionGroupCardProps) {
  const { t } = useAppTranslation();
  const questions = group.questions;
  const [index, setIndex] = useState(0);
  const [freeQuestion, setFreeQuestion] = useState<string | null>(null);
  // 每题草稿：键 = 原生题目 id（不按显示序号）。挂载时从父层恢复同分区草稿。
  // 草稿装载：**只接受本分区**（draftKey 相同）且题目 id 集合同构的稿。
  // 换题组/换修订后旧稿一律丢弃——否则旧答案会串到新题上（验收硬要求）。
  const questionIds = useMemo(
    () =>
      questions
        .map((question) => question.id)
        .sort()
        .join("\u0000"),
    [questions]
  );
  const restoreDrafts = useCallback((): Record<string, QuestionDraft> => {
    const raw = loadDraft?.(draftKey) as unknown as {
      key?: string;
      ids?: string;
      drafts?: Record<string, QuestionDraft>;
    } | null;
    if (!raw || typeof raw !== "object") return {};
    // 父层存的是带分区元信息的信封；键或题目集合对不上即视为无稿。
    if (raw.key !== draftKey || raw.ids !== questionIds) return {};
    return raw.drafts ?? {};
  }, [draftKey, loadDraft, questionIds]);
  const [drafts, setDrafts] = useState<Record<string, QuestionDraft>>(restoreDrafts);
  // 分区变化（换题组/换修订）→ 重新按新分区装载并回到第 1 题。
  useEffect(() => {
    setDrafts(restoreDrafts());
    setIndex(0);
    setFreeQuestion(null);
    // 只在分区键变化时重置：restoreDrafts 已随 draftKey/questionIds 变化
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [draftKey]);

  const draftOf = useCallback((id: string): QuestionDraft => drafts[id] ?? emptyDraft(), [drafts]);

  const writeDraft = useCallback(
    (id: string, next: QuestionDraft) => {
      setDrafts((current) => {
        const merged = { ...current, [id]: next };
        saveDraft?.(draftKey, {
          key: draftKey,
          ids: questionIds,
          drafts: merged,
        } as unknown as QuestionDraft);
        return merged;
      });
    },
    [draftKey, questionIds, saveDraft]
  );

  const current = questions[index];
  const currentDraft = current ? draftOf(current.id) : emptyDraft();
  const multi = current?.multiSelect === true;
  const hasOptions = (current?.options?.length ?? 0) > 0;
  const freeOpen = !hasOptions || freeQuestion === current?.id || currentDraft.custom.length > 0;
  const locked = disabled || submitting;

  const toggleOption = (label: string) => {
    if (!current || submitting) return;
    const selected = currentDraft.selected;
    const nextSelected = multi
      ? selected.includes(label)
        ? selected.filter((item) => item !== label)
        : [...selected, label]
      : selected[0] === label
        ? []
        : [label];
    setFreeQuestion(null);
    writeDraft(current.id, { selected: nextSelected, custom: "", skipped: false });
  };

  const setCustom = (value: string) => {
    if (!current || submitting) return;
    // 自由文本与选项二选一（后端也会拒并发两种）；输入文本即清空所选。
    writeDraft(current.id, { selected: [], custom: value, skipped: false });
  };

  const skipCurrent = () => {
    if (!current || submitting) return;
    setFreeQuestion(null);
    writeDraft(current.id, { selected: [], custom: "", skipped: true });
  };

  /** 每题是否已有可提交内容（选中或自由文本或显式跳过）。 */
  const answered = useMemo(
    () =>
      questions.map((question) => {
        const draft = drafts[question.id];
        if (!draft) return false;
        return draft.skipped || draft.selected.length > 0 || draft.custom.trim().length > 0;
      }),
    [drafts, questions]
  );
  const allAnswered = answered.length > 0 && answered.every(Boolean);

  const buildAnswers = useCallback((): DshQuestionAnswerItem[] => {
    return questions.map((question) => {
      const draft = drafts[question.id] ?? emptyDraft();
      if (draft.custom.trim().length > 0) {
        return { id: question.id, selected: [], custom: draft.custom.trim() };
      }
      if (draft.selected.length > 0) {
        return { id: question.id, selected: draft.selected };
      }
      // 显式跳过（或空答）= 空 selected、无 custom
      return { id: question.id, selected: [] };
    });
  }, [drafts, questions]);

  if (readOnly) {
    return (
      <section data-testid="dsh-question-console-only" className="mt-2 space-y-2 px-3 text-sm">
        {questions.map((question) => (
          <p key={question.id}>{question.question}</p>
        ))}
        <p className="text-xs text-[var(--mut)]">{t("mobile.dshQuestions.consoleOnly")}</p>
      </section>
    );
  }

  if (!live && group.state !== "continued") {
    return (
      <div data-testid="dsh-question-group" data-state="not-answerable" className="mt-2 px-1">
        <p className="text-xs text-[var(--mut)]">
          {t("mobile.dshQuestions.notAnswerable", { state: group.state })}
        </p>
      </div>
    );
  }

  return (
    <section
      data-testid="dsh-question-group"
      data-call-id={group.callId}
      data-state={group.state}
      className="mx-1 mt-1 shrink-0 rounded-xl border border-[var(--cb)] bg-[var(--cd)] px-2.5 py-2"
    >
      <div key={current?.id} className="space-y-1.5 p-0.5">
        <header className="flex items-center justify-between gap-2">
          <span className="text-xs text-[var(--mut)]" data-testid="dsh-question-progress">
            {t("mobile.dshQuestions.progress", {
              current: index + 1,
              total: questions.length,
            })}
          </span>
          {current?.header && (
            <span className="truncate text-xs font-medium" data-testid="dsh-question-header">
              {current.header}
            </span>
          )}
        </header>

        {settledReason !== null && (
          <p
            data-testid="dsh-question-settled"
            className="text-xs text-amber-600 dark:text-amber-400"
          >
            {settledReason}
          </p>
        )}

        {current && (
          <>
            <p data-testid="dsh-question-text" className="text-sm font-medium">
              {current.question}
            </p>
            {current.detail && (
              <p className="text-xs text-[var(--mut)]" data-testid="dsh-question-detail">
                {current.detail}
              </p>
            )}

            {hasOptions && (
              <div className="space-y-1">
                {(current.options ?? []).map((option, optionIndex) => (
                  <QuestionChoiceRow
                    key={option.label}
                    index={optionIndex}
                    label={option.label}
                    description={option.description}
                    inlineDescription
                    checked={currentDraft.selected.includes(option.label)}
                    multiSelect={multi}
                    disabled={locked}
                    testId={`dsh-question-option-${optionIndex}`}
                    onClick={() => toggleOption(option.label)}
                  />
                ))}
              </div>
            )}

            {freeOpen ? (
              <textarea
                key={current.id}
                aria-label={t("mobile.dshQuestions.freeLabel")}
                placeholder={t("mobile.dshQuestions.freeLabel")}
                data-testid="dsh-question-free"
                value={currentDraft.custom}
                disabled={locked}
                onChange={(event) => setCustom(event.target.value)}
                rows={1}
                className="block min-h-8 w-full resize-y rounded-lg border border-[var(--cb)] bg-transparent px-2 py-1 text-sm focus-visible:ring-1 focus-visible:ring-[var(--btnp)] focus-visible:outline-none"
              />
            ) : (
              <button
                type="button"
                data-testid="dsh-question-free-toggle"
                className="inline-flex min-h-7 items-center gap-1.5 rounded-md px-2 text-xs text-[var(--mut)] hover:bg-[var(--btnp)]/10 disabled:opacity-40"
                disabled={locked}
                onClick={() => setFreeQuestion(current.id)}
              >
                <Pencil size={12} aria-hidden="true" />
                {t("mobile.dshQuestions.freeLabel")}
              </button>
            )}
          </>
        )}
      </div>

      <footer className="mt-1 flex flex-wrap items-center justify-between gap-x-2 gap-y-1 border-t border-[var(--cb)] pt-1.5">
        <div className="flex items-center gap-1 text-xs text-[var(--mut)]">
          {questions.length > 1 && (
            <>
              <Button
                type="button"
                variant="ghost"
                size="sm"
                className="h-7 px-2 text-xs"
                data-testid="dsh-question-prev"
                disabled={locked || index === 0}
                onClick={() => setIndex(index - 1)}
              >
                {t("mobile.dshQuestions.prev")}
              </Button>
              <Button
                type="button"
                variant="ghost"
                size="sm"
                className="h-7 px-2 text-xs"
                data-testid="dsh-question-next"
                disabled={locked || index >= questions.length - 1}
                onClick={() => setIndex(index + 1)}
              >
                {t("mobile.dshQuestions.next")}
              </Button>
            </>
          )}
          <span data-testid="dsh-question-ready" className="tabular-nums">
            {t("mobile.dshQuestions.readyCount", {
              done: answered.filter(Boolean).length,
              total: questions.length,
            })}
          </span>
          <span className="sr-only" data-testid="dsh-question-state">
            {t(
              currentDraft.skipped
                ? "mobile.dshQuestions.skipped"
                : answered[index]
                  ? "mobile.dshQuestions.answered"
                  : "mobile.dshQuestions.unanswered"
            )}
          </span>
        </div>
        <div className="ml-auto flex items-center gap-1.5">
          {current && (
            <Button
              type="button"
              size="sm"
              variant="ghost"
              className="h-7 rounded-full px-2 text-xs"
              data-testid="dsh-question-skip"
              disabled={locked}
              aria-pressed={currentDraft.skipped}
              onClick={skipCurrent}
            >
              {t(currentDraft.skipped ? "mobile.dshQuestions.skipped" : "mobile.dshQuestions.skip")}
            </Button>
          )}
          <Button
            type="button"
            size="sm"
            className="h-7 rounded-full bg-[var(--btnp)] px-3 text-xs text-[var(--btnpt)]"
            data-testid="dsh-question-submit"
            disabled={locked || !allAnswered}
            onClick={() => onSubmit(buildAnswers())}
          >
            {t(submitting ? "mobile.dshQuestions.submitting" : "mobile.dshQuestions.submit")}
          </Button>
        </div>
      </footer>
    </section>
  );
}
