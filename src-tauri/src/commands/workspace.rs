//! 工作区导航 IPC（2026-10-26 计划 Task 7；公共契约「导航」条逐字）。
//!
//! * `workspace_request_navigation(tab)` → `WorkspaceNavigation`：**先存后 emit**——
//!   目标先入队（绝不因窗口缺失/事件失败而丢），再向应用广播
//!   `tuvis-workspace-navigation`，最后唤醒既有主窗口（show → unminimize → set_focus）。
//!   主窗口找不到时报明确错误，但**目标保留在队列**（下一次 peek 恢复）；**绝不建窗**。
//! * `workspace_peek_navigation()` → `Option<WorkspaceNavigation>`：主窗 hook 订阅
//!   就绪 / 重新可见时恢复监听前到达的目标。
//! * `workspace_ack_navigation(revision)` → `bool`：仅最新目标可清队（过期 ACK false）。
//!
//! 主窗关闭 = hide（`system_tray.rs` 的 CloseRequested 拦截），故这里只唤醒、永不重建。

use crate::services::workspace_navigation::{NavigationQueue, WorkspaceNavigation, WorkspaceTab};
use tauri::{Emitter, Manager};

/// 事件名（公共契约逐字）。前端 `src/lib/workspaceNavigation.ts` 同名常量对齐。
pub const WORKSPACE_NAVIGATION_EVENT: &str = "tuvis-workspace-navigation";

#[tauri::command]
pub fn workspace_request_navigation(
    app: tauri::AppHandle,
    queue: tauri::State<'_, NavigationQueue>,
    tab: WorkspaceTab,
) -> Result<WorkspaceNavigation, String> {
    // ① 先存：目标是「不能丢」的那一半，emit/唤醒都只是尽力送达
    let nav = queue.request(tab);
    // ② 后 emit：载荷 = 同一个 WorkspaceNavigation（camelCase serde 已带）。失败仅告警——
    //    目标已在队里，主窗 hook 的 peek 路径必然恢复它
    if let Err(err) = app.emit(WORKSPACE_NAVIGATION_EVENT, &nav) {
        log::warn!("导航事件广播失败（目标已入队，待 peek 恢复）: {err}");
    }
    // ③ 唤醒既有主窗口（关窗即 hide，不是退出）：show → unminimize → set_focus
    let Some(main) = app.get_webview_window("main") else {
        return Err(format!(
            "WORKSPACE_MAIN_WINDOW_NOT_FOUND: 主窗口不存在，导航目标（revision={}, tab={tab:?}）已保留在队列，主窗口下一次 peek 将恢复",
            nav.revision
        ));
    };
    if let Err(err) = main.show() {
        log::warn!("主窗口 show 失败: {err}");
    }
    if let Err(err) = main.unminimize() {
        log::warn!("主窗口 unminimize 失败: {err}");
    }
    if let Err(err) = main.set_focus() {
        log::warn!("主窗口 set_focus 失败: {err}");
    }
    Ok(nav)
}

#[tauri::command]
pub fn workspace_peek_navigation(
    queue: tauri::State<'_, NavigationQueue>,
) -> Option<WorkspaceNavigation> {
    queue.peek()
}

#[tauri::command]
pub fn workspace_ack_navigation(queue: tauri::State<'_, NavigationQueue>, revision: u64) -> bool {
    queue.ack(revision)
}
