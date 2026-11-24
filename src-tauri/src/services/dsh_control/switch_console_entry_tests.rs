//! 控制台入口在会话源已按当前选择过滤时的回归。
use super::*;
use crate::remote::{api, pairing, server};
use crate::session::{AgentType, ProcessForm, Session, SessionStatus, SessionsResponse};
use axum::{
    extract::{Query, State},
    http::{header, HeaderMap, StatusCode},
};
use std::collections::HashMap;

struct NoInjector;
impl crate::inject::engine::Injector for NoInjector {
    fn locate_and_inject(&self, _: u32, _: &str) -> Result<(), String> {
        panic!("不得注入终端")
    }
    fn locate_and_send_key_inner(&self, _: u32, _: &str) -> Result<(), String> {
        panic!("不得发送按键")
    }
}

fn selected_session(registry: &instances::InstanceRegistry) -> SessionsResponse {
    let id = registry.selected().unwrap().unwrap().instance_id;
    SessionsResponse {
        sessions: vec![Session {
            id: "shared-session".into(),
            agent_type: AgentType::Dsh,
            project_name: "fixture".into(),
            project_path: "/tmp/fixture".into(),
            title: None,
            git_branch: None,
            github_url: None,
            status: SessionStatus::Idle,
            last_message: None,
            last_message_role: None,
            last_message_subagent_report: false,
            flap_from_subagent_activity: false,
            last_activity_at: "2026-11-11T00:00:00Z".into(),
            pid: HOST_PID,
            cpu_usage: 0.0,
            active_subagent_count: 0,
            form: ProcessForm::App,
            jump_supported: false,
            unread: false,
            unread_turned_green_at_ms: None,
            dsh_instance_id: Some(id),
            dsh_subagent: None,
        }],
        total_count: 1,
        waiting_count: 0,
    }
}

fn state(f: &SelectFixture) -> Arc<server::RemoteState> {
    let store = pairing::DeviceStore::memory();
    let source = Arc::clone(&f.base.registry);
    let fresh = Arc::clone(&f.base.registry);
    Arc::new(server::RemoteState {
        opencode_native: Arc::new(crate::services::opencode_native::NativeHub::new(
            crate::remote::pairing::DeviceStore::memory(),
        )),
        session_source: Box::new(move || selected_session(&source)),
        fresh_session_source: Box::new(move || selected_session(&fresh)),
        pairing_counter: Box::new(Vec::new),
        board_hidden_ids: Box::new(Vec::new),
        board_hidden_hide: Arc::new(|_| panic!("不得隐藏会话")),
        board_hidden_unhide: Arc::new(|_| panic!("不得恢复会话")),
        unread_mark_read: Arc::new(|_, _, _| panic!("不得修改未读状态")),
        session_close: Arc::new(|_| panic!("不得关闭进程")),
        codex_control: Arc::new(
            crate::remote::codex_ipc::CodexControlHub::disabled_with_store(store.clone()),
        ),
        store,
        host_source: Box::new(|| serde_json::json!({})),
        message_source: Box::new(|_, _, _| Err("无内容源".into())),
        path_source: Box::new(|_, _, _| (vec![], false)),
        watcher_tx: tokio::sync::broadcast::channel(8).0,
        sse_registry: Arc::new(server::SseRegistry::default()),
        max_devices_source: Box::new(|| 3),
        pin_limiter: Mutex::new(crate::remote::pin::PinRateLimiter::new()),
        global_pin_limiter: Mutex::new(crate::remote::pin::PinRateLimiter::global()),
        pin_source: Box::new(|| None),
        now_source: Box::new(|| chrono::Utc::now().timestamp_millis()),
        via_hosts_source: Box::new(|| None),
        rate_bucket_channels_source: Box::new(Vec::new),
        injector: Arc::new(NoInjector),
        resume_spawner: Arc::new(|_| panic!("不得启动终端")),
        archive_source: Box::new(Vec::new),
        archive_delete: Arc::new(|_| panic!("不得删除归档")),
        confirm_probe: Arc::new(|_, _, _| panic!("不得读取终端")),
        dialog_probe: Arc::new(|_, _| panic!("不得读取终端")),
        screen_probe: Arc::new(|_, _| panic!("不得读取终端")),
        capability_table: crate::inject::capability::new_table(),
        create_hub: Arc::new(server::CreateTaskHub::stub()),
        home_source: Box::new(|| None),
        ui_config_source: Box::new(|| None),
        subagent_source: Default::default(),
        subagent_message_source: Default::default(),
        dsh_control: crate::services::dsh_control::disabled_arc(),
        dsh_auto_adapt_source: Arc::new(|| false),
        desktop_language: Default::default(),
        dsh_console_mount: Some(Arc::clone(&f.mount)),
        dsh_console_upstream_source: Some(Arc::new(|id: &str| {
            Ok(crate::remote::console_upstream::ConsoleUpstream {
                instance_id: id.into(),
                authority: HOST_AUTHORITY.into(),
                epoch: 1,
            })
        })),
    })
}

#[tokio::test]
// 与本文件其余用例共用全局提交边界串行门（Important-7；tokio Mutex 守卫跨
// await 存活——入口兑换、探针钩子与边界翻转不得交错）。
async fn console_entry_rejects_old_instance_when_session_source_tracks_selection() {
    let _serial = TEST_BOUNDARY_SERIAL.lock().await;
    let _guard = ProbeGuard;
    let f = SelectFixture::new();
    f.close_probe(Arc::new(Mutex::new(Vec::new())));
    let state = state(&f);
    let mut headers = HeaderMap::new();
    let cookie = pairing::device_cookie("00000000000000000000000000000001");
    headers.insert(
        header::COOKIE,
        cookie.split(';').next().unwrap().parse().unwrap(),
    );
    let params = HashMap::from([
        ("session_id".into(), "shared-session".into()),
        ("instanceId".into(), f.ids[0].clone()),
    ]);
    let before = api::dsh_console_entry(
        State(Arc::clone(&state)),
        Query(params.clone()),
        headers.clone(),
    )
    .await;
    assert_ne!(
        before.status(),
        StatusCode::NOT_FOUND,
        "切走前会话可被 A 定位"
    );
    f.select(&f.ids[1]);
    let after = api::dsh_console_entry(State(state), Query(params), headers).await;
    assert_eq!(
        after.status(),
        StatusCode::NOT_FOUND,
        "会话源随选择投影时，旧 A 的入口不得定位到 B 同名会话"
    );
}
