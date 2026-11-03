// 四页工作区的跨窗导航入口（2026-10-26 计划 Task 7）。
//
// 命令名与事件名只此一处（公共契约逐字），前端任何模块都不得自带第二份字面量：
//  * `workspace_request_navigation(tab)` → `WorkspaceNavigation`（Rust 侧先存后 emit，
//    并唤醒既有主窗口；绝不创建新窗口）；
//  * `workspace_peek_navigation()` → `WorkspaceNavigation | null`；
//  * `workspace_ack_navigation(revision)` → `boolean`；
//  * 事件 `tuvis-workspace-navigation` 载荷 = `{ revision, tab }`（camelCase）。
import { invoke } from "@tauri-apps/api/core";
import type { WorkspaceNavigation, WorkspaceTab } from "@/types/workspace";

/** 导航事件名（Rust `commands/workspace.rs` 的常量逐字对齐） */
export const WORKSPACE_NAVIGATION_EVENT = "tuvis-workspace-navigation";

/** 三个导航 IPC 命令名（公共契约逐字；消费方一律 import，不写第二份字面量） */
export const WORKSPACE_NAVIGATION_COMMANDS = {
  request: "workspace_request_navigation",
  peek: "workspace_peek_navigation",
  ack: "workspace_ack_navigation",
} as const;

/**
 * 请求主窗口切到指定页签。从**任何窗口**调用都成立：
 *  * 在主窗口调用 = 本窗页签点击（同样经 revision 排序，与跨窗目标一视同仁）；
 *  * 在其他窗口调用 = Rust 侧唤醒（show/unminimize/set_focus）主窗口并广播事件。
 * 事件尚未被主窗消费也不丢——目标在 Rust 队列里，主窗 hook 的 peek 路径会恢复。
 */
export async function openWorkspaceTab(tab: WorkspaceTab): Promise<void> {
  await invoke<WorkspaceNavigation>(WORKSPACE_NAVIGATION_COMMANDS.request, { tab });
}

/** 查看待消费的导航目标（不消费）；null = 队列空 */
export function peekWorkspaceNavigation(): Promise<WorkspaceNavigation | null> {
  return invoke<WorkspaceNavigation | null>(WORKSPACE_NAVIGATION_COMMANDS.peek);
}

/** 确认消费；false = 目标已被更新的请求替换（过期 ACK，无害） */
export function ackWorkspaceNavigation(revision: number): Promise<boolean> {
  return invoke<boolean>(WORKSPACE_NAVIGATION_COMMANDS.ack, { revision });
}
