use super::*;
use http_body_util::BodyExt;

#[derive(Clone, Copy, Debug)]
enum ReadEndpoint {
    View,
    Form,
    Agents,
    Events,
}

async fn read_pinned(
    f: &Fixture,
    token: &str,
    endpoint: ReadEndpoint,
) -> axum::http::Response<axum::body::Body> {
    let path = match endpoint {
        ReadEndpoint::View => "/m/api/v1/opencode/view?session_id=ses-1",
        ReadEndpoint::Form => "/m/api/v1/opencode/forms/frm-1?session_id=ses-1",
        ReadEndpoint::Agents => "/m/api/v1/opencode/agents?session_id=ses-1",
        ReadEndpoint::Events => return pinned_events(f, token).await,
    };
    let (mut parts, body) = req("GET", path, Some(COOKIE), None).into_parts();
    parts
        .headers
        .insert("x-opencode-expected-binding", token.parse().unwrap());
    f.app
        .clone()
        .oneshot(axum::http::Request::from_parts(parts, body))
        .await
        .unwrap()
}

async fn empty_scan_keeps_a(endpoint: ReadEndpoint) {
    let (f, token) = readonly_unproven_fixture().await;
    *f.scan_override.lock().unwrap() = Some(vec![]);
    assert_eq!(f.route_state("ses-1"), None, "A 从未认领 active 路由");
    let response = read_pinned(&f, &token, endpoint).await;
    let status = response.status();
    if matches!(endpoint, ReadEndpoint::Events) && status == 200 {
        wait_for_event_connections(&f.peer, 1).await;
        drop(response);
    } else {
        let body = body_of(response).await;
        assert_eq!(
            status, 200,
            "扫描为空时原 token 的 {endpoint:?} 仍应只读 A：{body}"
        );
        let json: serde_json::Value = serde_json::from_str(&body).unwrap();
        match endpoint {
            ReadEndpoint::View => {
                assert_eq!(json["bindingToken"], token);
                assert_eq!(json["serviceId"], PEER_SERVICE_ID);
                assert_ne!(json["route"], "opencode_native");
            }
            ReadEndpoint::Form => assert_eq!(json["answers"]["q0"]["value"], "blue"),
            ReadEndpoint::Agents => assert_eq!(json["current"], "agent-a"),
            ReadEndpoint::Events => unreachable!(),
        }
    }
    assert_eq!(f.route_state("ses-1"), None);
    assert_eq!(f.peer.writes(), 0);
}

macro_rules! empty_scan_test {
    ($name:ident, $endpoint:ident) => {
        #[tokio::test]
        async fn $name() {
            empty_scan_keeps_a(ReadEndpoint::$endpoint).await;
        }
    };
}
empty_scan_test!(empty_scan_original_binding_keeps_view, View);
empty_scan_test!(empty_scan_original_binding_keeps_form, Form);
empty_scan_test!(empty_scan_original_binding_keeps_agents, Agents);
empty_scan_test!(empty_scan_original_binding_keeps_events, Events);

async fn replacement_scan_cannot_expose_b(endpoint: ReadEndpoint) {
    let (f, token) = readonly_unproven_fixture().await;
    let b_dir = f._temp.path().join("different-project-b");
    std::fs::create_dir_all(&b_dir).unwrap();
    let b_dir = b_dir.canonicalize().unwrap();
    let b = Peer::start(&b_dir).await;
    b.set_info_pid(9999);
    let positive = reqwest::Client::builder()
        .no_proxy()
        .build()
        .unwrap()
        .get(format!("{}/api/agent", b.endpoint))
        .send()
        .await
        .unwrap();
    assert_eq!(positive.status(), 200, "B 假服务必须实际可达");
    assert!(
        positive.text().await.unwrap().contains("agent-b-secret"),
        "B 必须返回可区分的数据"
    );
    let mut b_card = opencode_session("ses-1", &b_dir);
    b_card.pid = 9999;
    *f.scan_override.lock().unwrap() = Some(vec![b_card]);
    std::fs::write(f._temp.path().join("service.json"), format!(
        r#"{{"id":"svc-b","version":"{PEER_VERSION}","url":"{}","pid":9999,"password":"pw-fixture"}}"#, b.endpoint
    )).unwrap();
    assert_eq!(f.route_state("ses-1"), None);
    let response = read_pinned(&f, &token, endpoint).await;
    let status = response.status();
    if status == 200 && matches!(endpoint, ReadEndpoint::Events) {
        wait_for_event_connections(&f.peer, 1).await;
        assert_eq!(
            b.shared.event_connections.load(Ordering::SeqCst),
            0,
            "不得订阅 B"
        );
        drop(response);
    } else {
        let body = body_of(response).await;
        assert!(
            !body.contains("agent-b-secret")
                && !body.contains("form-b-secret")
                && !body.contains("svc-b"),
            "A 的 token 不得泄露 B：{body}"
        );
        if status == 200 {
            let json: serde_json::Value = serde_json::from_str(&body).unwrap();
            match endpoint {
                ReadEndpoint::View => {
                    assert_eq!(json["bindingToken"], token);
                    assert_eq!(json["serviceId"], PEER_SERVICE_ID);
                }
                ReadEndpoint::Form => assert_eq!(json["answers"]["q0"]["value"], "blue"),
                ReadEndpoint::Agents => assert_eq!(json["current"], "agent-a"),
                ReadEndpoint::Events => unreachable!(),
            }
        } else {
            assert!(
                status == 404 || status == 409,
                "替代目标须受控拒绝，实际 {status}：{body}"
            );
        }
    }
    assert_eq!(f.route_state("ses-1"), None);
    assert_eq!(b.shared.event_connections.load(Ordering::SeqCst), 0);
    assert_eq!(f.peer.writes(), 0);
    assert_eq!(b.writes(), 0);
}

