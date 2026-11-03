/** CLI 统一的选项行（2026-10-28）：全宽、序号徽标、可选 `[✓]/[ ]` 勾选字形、
 * 粗标题 + 描述；选中态用中性强调环，与终端问答卡/审批卡同一视觉语言。
 *
 * 不承载业务语义：多选勾选字形只在调用方确有 multiSelect 契约时传入；
 * 没有该契约的卡片不得伪造多选。 */
export interface QuestionChoiceRowProps {
  index: number;
  label: string;
  description?: string;
  checked?: boolean;
  multiSelect?: boolean;
  disabled?: boolean;
  testId?: string;
  onClick: () => void;
}

export default function QuestionChoiceRow({
  index,
  label,
  description,
  checked = false,
  multiSelect = false,
  disabled = false,
  testId,
  onClick,
}: QuestionChoiceRowProps) {
  return (
    <button
      type="button"
      data-testid={testId}
      data-checked={multiSelect && checked ? "true" : undefined}
      aria-pressed={checked}
      disabled={disabled}
      onClick={onClick}
      className={`flex w-full items-start gap-2 rounded-lg px-2.5 py-1.5 text-left text-sm disabled:opacity-40 ${
        checked
          ? "bg-[var(--btnp)]/15 text-[var(--tx)] ring-1 ring-[var(--btnp)]"
          : "bg-[var(--btnp)]/5 text-[var(--tx)] hover:bg-[var(--btnp)]/10 dark:hover:bg-[var(--bub)]"
      }`}
    >
      <span
        aria-hidden="true"
        className="mt-0.5 inline-flex h-4 w-4 shrink-0 items-center justify-center rounded bg-[var(--btnp)]/15 text-[10px] font-semibold text-[var(--tx)]"
      >
        {index + 1}
      </span>
      {multiSelect && (
        <span aria-hidden="true" className="mt-0.5 mr-0.5 font-mono text-xs text-[var(--tx)]">
          {checked ? "[✓]" : "[ ]"}
        </span>
      )}
      <span className="min-w-0">
        <span className="block font-medium">{label}</span>
        {description && (
          <span className="mt-0.5 block text-xs text-[var(--mut)]">{description}</span>
        )}
      </span>
    </button>
  );
}
