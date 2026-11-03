/**
 * B02：**主区域只一行**——可用操作标签 + 一个「适配详情」入口。
 *
 * - 只消费后端 `effectiveOperations`（单一事实源），不自行推断；
 * - T03：芯片由**支持状态**（`support`，稳定展示面）驱动，不由 `allowed`（实时门）
 *   驱动——运行中/暂停/短暂断链的当前原因只放适配详情，不把
 *   "已支持"闪成"该版本不可用"，芯片位置与存在性稳定；
 * - 主行仅保留能力名称；逐项支持状态、临时原因和未匹配项全部进入详情；
 *   未实现操作不出现（前端不猜）；
 * - 不可用项证据与完整原因只放适配详情，主区域没有「更多 N / N 项不可用」；
 * - 标签区单独横向滚动，详情入口固定在行尾（窄屏也始终可点到）；
 * - 不发起写动作：这个组件只做导航与解释。
 */
import { useMemo } from "react";
import i18n from "@/i18n";
import type { CodexEffectiveOperations, CodexOperation, CodexSupportState } from "./codexIpcApi";
import { supportOf } from "./codexIpcApi";

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

/** T03：支持态文案（zh/en 同步；单源，详情页复用）。 */
export function codexSupportText(support: CodexSupportState): string {
  const table: Record<CodexSupportState, string> = {
    supported: "mobile.codex.ops.support.supported",
    identifying: "mobile.codex.ops.support.identifying",
    unmatched: "mobile.codex.ops.support.unmatched",
    not_implemented: "mobile.codex.ops.support.notImplemented",
  };
  return i18n.t(table[support]);
}

/** T03：当前原因文案（只解释"现在为什么不能"，不改支持展示；词表外 → 原机器码）。 */
export function codexReasonText(reasonCode: string): string {
  const table: Record<string, string> = {
    operation_runtime_active: "mobile.codex.ops.reasons.running",
    operation_blocking_pending: "mobile.codex.ops.reasons.pendingNative",
    operation_queue_paused: "mobile.codex.ops.reasons.queuePaused",
    operation_auto_adapt_off: "mobile.codex.ops.reasons.adaptOff",
    operation_target_stale: "mobile.codex.ops.reasons.stale",
    operation_runtime_unknown: "mobile.codex.ops.reasons.runtimeUnknown",
  };
  const key = table[reasonCode];
  return key ? i18n.t(key) : reasonCode;
}

export interface CodexOperationToolbarProps {
  operations?: CodexEffectiveOperations;
  onOpenDetails?: () => void;
  /** R07：连接恢复中（同身份重连窗内沿用了上次支持展示）。只影响行尾的
   *  「连接恢复中」提示，不改变芯片的已支持判定与任何资格。 */
  reconnecting?: boolean;
  /** C03：沿用的展示身份**尚未通过本代际内容 SHA 核验**（「待核对」）。与
   *  「连接恢复中」并列如实显示，不改变芯片判定与任何资格。 */
  identityPending?: boolean;
}

export default function CodexOperationToolbar({
  operations,
  onOpenDetails,
  reconnecting,
  identityPending,
}: CodexOperationToolbarProps) {
  const { supported, known } = useMemo(() => {
    if (!operations) return { supported: [], known: false };
    const entries = Object.entries(operations.operations) as [
      CodexOperation,
      NonNullable<CodexEffectiveOperations["operations"][CodexOperation]>,
    ][];
    return {
      // T03：芯片 = 已实现**且已支持**的操作（稳定位置）；`allowed` 只决定是否
      // 在详情中解释当前原因，主行不重复铺开。
      supported: entries.filter(([, status]) => supportOf(status) === "supported"),
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
        ) : (
          <>
            {supported.map(([operation]) => (
              <span
                key={operation}
                data-testid={`codex-ops-chip-${operation}`}
                className="shrink-0 rounded-full border border-[var(--bd)] px-2 py-0.5"
              >
                {opLabel(operation)}
              </span>
            ))}
            {supported.length === 0 && (
              <span className="shrink-0 text-[var(--mut)]">
                {i18n.t("mobile.codex.ops.noneAvailable")}
              </span>
            )}
          </>
        )}
      </div>
      {/* R07：同身份重连窗内沿用了上次支持展示 → 如实标「连接恢复中」（它只解释
          芯片为何仍显示已支持，不参与任何资格判定）。 */}
      {reconnecting && (
        <span data-testid="codex-ops-reconnecting" className="shrink-0 text-amber-700">
          {i18n.t("mobile.codex.ops.reconnecting")}
        </span>
      )}
      {/* C03：沿用中的展示身份尚未通过本代际内容 SHA 核验 → 「待核对」。与已确认
          能力明确区分；只解释展示来源，不参与任何资格判定。 */}
      {identityPending && (
        <span data-testid="codex-ops-identity-pending" className="shrink-0 text-amber-700">
          {i18n.t("mobile.codex.ops.identityPending")}
        </span>
      )}
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