macro_rules! replacement_scan_test {
    ($name:ident, $endpoint:ident) => {
        #[tokio::test]
        async fn $name() {
            replacement_scan_cannot_expose_b(ReadEndpoint::$endpoint).await;
        }
    };
}
replacement_scan_test!(replacement_scan_original_binding_never_reads_b_view, View);
replacement_scan_test!(replacement_scan_original_binding_never_reads_b_form, Form);
replacement_scan_test!(
    replacement_scan_original_binding_never_reads_b_agents,
    Agents
);
replacement_scan_test!(
    replacement_scan_original_binding_never_subscribes_b_events,
    Events
);

async fn readonly_upstream_event_notifies_or_closes(replace_registration: bool) {
    let (f, token) = readonly_unproven_fixture().await;
    let response = pinned_events(&f, &token).await;
    assert_eq!(response.status(), 200);
    let mut stream = response.into_body();
    let first = tokio::time::timeout(std::time::Duration::from_secs(10), stream.frame())
        .await
        .expect("首次 revision 必须到达")
        .expect("流仍打开")
        .expect("流帧");
    let initial = String::from_utf8(first.into_data().unwrap().to_vec()).unwrap();
    assert!(initial.contains("event: revision"));
    wait_for_event_connections(&f.peer, 1).await;
    assert_eq!(f.route_state("ses-1"), None);

    let b = Peer::start(&f.dir).await;
    b.set_info_pid(9999);
    if replace_registration {
        // 只替换登记；A 的进程事实、服务、已选会话及已连接的 pump 都不变。
        std::fs::write(f._temp.path().join("service.json"), format!(
            r#"{{"id":"svc-b","version":"{PEER_VERSION}","url":"{}","pid":9999,"password":"pw-fixture"}}"#, b.endpoint
        )).unwrap();
    }
    f.peer.shared.prompts.lock().unwrap().push(serde_json::json!({
        "type":"user", "id":"upstream-event-message", "text":"upstream-event-marker", "time":{"created":1}
    }));
    let sent = f.peer.shared.event_sent.notified();
    f.peer.shared.emit_event.notify_one();
    tokio::time::timeout(std::time::Duration::from_secs(10), sent)
        .await
        .expect("A 必须实际发送上游事件");
    // 不调用 view 或其他手动 GET；只等待 pump 传播真实上游事件或关闭下游。
    let next = tokio::time::timeout(std::time::Duration::from_secs(4), stream.frame()).await;
    // 观察窗口结束后才做正向核验，不能用手动刷新制造上面的通知。
    let (status, view) =
        get_with_binding(&f, "/m/api/v1/opencode/view?session_id=ses-1", &token).await;
    assert_eq!(status, 200, "A 必须仍可手刷：{view}");
    assert_eq!(view["bindingToken"], token);
    assert_eq!(view["serviceId"], PEER_SERVICE_ID);
    assert!(
        view.to_string().contains("upstream-event-marker"),
        "A 必须实际返回更新后的数据：{view}"
    );
    assert_eq!(b.requests(), 0, "不得向 B 发业务请求");
    assert_eq!(b.shared.event_connections.load(Ordering::SeqCst), 0);
    assert_eq!(b.writes(), 0);
    assert_eq!(f.peer.writes(), 0);
    let next = next.expect("A 上游已发事件，但下游既未通知也未断流");
    if !replace_registration {
        assert!(next.is_some(), "正向对照必须收到变更通知");
    }
    if let Some(frame) = next {
        let data = frame
            .expect("事件流不能出错")
            .into_data()
            .expect("通知必须是数据帧");
        let notification = String::from_utf8(data.to_vec()).unwrap();
        assert!(
            notification.contains("event: revision"),
            "保活不能冒充有效通知：{notification}"
        );
        assert_ne!(notification, initial, "修订号必须变化");
    }
}

