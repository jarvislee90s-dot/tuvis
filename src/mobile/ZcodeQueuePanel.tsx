// ZCode 官方输入队列面板（R2）：只读、紧凑、默认一行。
//
// 为什么存在：官方在运行中把新消息放进**自己的**输入队列（桌面端能看到队列与
// 「已暂停」），手机此前完全看不到。这里只做官方 `state.queue` 的投影展示——
// 不建自建队列、不提供插队/撤回/立即发送（那些是官方写命令，本任务不做），
// 也绝不把「当前忙」的本地猜测当成「已入队」事实。
//
// 三态纪律（与 Rust `QueueView` 逐字段对应）：
// - `known !== true`（缺字段/宿主未宣告）→ **不渲染**：未知绝不冒充「空队列」；
// - `known=true` + `items=[]` → 保留根节点（`data-state="empty"`）但无可见文本：
//   与「未知」可区分，又不给每个会话常驻一段空文案；
// - `paused === null`（宿主未给 `autoDrain`）→ 「暂停状态未知」，既不写「已暂停」
//   也不写「运行中」。
import { useState } from "react";
import i18n from "@/i18n";
import type { QueueView } from "@/lib/api/zcodeControl";

/** 暂停态 → 根节点 `data-state`（测试与样式共用同一套词）。 */
function queueState(queue: QueueView): "empty" | "paused" | "pause-unknown" | "active" {
  if (queue.items.length === 0) return "empty";
  if (queue.paused === true) return "paused";
  if (queue.paused === null) return "pause-unknown";
  return "active";
}

export default function ZcodeQueuePanel({ queue }: { queue?: QueueView | null }) {
  const [expanded, setExpanded] = useState(false);

  // 未知 ≠ 空：宿主没宣告队列时什么都不说（旧载荷同样走这里）。
  if (queue === undefined || queue === null || queue.known !== true) return null;

  const state = queueState(queue);
  // 已知空队列：不占位（无可见文本），但保留根节点让「空」与「未知」可区分。
  if (state === "empty") return <div data-testid="zcode-queue" data-state="empty" hidden />;

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
        {queue.paused === null && (
          <span data-testid="zcode-queue-pause-unknown">
            {i18n.t("mobile.zcodeControl.queuePauseUnknown")}
          </span>
        )}
      </button>
      {expanded && (
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
