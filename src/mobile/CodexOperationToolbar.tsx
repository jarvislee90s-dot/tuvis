/**
 * B02：**主区域只一行**——可用操作标签 + 一个「适配详情」入口。
 *
 * - 只消费后端 `effectiveOperations`（单一事实源），不自行推断；
 * - 不可用项、证据与原因**只放适配详情**，主区域没有「更多 N / N 项不可用」；
 * - 标签区单独横向滚动，详情入口固定在行尾（窄屏也始终可点到）；
 * - 不发起写动作：这个组件只做导航与解释。
 */
import { useMemo } from "react";
import i18n from "@/i18n";
import type { CodexEffectiveOperations, CodexOperation } from "./codexIpcApi";

function opLabel(operation: CodexOperation): string {
  const table: Partial<Record<CodexOperation, string>> = {
    send: i18n.t("mobile.codex.ops.labels.send"),
    command_approval: i18n.t("mobile.codex.ops.labels.approve"),
    file_change_approval: i18n.t("mobile.codex.ops.labels.fileApprove"),
    blocking_input: i18n.t("mobile.codex.ops.labels.answer"),
    async_answer_idle: i18n.t("mobile.codex.ops.labels.asyncAnswer"),
    interrupt: i18n.t("mobile.codex.ops.labels.stop"),
  };
  return table[operation] ?? operation;
}

export interface CodexOperationToolbarProps {
  operations?: CodexEffectiveOperations;
  onOpenDetails?: () => void;
}

export default function CodexOperationToolbar({
  operations,
  onOpenDetails,
}: CodexOperationToolbarProps) {
  const { available, known } = useMemo(() => {
    if (!operations) return { available: [] as [CodexOperation, unknown][], known: false };
    const entries = Object.entries(operations.operations) as [
      CodexOperation,
      NonNullable<CodexEffectiveOperations["operations"][CodexOperation]>,
    ][];
    return {
      available: entries.filter(([, status]) => status.implemented && status.allowed),
      known: true,
    };
  }, [operations]);

  return (
    <div data-testid="codex-ops-row" className="flex min-w-0 items-center gap-2 text-xs">
      <div
        data-testid="codex-ops-chips"
        className="flex min-w-0 flex-1 items-center gap-2 overflow-x-auto whitespace-nowrap"
      >
        <span className="shrink-0 text-[var(--mut)]">{i18n.t("mobile.codex.ops.row")}</span>
        {!known ? (
          <span className="shrink-0 text-[var(--mut)]">{i18n.t("mobile.codex.async.unknown")}</span>
        ) : available.length === 0 ? (
          <span className="shrink-0 text-[var(--mut)]">
            {i18n.t("mobile.codex.ops.noneAvailable")}
          </span>
        ) : (
          available.map(([operation]) => (
            <span
              key={operation}
              data-testid={`codex-ops-chip-${operation}`}
              className="shrink-0 rounded-full border border-[var(--bd)] px-2 py-0.5"
            >
              {opLabel(operation)}
            </span>
          ))
        )}
      </div>
      {onOpenDetails && (
        <button
          type="button"
          data-testid="codex-ops-details"
          onClick={onOpenDetails}
          className="shrink-0 text-[var(--mut)] underline"
        >
          {i18n.t("mobile.codex.ops.details")}
        </button>
      )}
    </div>
  );
}