#[tokio::test]
async fn readonly_a_upstream_event_notifies_without_registration_change() {
    readonly_upstream_event_notifies_or_closes(false).await;
}

#[tokio::test]
async fn readonly_a_upstream_event_after_b_registration_notifies_or_closes() {
    readonly_upstream_event_notifies_or_closes(true).await;
}

#[tokio::test]
async fn readonly_manual_refresh_without_upstream_event_notifies_or_closes_stream() {
    let (f, token) = readonly_unproven_fixture().await;
    let response = pinned_events(&f, &token).await;
    assert_eq!(response.status(), 200);
    let mut stream = response.into_body();
    let first = tokio::time::timeout(std::time::Duration::from_secs(10), stream.frame())
        .await
        .expect("首次 revision 必须到达")
        .expect("流仍打开")
        .expect("流帧");
    let initial = String::from_utf8(first.into_data().expect("数据帧").to_vec()).unwrap();
    assert!(initial.contains("event: revision"));
    wait_for_event_connections(&f.peer, 1).await;

    // 上游事件连接保持静默，只改变 HTTP 回读；手动刷新必须实际观察到新消息。
    f.peer.shared.prompts.lock().unwrap().push(serde_json::json!({
        "type":"user", "id":"manual-refresh-message", "text":"manual-refresh-marker", "time":{"created":1}
    }));
    let (status, view) =
        get_with_binding(&f, "/m/api/v1/opencode/view?session_id=ses-1", &token).await;
    assert_eq!(status, 200, "A 可以手动刷新：{view}");
    assert!(
        view.to_string().contains("manual-refresh-marker"),
        "刷新必须真的看到变更：{view}"
    );

    let next = tokio::time::timeout(std::time::Duration::from_secs(4), stream.frame())
        .await
        .expect("上游无通知时，刷新后的下游必须通知有效 revision 或主动断流");
    if let Some(frame) = next {
        let data = frame
            .expect("事件流不能出错")
            .into_data()
            .expect("通知必须是数据帧");
        let notification = String::from_utf8(data.to_vec()).unwrap();
        assert!(
            notification.contains("event: revision"),
            "保活不能冒充有效通知"
        );
        assert_ne!(notification, initial, "修订号必须变化，不能重发首次通知");
    }
    assert_eq!(f.peer.writes(), 0);
}

#[tokio::test]
async fn readonly_action_verify_discovering_b_preserves_selected_a() {
    let (f, token) = readonly_unproven_fixture().await;
    // 旧动作由 A 创建；先初始化账本，再插入未决行，避免惰性恢复干扰断言。
    let _ = f.state.opencode_native.verify("dev-1", "absent").await;
    f.seed_action("a_pending_verify", "ses-1", "unconfirmed");
    let before = f.action_row("a_pending_verify");

    let b = Peer::start(&f.dir).await;
    b.set_info_pid(9999);
    std::fs::write(f._temp.path().join("service.json"), format!(
        r#"{{"id":"svc-b","version":"{PEER_VERSION}","url":"{}","pid":9999,"password":"pw-fixture"}}"#, b.endpoint
    )).unwrap();

    let (status, body) = post(
        &f.app,
        "/m/api/v1/opencode/actions/a_pending_verify/verify",
        COOKIE,
        "{}".into(),
    )
    .await;
    assert_eq!(status, 200, "未决动作只读核对应返回原状态：{body}");
    assert_eq!(
        f.action_row("a_pending_verify"),
        before,
        "B 不能证明 A 的动作"
    );

    // 恢复 A 的登记前检查 A 仍在缓存中；若误安装 B 或摘除 A，原令牌会被拒绝。
    let (status, view) =
        get_with_binding(&f, "/m/api/v1/opencode/view?session_id=ses-1", &token).await;
    assert_eq!(
        status,
        200,
        "核对发现 B 不能摘除 A 的原绑定（B 请求数 {}）：{view}",
        b.requests()
    );
    assert_eq!(view["bindingToken"], token);
    assert_eq!(view["serviceId"], PEER_SERVICE_ID);
    assert_eq!(f.route_state("ses-1"), None);
    assert_eq!(f.peer.writes(), 0);
    assert_eq!(b.writes(), 0);
}
