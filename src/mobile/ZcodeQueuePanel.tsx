// ZCode 官方输入队列面板（W4）：一行一条 + 引导/修改/撤回 + 恢复运行。
//
// 为什么存在：官方在运行中把新消息放进**自己的**输入队列（桌面端能看到队列与
// 「已暂停」），手机此前完全看不到、也改不了。这里只做官方 `state.queue` 的投影
// 展示与**受授权约束**的入口——不建自建队列、不构造官方信封、不自行把项从列表里
// 删掉（权威队列只来自服务端），也绝不把「当前忙」的本地猜测当成「已入队」事实。
//
// 三态纪律（与 Rust `QueueView` 逐字段对应）：
// - `known !== true`（缺字段/宿主未宣告）→ **不渲染**：未知绝不冒充「空队列」；
// - `known=true` + `items=[]` + 未暂停 → 保留根节点（`data-state="empty"`）但无可见
//   文本：与「未知」可区分，又不给每个会话常驻一段空文案；
// - `paused=true` → **即使队列已空**也保留暂停标志与「恢复运行」入口（官方暂停是
//   容器级状态，清空不等于恢复）；`pauseReason` 只认官方枚举 `stopped|manual|error`
//   的可读文案，其它值只显示「已暂停」、不猜原因；
// - `paused === null`（宿主未给 `autoDrain`）→ 「暂停状态未知」，既不写「已暂停」
//   也不写「运行中」。
//
// 按钮门（计划 §5.2/§5.3）：
// - 「引导」**恒禁用**：官方侧没有「把已排队项转成 guide」的证据，绝不拿
//   `sendQueuedNow`（= 立即执行）冒充；本组件连 `onGuide` 都不收，从结构上排除误接。
// - 「修改」「撤回」= 写面硬门 + Tuvis `queueEdit` 能力 + `availability.queueEditAllowed` + `kind==="sendText"`
//   + `dispatch.state` 可写 + 非在途；带附件的项**只保留撤回**（修改会丢附件）。
// - 在途是**单写槽**：任何一项在途时所有队列动作按钮一并禁用，但只有该项显示
//   「撤回中…」；草稿与主按钮的在途语义由面板层（ZcodeControlPanel）负责。
// - 高频 SSE 只换 props：行以 `inputId` 为 key，不重挂、不抢焦点。
import { useState } from "react";
import i18n from "@/i18n";
import type { QueueItemView, QueueView } from "@/lib/api/zcodeControl";
import { queuePreview } from "./queuePreview";

/** 官方 `pauseReason` 枚举 → 文案键；非枚举值一律不映射（不猜原因）。 */
const PAUSE_REASON_KEYS: Record<string, string> = {
  stopped: "mobile.zcodeControl.queuePauseStopped",
  manual: "mobile.zcodeControl.queuePauseManual",
  error: "mobile.zcodeControl.queuePauseError",
};

/**
 * 官方 `dispatch.state` 当前仍可撤回/修改的值。`reserved` 尚未通过 W0 实机验证，
 * 因而与 `promoting`（正在提升执行）、`drained`（已消费）、未知和缺字段一样保守关闭。
 *
 * **与后端同一个集合**：Rust `services::zcode_control::types::WRITABLE_DISPATCH_STATES`
 * （服务端写前复核用的是它）。两端一旦漂移，就会出现「手机端按钮可点、服务端拒收」的
 * 静默不一致——修改本集合必须同步改那一侧。
 */
const WRITABLE_DISPATCH_STATES: ReadonlySet<string> = new Set(["queued", "admitted"]);
const KNOWN_DISPATCH_STATES: ReadonlySet<string> = new Set([
  "queued",
  "admitted",
  "reserved",
  "promoting",
  "drained",
]);

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

/** `dispatch.state` 是否仍在可撤回/可修改的白名单内（未知 = 否）。 */
export function dispatchWritable(item: QueueItemView): boolean {
  return item.dispatchState !== null && WRITABLE_DISPATCH_STATES.has(item.dispatchState);
}

/** 行首序号文案：官方没给 `order.queuePosition` 就显示「—」，绝不假造。 */
export function positionLabel(item: QueueItemView): string {
  return item.position === null ? "—" : `#${item.position + 1}`;
}

