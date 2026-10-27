//! 工作区导航队列单元测试（2026-10-26 计划 Task 7；公共契约「导航」条逐字）。
//!
//! 覆盖（brief Step1 `navigation_before_ready_and_stale_ack`）：
//! * 监听就绪前 request ⇒ peek 可取（跨窗目标不能只依赖一次广播）；
//! * request1 后 request2 ⇒ 过期 ACK 不能清掉新目标（只剩最新有效）；
//! * 最新目标的 ACK 才清队；
//! * revision 严格单调递增；
//! * serde 线格式逐字 = kebab-case tab / camelCase `WorkspaceNavigation`。
//!
//! 纪律：纯内存状态机，不需要 Tauri runtime、不碰真机 `~/.tuvis`。

use multi_agents_manager_lib::services::workspace_navigation::{
    NavigationQueue, WorkspaceNavigation, WorkspaceTab,
};

/// 监听就绪前的请求 + 过期 ACK（brief Step1 用例名逐字）。
#[test]
fn navigation_before_ready_and_stale_ack() {
    let queue = NavigationQueue::default();

    // —— 监听就绪前到达的目标：暂存，peek 可取（不能只依赖一次广播）——
    let first = queue.request(WorkspaceTab::Usage);
    assert_eq!(
        queue.peek(),
        Some(WorkspaceNavigation {
            revision: first.revision,
            tab: WorkspaceTab::Usage
        })
    );

    // —— 第二个目标已经在队列：过期 ACK 不清队、不回退 ——
    let second = queue.request(WorkspaceTab::Speed);
    assert!(!queue.ack(first.revision)); // 第二个目标已经在队列
    assert_eq!(queue.peek().unwrap().revision, second.revision);

    // —— 最新目标的 ACK 才清队 ——
    assert!(queue.ack(second.revision));
    assert_eq!(queue.peek(), None);

    // —— 清队后的 ACK 同样无效（幂等假）——
    assert!(!queue.ack(second.revision));
}

/// revision 严格单调递增（跨多次 request，不因 ack/peek 重置）。
#[test]
fn revisions_strictly_increase() {
    let queue = NavigationQueue::default();
    let mut last = 0u64;
    for tab in [
        WorkspaceTab::Messages,
        WorkspaceTab::Resources,
        WorkspaceTab::Usage,
        WorkspaceTab::Speed,
        WorkspaceTab::Usage,
    ] {
        let nav = queue.request(tab);
        assert!(nav.revision > last, "revision 必须严格递增");
        assert_eq!(nav.tab, tab);
        last = nav.revision;
        // peek 不消费：同一目标反复可取
        assert_eq!(queue.peek(), Some(nav));
    }
    // ack 任意一个历史 revision 都不清队（只有最新 revision 命中）
    assert!(!queue.ack(1));
    assert_eq!(queue.peek().unwrap().revision, last);
    assert!(queue.ack(last));
    assert_eq!(queue.peek(), None);
}

/// serde 线格式逐字（契约：tab kebab-case、结构 camelCase）。
#[test]
fn serde_wire_forms_are_exact() {
    assert_eq!(
        serde_json::to_string(&WorkspaceTab::Messages).unwrap(),
        r#""messages""#
    );
    assert_eq!(
        serde_json::to_string(&WorkspaceTab::Resources).unwrap(),
        r#""resources""#
    );
    assert_eq!(
        serde_json::to_string(&WorkspaceTab::Usage).unwrap(),
        r#""usage""#
    );
    assert_eq!(
        serde_json::to_string(&WorkspaceTab::Speed).unwrap(),
        r#""speed""#
    );

    let nav = WorkspaceNavigation {
        revision: 3,
        tab: WorkspaceTab::Usage,
    };
    assert_eq!(
        serde_json::to_string(&nav).unwrap(),
        r#"{"revision":3,"tab":"usage"}"#
    );
    // 反序列化对称（IPC peek/ack 回程的载荷形状）
    let back: WorkspaceNavigation =
        serde_json::from_str(r#"{"revision":3,"tab":"usage"}"#).unwrap();
    assert_eq!(back, nav);
}
