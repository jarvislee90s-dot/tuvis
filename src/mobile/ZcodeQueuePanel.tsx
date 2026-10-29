// ZCode 官方输入队列面板（R2）：只读、紧凑、默认一行。
//
// 为什么存在：官方在运行中把新消息放进**自己的**输入队列（桌面端能看到队列与
// 「已暂停」），手机此前完全看不到。这里只做官方 `state.queue` 的投影展示——
// 不建自建队列、不提供插队/撤回/立即发送（那些是官方写命令，本任务不做），
// 也绝不把「当前忙」的本地猜测当成「已入队」事实。
//
// 三态纪律（与 Rust `QueueView` 逐字段对应）：
// - `known !== true`（缺字段/宿主未宣告）→ **不渲染**：未知绝不冒充「空队列」；
// - `known=true` + `items=[]` + 未暂停 → 保留根节点（`data-state="empty"`）但无可见
//   文本：与「未知」可区分，又不给每个会话常驻一段空文案；
// - `paused=true` → **即使队列已空**也保留一行暂停标志（官方暂停是容器级状态，
//   清空不等于恢复）；`pauseReason` 只认官方枚举 `stopped|manual|error` 的可读文案，
//   其它值只显示「已暂停」、不猜原因；
// - `paused === null`（宿主未给 `autoDrain`）→ 「暂停状态未知」，既不写「已暂停」
//   也不写「运行中」。
import { useState } from "react";
import i18n from "@/i18n";
import type { QueueView } from "@/lib/api/zcodeControl";

/** 官方 `pauseReason` 枚举 → 文案键；非枚举值一律不映射（不猜原因）。 */
const PAUSE_REASON_KEYS: Record<string, string> = {
  stopped: "mobile.zcodeControl.queuePauseStopped",
  manual: "mobile.zcodeControl.queuePauseManual",
  error: "mobile.zcodeControl.queuePauseError",
};

/** 暂停态 → 根节点 `data-state`（测试与样式共用同一套词）。 */
function queueState(queue: QueueView): "empty" | "paused" | "pause-unknown" | "active" {
  // 暂停优先于「空」：官方在队列清空后仍可能保持 autoDrain=false（如回合被停止
  // 后 drain 只剩暂停态），把这条事实藏起来会让手机误以为可以正常发送。
  if (queue.paused === true) return "paused";
  if (queue.items.length === 0) return "empty";
  if (queue.paused === null) return "pause-unknown";
  return "active";
}

/** 官方暂停原因的可读文案键（只认枚举三值，其它 = 无文案）。 */
function pauseReasonKey(queue: QueueView): string | null {
  if (queue.paused !== true) return null;
  const reason = queue.pauseReason;
  if (typeof reason !== "string") return null;
  return PAUSE_REASON_KEYS[reason] ?? null;
}

export default function ZcodeQueuePanel({ queue }: { queue?: QueueView | null }) {
  const [expanded, setExpanded] = useState(false);

  // 未知 ≠ 空：宿主没宣告队列时什么都不说（旧载荷同样走这里）。
  if (queue === undefined || queue === null || queue.known !== true) return null;

  const state = queueState(queue);
  // 已知空队列且未暂停：不占位（无可见文本），但保留根节点让「空」与「未知」可区分。
  if (state === "empty") return <div data-testid="zcode-queue" data-state="empty" hidden />;

  const reasonKey = pauseReasonKey(queue);
  return (
    <div
      data-testid="zcode-queue"
      data-state={state}
      className="mb-2 rounded-lg border border-[var(--cb)] px-2 py-1"
    >
      <button
        type="button"
        data-testid="zcode-queue-toggle"
        aria-expanded={expanded}
        aria-label={i18n.t("mobile.zcodeControl.queueToggleAria")}
        onClick={() => setExpanded((prev) => !prev)}
        className="flex w-full flex-wrap items-center gap-2 text-left text-xs text-[var(--mut)]"
      >
        <span data-testid="zcode-queue-count">
          {i18n.t("mobile.zcodeControl.queueCount", { count: queue.items.length })}
        </span>
        {queue.paused === true && (
          <span data-testid="zcode-queue-paused" className="text-amber-700 dark:text-amber-400">
            {i18n.t("mobile.zcodeControl.queuePaused")}
          </span>
        )}
        {reasonKey !== null && (
          <span data-testid="zcode-queue-pause-reason">{i18n.t(reasonKey)}</span>
        )}
        {queue.paused === null && (
          <span data-testid="zcode-queue-pause-unknown">
            {i18n.t("mobile.zcodeControl.queuePauseUnknown")}
          </span>
        )}
      </button>
      {expanded && queue.items.length > 0 && (
        <ul data-testid="zcode-queue-items" className="mt-1 space-y-1">
          {queue.items.map((item) => (
            <li
              key={item.inputId}
              data-testid={`zcode-queue-item-${item.inputId}`}
              className="flex gap-1.5 text-xs text-[var(--tx)]"
            >
              {/* 位置只在宿主给了 queuePosition 时展示（0 基 → 1 基），缺席不猜。 */}
              {item.position !== null && (
                <span
                  data-testid={`zcode-queue-position-${item.inputId}`}
                  className="shrink-0 text-[var(--mut)]"
                >
                  #{item.position + 1}
                </span>
              )}
              <span className="break-words whitespace-pre-wrap">{item.text}</span>
            </li>
          ))}
        </ul>
      )}
    </div>
  );
}