export interface ZcodeQueuePanelProps {
  queue?: QueueView | null;
  /** 写面硬门（paired + ready + 代际新鲜）：关闭时所有队列动作禁用。 */
  canWrite?: boolean;
  /** 面板内任一写动作在途（含审批/问答）：共享单写槽，禁用队列的其他入口。 */
  writeBusy?: boolean;
  /** 面板合并 Tuvis `queueEdit` 能力与官方 `availability.queueEdit.allowed` 后的双层资格。 */
  queueEditAllowed?: boolean;
  /** `capabilities.queueResume.enabled`（恢复队列自动处理的独立授权槽位）。 */
  queueResumeAllowed?: boolean;
  /** 当前在途的队列项（单写槽：只影响 loading 标识，不影响权威队列）。 */
  busyItemId?: string | null;
  /** 恢复动作在途。 */
  busyResume?: boolean;
  /** 已由**新鲜**队列确认 `paused=false`（只有那时面板才说「已恢复」）。 */
  resumeConfirmed?: boolean;
  /**
   * 「修改」：把该项的**完整原文**撤回到输入框（撤回 + 回填 + 冲突处理都由面板层
   * 负责；队列组件只传官方 `inputId`）。异步过程由面板层自己兜错，这里不 await。
   */
  onEditToDraft?: (queueItemId: string) => void;
  /** 「撤回」：删一条官方队列项（不触发任何回填）。 */
  onDelete?: (queueItemId: string) => void;
  /** 「移到最前/上移/下移」：显式目标项 ID，绝不传 null（后端会拒）。 */
  onMove?: (queueItemId: string, beforeQueueItemId: string) => void;
  /** 「恢复运行」：只发 `queueResume`，绝不自动发送/自动点击。 */
  onResume?: () => void;
}

