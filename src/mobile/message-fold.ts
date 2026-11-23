// 消息折叠共享纯函数（2026-09-20 归档详情对齐批从 SessionDetail 迁出）：
// 活会话与归档详情共用同一套折叠摘要文案与可折叠判定，防两处漂移。
// 只放组件外纯函数——折叠状态（expandedOverride Map）仍归各页面自持。
// M5 文案双语化（Task 7）：折叠摘要经 mobile.fold.* 键渲染期现取（中英随语言切换）。
import i18n from "@/i18n";
import type { SessionMessage } from "./api";
import type { PublicNativeView } from "./opencodeNativeApi";

/** 原生正文行和详情页的一键入口共用同一折叠状态。 */
export interface NativeProcessFold {
  isCollapsed: (message: SessionMessage) => boolean;
  onToggle: (message: SessionMessage) => void;
}

/** 过程消息 kind：恒为可折叠对象（活会话运行中折叠语义的 wire 子集）
 *
 *  2026-11-22 任务 A：`system-note`（OpenCode synthetic 系统提醒，如切 agent 的
 *  Plan mode 提示）同样以默认折叠的过程消息呈现——它属会话可见历史，
 *  但不是用户消息，也不该占据助手的正式正文气泡。 */
export function isProcessKind(kind: string): boolean {
  return (
    kind === "thinking" || kind === "tool-call" || kind === "tool-result" || kind === "system-note"
  );
}

/** seq 是快照内的下标。用原生记录/调用 id 保留跨快照的折叠覆盖。 */
export function nativeProcessKeys(view: PublicNativeView | null): Map<number, string> {
  const keys = new Map<number, string>();
  if (!view) return keys;
  const occurrences = new Map<string, number>();
  for (const [index, message] of view.messages.entries()) {
    if (!isProcessKind(message.kind)) continue;
    const recordId = view.messageIds?.[message.seq];
    const previous = view.messages[index - 1];
    // 投影把工具结果紧跟在它的调用之后，但 toolCallIds 只记录调用行。
    const matchingCall =
      message.kind === "tool-result" &&
      previous?.kind === "tool-call" &&
      recordId === view.messageIds?.[previous.seq]
        ? view.toolCallIds?.[previous.seq]
        : undefined;
    const callId = view.toolCallIds?.[message.seq] ?? matchingCall;
    if (!recordId) {
      keys.set(message.seq, JSON.stringify([view.revision, message.seq]));
      continue;
    }
    const group = JSON.stringify([recordId, message.kind, callId]);
    const occurrence = occurrences.get(group) ?? 0;
    occurrences.set(group, occurrence + 1);
    keys.set(message.seq, `${group}:${occurrence}`);
  }
  return keys;
}

/** 折叠行的摘要标签 */
export function collapsedLabel(m: SessionMessage): string {
  switch (m.kind) {
    case "thinking":
      return i18n.t("mobile.fold.thinking");
    case "tool-call":
      return m.toolName
        ? i18n.t("mobile.fold.toolCall", { tool: m.toolName })
        : i18n.t("mobile.fold.toolCallPlain");
    case "tool-result":
      return i18n.t("mobile.fold.toolResult");
    case "system-note":
      return i18n.t("mobile.fold.systemNote");
    case "assistant":
      return i18n.t("mobile.fold.earlierReplies");
    case "plan":
      // 防御位：plan 一等卡片不可折叠（isToggleable/isCollapsed 恒展开），正常不渲染此头
      // （2026-09-23 main 侧批次丁/戊计划卡合入时移植，保持与活会话页同源）
      return i18n.t("mobile.fold.plan");
    default:
      return i18n.t("mobile.fold.collapsed");
  }
}
