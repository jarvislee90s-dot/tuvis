//! 主窗工作区导航目标队列（2026-10-26 计划 Task 7）。
//!
//! 四页同窗后，宠物/托盘/迷你用量条等**其他窗口**的「打开用量详情」不再创建独立
//! `usage-dashboard` 窗口，而是请求主窗口切到对应页签。跨窗口请求可能在主窗口
//! 监听就绪前（启动早期）或显示期（主窗隐藏在托盘）到达——本模块把目标**暂存**
//! 在带 revision 的队列里：只有最新目标有效（旧目标被替换），前端按 revision
//! 单调应用并 ACK，过期 ACK 不清队。
//!
//! 纯内存状态（不落盘）：导航是「这一次运行」的瞬时意图，重启即清零合理。
//! 事件名与 IPC 命令名逐字来自公共契约：`tuvis-workspace-navigation` /
//! `workspace_request_navigation` / `workspace_peek_navigation` / `workspace_ack_navigation`。

use serde::{Deserialize, Serialize};
use std::sync::Mutex;

/// 四个一级页面（公共契约逐字：kebab-case 线格式）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum WorkspaceTab {
    Messages,
    Resources,
    Usage,
    Speed,
}

/// 一次导航目标：revision 单调递增，前端只应用比已应用更新（更大 revision）的目标。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceNavigation {
    pub revision: u64,
    pub tab: WorkspaceTab,
}

/// 待导航目标队列（managed state）。`request` 换新 revision 并**替换**旧目标——
/// 旧目标没有存在价值（用户最终只关心最后一跳）；`peek` 只看不消费，`ack` 只在
/// revision 与 pending 一致时清队（乱序/过期 ACK 返回 false 且保留新目标）。
#[derive(Default)]
pub struct NavigationQueue {
    inner: Mutex<QueueState>,
}

#[derive(Default)]
struct QueueState {
    next_revision: u64,
    pending: Option<WorkspaceNavigation>,
}

impl NavigationQueue {
    /// 登记一个新目标：分配下一个 revision，替换任何 pending 中的旧目标。
    pub fn request(&self, tab: WorkspaceTab) -> WorkspaceNavigation {
        let mut state = self.lock();
        state.next_revision += 1;
        let nav = WorkspaceNavigation {
            revision: state.next_revision,
            tab,
        };
        state.pending = Some(nav);
        nav
    }

    /// 查看当前 pending 目标（不消费）。主窗口 hook 订阅就绪 / 重新可见时用它恢复。
    pub fn peek(&self) -> Option<WorkspaceNavigation> {
        self.lock().pending
    }

    /// 确认消费：仅当 `revision` 正是当前 pending 时清队并返回 true；
    /// 过期 ACK（新目标已替换旧目标）返回 false 且**保留**新目标。
    pub fn ack(&self, revision: u64) -> bool {
        let mut state = self.lock();
        match state.pending {
            Some(pending) if pending.revision == revision => {
                state.pending = None;
                true
            }
            _ => false,
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, QueueState> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}
