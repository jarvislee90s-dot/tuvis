// Tuvis 本地待发队列（Q6）：只展示本设备可见项，所有写动作由父面板编排。
// 默认展开；每行是「第 N 条 | 20 字素摘要 | 插队 / 再编辑 / 撤回」。
import i18n from "@/i18n";
import type { LocalQueueView } from "@/lib/api/zcodeControl";
import { zcodeQueuePreview } from "./zcode-local-queue-state";

export interface ZcodeLocalQueuePanelProps {
  view: LocalQueueView | null;
  busy: boolean;
  /** 仍有未人工核对的投递/停止结果：继续队列与插队都必须先核对（Task A）。 */
  reviewRequired?: boolean;
  onPrioritize: (itemId: string, expectedVersion: number) => void;
  onEdit: (itemId: string, expectedVersion: number) => void;
  onRetract: (itemId: string, expectedVersion: number) => void;
  onResume: () => void;
  onRecoverEdit?: (requestId: string) => void;
}

export default function ZcodeLocalQueuePanel({
  view,
  busy,
  reviewRequired = false,
  onPrioritize,
  onEdit,
  onRetract,
  onResume,
  onRecoverEdit,
}: ZcodeLocalQueuePanelProps) {
  if (view === null) return null;
  const visible = view.items.filter(
    (item) => item.state !== "cancelled" && item.state !== "accepted"
  );
  if (visible.length === 0 && view.recoverableEdits.length === 0 && view.autoDrain) return null;

  return (
    <section
      data-testid="zcode-local-queue"
      data-version={view.version}
      data-auto-drain={view.autoDrain ? "true" : "false"}
      className="mb-2 rounded-lg border border-[var(--cb)] bg-[var(--cbg)] px-2 py-1.5"
    >
      <div className="flex flex-wrap items-center gap-2">
        <span className="text-xs font-medium text-[var(--tx)]">
          {i18n.t("mobile.zcodeControl.localQueueTitle")}
        </span>
        <span className="text-[11px] text-[var(--mut)]">{visible.length}</span>
        {!view.autoDrain && (
          <button
            type="button"
            data-testid="zcode-local-queue-resume"
            disabled={busy || reviewRequired}
            title={
              reviewRequired ? i18n.t("mobile.zcodeControl.localQueueReviewRequired") : undefined
            }
            onClick={onResume}
            className="ml-auto rounded-full border border-[var(--cb)] px-2 py-0.5 text-[11px] text-[var(--tx)] disabled:opacity-40"
          >
            {i18n.t("mobile.zcodeControl.localQueueResume")}
          </button>
        )}
      </div>
      {reviewRequired && (
        <p
          data-testid="zcode-local-queue-review-required"
          data-reason-code="queue_review_required"
          className="mt-1 text-[11px] text-[var(--mut)]"
        >
          {i18n.t("mobile.zcodeControl.localQueueReviewRequired")}
        </p>
      )}
      {!view.autoDrain && (
        <p data-testid="zcode-local-queue-paused" className="mt-1 text-[11px] text-[var(--mut)]">
          {i18n.t("mobile.zcodeControl.localQueuePaused")}
        </p>
      )}
      {visible.length > 0 && (
        <ul className="mt-1 divide-y divide-[var(--cb)]">
          {visible.map((item) => {
            const writable = !busy && item.state === "queued";
            // 插队会改变派发顺序：未核对 barrier 存在时必须先在“待核对结果”里核对。
            const canPrioritize = writable && !reviewRequired;
            return (
              <li
                key={item.itemId}
                data-testid="zcode-local-item"
                data-item-id={item.itemId}
                data-item-version={item.itemVersion}
                className="flex flex-wrap items-center gap-x-2 gap-y-1 py-1"
              >
                <span className="shrink-0 text-[11px] text-[var(--mut)]">
                  {i18n.t("mobile.zcodeControl.localQueuePosition", { position: item.position })}
                </span>
                <span
                  className="min-w-0 flex-1 truncate text-xs text-[var(--tx)]"
                  title={item.textPreview}
                >
                  {zcodeQueuePreview(item.textPreview)}
                </span>
                {item.state !== "queued" && (
                  <span
                    data-testid="zcode-local-item-state"
                    data-state={item.state}
                    className="shrink-0 text-[10px] text-[var(--mut)]"
                  >
                    {i18n.t(
                      item.state === "unknown"
                        ? "mobile.zcodeControl.localQueueStateUnknown"
                        : item.state === "dispatching"
                          ? "mobile.zcodeControl.localQueueStateDispatching"
                          : "mobile.zcodeControl.localQueueStateFailed"
                    )}
                  </span>
                )}
                {item.hasAttachments && (
                  <span
                    data-testid="zcode-local-item-attachments"
                    title={i18n.t("mobile.zcodeControl.localQueueHasAttachments")}
                    className="shrink-0 text-[11px] text-[var(--mut)]"
                  >
                    📎
                  </span>
                )}
                <button
                  type="button"
                  disabled={!canPrioritize}
                  onClick={() => onPrioritize(item.itemId, item.itemVersion)}
                  className="shrink-0 rounded-full border border-[var(--cb)] px-2 py-0.5 text-[11px] text-[var(--tx)] disabled:opacity-40"
                >
                  {i18n.t("mobile.zcodeControl.localQueuePrioritize")}
                </button>
                <button
                  type="button"
                  disabled={!writable}
                  onClick={() => onEdit(item.itemId, item.itemVersion)}
                  className="shrink-0 rounded-full border border-[var(--cb)] px-2 py-0.5 text-[11px] text-[var(--tx)] disabled:opacity-40"
                >
                  {i18n.t("mobile.zcodeControl.localQueueEdit")}
                </button>
                <button
                  type="button"
                  disabled={!writable}
                  onClick={() => onRetract(item.itemId, item.itemVersion)}
                  className="shrink-0 rounded-full border border-[var(--cb)] px-2 py-0.5 text-[11px] text-[var(--tx)] disabled:opacity-40"
                >
                  {i18n.t("mobile.zcodeControl.localQueueRetract")}
                </button>
              </li>
            );
          })}
        </ul>
      )}
      {view.recoverableEdits.length > 0 && (
        <div className="mt-1 flex flex-wrap gap-2">
          {view.recoverableEdits.map((edit) => (
            <button
              key={edit.requestId}
              type="button"
              disabled={busy || onRecoverEdit === undefined}
              onClick={() => onRecoverEdit?.(edit.requestId)}
              className="rounded-full border border-[var(--cb)] px-2 py-0.5 text-[11px] text-[var(--tx)] disabled:opacity-40"
            >
              {i18n.t("mobile.zcodeControl.localQueueRecoverEdit")}
            </button>
          ))}
        </div>
      )}
    </section>
  );
}