export default function ZcodeQueuePanel({
  queue,
  canWrite = false,
  writeBusy = false,
  queueEditAllowed = false,
  queueResumeAllowed = false,
  busyItemId = null,
  busyResume = false,
  resumeConfirmed = false,
  onEditToDraft,
  onDelete,
  onMove,
  onResume,
}: ZcodeQueuePanelProps) {
  const [expanded, setExpanded] = useState(false);
  /** 当前展开「重排菜单」的项（按 inputId，不按下标：SSE 增删不串行）。 */
  const [menuFor, setMenuFor] = useState<string | null>(null);

  // 未知 ≠ 空：宿主没宣告队列时什么都不说（旧载荷同样走这里）。
  if (queue === undefined || queue === null || queue.known !== true) return null;

  const state = queueState(queue);
  // 已知空队列且未暂停：不占位（无可见文本），但保留根节点让「空」与「未知」可区分。
  // 例外：刚确认「已恢复」时留一行确认（否则确认文案会随队列清空被吞掉）。
  if (state === "empty" && !resumeConfirmed) {
    return <div data-testid="zcode-queue" data-state="empty" hidden />;
  }

  const reasonKey = pauseReasonKey(queue);
  const items = queue.items;
  /** 单写槽：任何队列动作在途 → 所有动作按钮禁用（绝不产生第二个写命令）。 */
  const busy = writeBusy || busyItemId !== null || busyResume;
  const canResume = canWrite && queueResumeAllowed && !busy;
  /** 全局禁用原因（写面/资格）：与条目无关，只在列表上方说一次。 */
  const globalEditReason = !canWrite
    ? i18n.t("mobile.zcodeControl.actionBlocked")
    : !queueEditAllowed
      ? i18n.t("mobile.zcodeControl.capabilityQueueEditClosed")
      : null;
  /** 引导恒禁用：官方没有「已排队项转引导」的证据，绝不接 `queueNow`。 */
  const guideReason = i18n.t("mobile.zcodeControl.queueGuideReason");

  /** 未验证/未来状态不冒充已消费；reserved 在 W0 实机门通过前单独保守禁用。 */
  const itemDispatchReason = (item: QueueItemView): string | null => {
    if (
      item.dispatchState === null ||
      item.dispatchState === "reserved" ||
      !KNOWN_DISPATCH_STATES.has(item.dispatchState)
    ) {
      return i18n.t("mobile.zcodeControl.queueItemStateUnverifiedReason");
    }
    if (!dispatchWritable(item)) return i18n.t("mobile.zcodeControl.queueItemConsumedReason");
    return null;
  };

  /** 条目级「修改」禁用原因（与全局原因二选一展示）。 */
  const itemEditReason = (item: QueueItemView): string | null => {
    if (item.kind !== "sendText") return i18n.t("mobile.zcodeControl.queueEditKindReason");
    const dispatchReason = itemDispatchReason(item);
    if (dispatchReason !== null) return dispatchReason;
    if (item.hasAttachments) return i18n.t("mobile.zcodeControl.queueEditAttachmentsReason");
    return null;
  };

  /** 条目级「撤回」禁用原因（带附件不影响撤回）。 */
  const itemDeleteReason = (item: QueueItemView): string | null => {
    if (item.kind !== "sendText") return i18n.t("mobile.zcodeControl.queueEditKindReason");
    return itemDispatchReason(item);
  };

  const renderEditButton = (item: QueueItemView) => {
    const reason = globalEditReason ?? itemEditReason(item);
    return (
      <button
        type="button"
        data-testid={`zcode-queue-edit-${item.inputId}`}
        disabled={reason !== null || busy}
        title={reason ?? undefined}
        onClick={() => onEditToDraft?.(item.inputId)}
        className="shrink-0 rounded-full border border-[var(--cb)] px-2 py-0.5 text-[11px] leading-4 text-[var(--tx)] disabled:opacity-40"
      >
        {i18n.t("mobile.zcodeControl.queueEdit")}
      </button>
    );
  };

  const renderDeleteButton = (item: QueueItemView) => {
    const reason = globalEditReason ?? itemDeleteReason(item);
    const deleting = busyItemId === item.inputId;
    return (
      <button
        type="button"
        data-testid={`zcode-queue-delete-${item.inputId}`}
        data-busy={deleting ? "true" : "false"}
        // ACK 只到「撤回中」：项真正消失要靠新鲜队列确认（面板层负责）。
        disabled={reason !== null || busy}
        title={reason ?? undefined}
        onClick={() => onDelete?.(item.inputId)}
        className="shrink-0 rounded-full border border-[var(--cb)] px-2 py-0.5 text-[11px] leading-4 text-[var(--tx)] disabled:opacity-40"
      >
        {deleting
          ? i18n.t("mobile.zcodeControl.queueDeleting")
          : i18n.t("mobile.zcodeControl.queueDelete")}
      </button>
    );
  };

  return (
    <div
      data-testid="zcode-queue"
      data-state={state}
      className="mb-2 rounded-lg border border-[var(--cb)] px-2 py-1"
    >
      <div className="flex flex-wrap items-center gap-2 text-xs text-[var(--mut)]">
        <button
          type="button"
          data-testid="zcode-queue-toggle"
          aria-expanded={expanded}
          aria-label={i18n.t("mobile.zcodeControl.queueToggleAria")}
          onClick={() => setExpanded((prev) => !prev)}
          className="flex min-w-0 flex-wrap items-center gap-2 text-left text-xs text-[var(--mut)]"
        >
          <span data-testid="zcode-queue-count">
            {i18n.t("mobile.zcodeControl.queueCount", { count: items.length })}
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
        {/* 恢复入口：`known && paused===true` 就显示——**即使队列已空**。绝不自动点击。 */}
        {queue.paused === true && (
          <button
            type="button"
            data-testid="zcode-queue-resume"
            disabled={!canResume}
            title={
              canResume ? undefined : i18n.t("mobile.zcodeControl.capabilityQueueResumeClosed")
            }
            onClick={() => onResume?.()}
            className="shrink-0 rounded-full border border-[var(--cb)] px-2 py-0.5 text-[11px] leading-4 text-[var(--tx)] disabled:opacity-40"
          >
            {busyResume
              ? i18n.t("mobile.zcodeControl.queueResuming")
              : i18n.t("mobile.zcodeControl.queueResume")}
          </button>
        )}
        {/* 「已恢复」只在**新鲜队列 paused=false**（父组件确认）后出现：不代表被打断
            的回合会重启，也不代表下一条已经在跑。 */}
        {resumeConfirmed && (
          <span data-testid="zcode-queue-resume-done" className="text-[11px]">
            {i18n.t("mobile.zcodeControl.queueResumeDone")}
          </span>
        )}
      </div>
      {items.length > 0 && (
        // 引导原因只在这里说一次（每个按钮都挂同一句会让 320px 行内挤爆）。
        <p data-testid="zcode-queue-guide-reason" className="mt-1 text-[11px] text-[var(--mut)]">
          {guideReason}
        </p>
      )}
      {items.length > 0 && globalEditReason !== null && (
        <p data-testid="zcode-queue-edit-blocked" className="text-[11px] text-[var(--mut)]">
          {globalEditReason}
        </p>
      )}
      {expanded && items.length > 0 && (
        // 320px/125% 允许**容器内**横向滚动：按钮不折行、不隐藏，整页绝不横向溢出。
        <ul data-testid="zcode-queue-items" className="mt-1 space-y-1 overflow-x-auto">
          {items.map((item, index) => {
            const editReason = globalEditReason ?? itemEditReason(item);
            const deleteReason = globalEditReason ?? itemDeleteReason(item);
            const canMove = canWrite && queueEditAllowed && dispatchWritable(item) && !busy;
            const firstId = items[0]?.inputId ?? null;
            const upId = items[index - 1]?.inputId ?? null;
            // 「下移」= 放到下一项之后：显式目标 = items[index+2] 的 ID。没有这个目标
            // 就无法在不传 null（后端会拒）的前提下表达「移末尾」，因此禁用。
            const downId = items[index + 2]?.inputId ?? null;
            const move = (before: string | null) => {
              if (before === null || !canMove) return;
              setMenuFor(null);
              onMove?.(item.inputId, before);
            };
            return (
              <li
                key={item.inputId}
                data-testid={`zcode-queue-item-${item.inputId}`}
                data-busy={busyItemId === item.inputId ? "true" : "false"}
                className="flex min-w-0 flex-col gap-0.5 text-xs text-[var(--tx)]"
              >
                {/* 视觉一行：序号 → 前 20 个字素预览 → 引导 → 修改 → 撤回。 */}
                <div
                  data-testid={`zcode-queue-row-${item.inputId}`}
                  className="flex min-w-0 items-center gap-1"
                >
                  <button
                    type="button"
                    data-testid={`zcode-queue-position-${item.inputId}`}
                    aria-haspopup="menu"
                    aria-expanded={menuFor === item.inputId}
                    aria-label={i18n.t("mobile.zcodeControl.queueMoveMenuAria", {
                      position: positionLabel(item),
                    })}
                    onClick={() =>
                      setMenuFor((prev) => (prev === item.inputId ? null : item.inputId))
                    }
                    className="shrink-0 rounded px-0.5 text-[var(--mut)]"
                  >
                    {positionLabel(item)}
                  </button>
                  {/* 预览只用于展示（`title` 放全文）；载荷永远取 `item.text` 原文。 */}
                  <span
                    data-testid={`zcode-queue-preview-${item.inputId}`}
                    title={item.text}
                    className="min-w-0 flex-1 truncate"
                  >
                    {queuePreview(item.text)}
                  </span>
                  <button
                    type="button"
                    data-testid={`zcode-queue-guide-${item.inputId}`}
                    // 恒禁用：没有「已排队项转引导」的实机证据，绝不接 queueNow。
                    disabled
                    title={guideReason}
                    className="shrink-0 rounded-full border border-[var(--cb)] px-2 py-0.5 text-[11px] leading-4 text-[var(--tx)] disabled:opacity-40"
                  >
                    {i18n.t("mobile.zcodeControl.queueGuide")}
                  </button>
                  {renderEditButton(item)}
                  {renderDeleteButton(item)}
                </div>
                {menuFor === item.inputId && (
                  <div
                    role="menu"
                    data-testid={`zcode-queue-menu-${item.inputId}`}
                    aria-label={i18n.t("mobile.zcodeControl.queueMoveMenuAria", {
                      position: positionLabel(item),
                    })}
                    className="flex flex-wrap gap-1 rounded-lg border border-[var(--cb)] px-1 py-1"
                  >
                    {[
                      {
                        key: "first",
                        label: i18n.t("mobile.zcodeControl.queueMoveFirst"),
                        before: firstId,
                        disabled: !canMove || index === 0,
                        testId: `zcode-queue-move-first-${item.inputId}`,
                      },
                      {
                        key: "up",
                        label: i18n.t("mobile.zcodeControl.queueMoveUp"),
                        before: upId,
                        disabled: !canMove || upId === null,
                        testId: `zcode-queue-move-up-${item.inputId}`,
                      },
                      {
                        key: "down",
                        label: i18n.t("mobile.zcodeControl.queueMoveDown"),
                        before: downId,
                        disabled: !canMove || downId === null,
                        testId: `zcode-queue-move-down-${item.inputId}`,
                      },
                    ].map((option) => (
                      <button
                        key={option.key}
                        type="button"
                        role="menuitem"
                        data-testid={option.testId}
                        disabled={option.disabled}
                        onClick={() => move(option.before)}
                        className="rounded-full border border-[var(--cb)] px-2 py-0.5 text-[11px] leading-4 text-[var(--tx)] disabled:opacity-40"
                      >
                        {option.label}
                      </button>
                    ))}
                  </div>
                )}
                {/* 条目级原因（带附件/已消费）：只在该项确实受限时多一行，常见行仍是一行。 */}
                {globalEditReason === null && (editReason !== null || deleteReason !== null) && (
                  <p
                    data-testid={`zcode-queue-item-reason-${item.inputId}`}
                    className="pl-5 text-[10px] leading-3 text-[var(--mut)]"
                  >
                    {editReason ?? deleteReason}
                  </p>
                )}
              </li>
            );
          })}
        </ul>
      )}
    </div>
  );
}
