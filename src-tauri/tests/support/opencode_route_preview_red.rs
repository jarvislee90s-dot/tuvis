use super::*;
use serde_json::{json, Value};

const PREVIEW: &str = "/m/api/v1/opencode/route/preview?sessionId=ses-1";
const NATIVE: &str = "/m/api/v1/opencode/route/native";

async fn get_as(f: &Fixture, uri: &str, cookie: &str) -> (axum::http::StatusCode, Value) {
    let response = f
        .app
        .clone()
        .oneshot(req("GET", uri, Some(cookie), None))
        .await
        .unwrap();
    let status = response.status();
    let body = body_of(response).await;
    (
        status,
        serde_json::from_str(&body).unwrap_or(json!({"raw": body})),
    )
}

async fn preview(f: &Fixture, cookie: &str) -> Value {
    let (status, body) = get_as(f, PREVIEW, cookie).await;
    assert_eq!(status, 200, "预览端点必须存在并返回候选：{body}");
    assert_eq!(body["sessionId"], "ses-1");
    assert_eq!(body["candidate"]["sessionId"], "ses-1");
    assert!(body["candidate"]["bindingToken"]
        .as_str()
        .is_some_and(|s| !s.is_empty()));
    assert!(body["previewToken"].as_str().is_some_and(|s| !s.is_empty()));
    assert_ne!(body["previewToken"], body["candidate"]["bindingToken"]);
    assert!(body["expiresAtMs"].as_i64().is_some());
    body
}

async fn established() -> (Fixture, String, i64) {
    let f = Fixture::single(Switch::On).await;
    let (status, view) = f.view("ses-1").await;
    assert_eq!(status, 200, "建立旧路由：{view}");
    let token = view["bindingToken"].as_str().unwrap().to_owned();
    let version = view["routeVersion"].as_i64().unwrap();
    (f, token, version)
}

fn register(f: &Fixture, peer: &Peer, id: &str) {
    std::fs::write(f._temp.path().join("service.json"), format!(
        r#"{{"id":"{id}","version":"{PEER_VERSION}","url":"{}","pid":{PEER_PID},"password":"pw-fixture"}}"#, peer.endpoint
    )).unwrap();
}

fn snapshot(f: &Fixture) -> Vec<Vec<Vec<String>>> {
    f.state.store.with(|c| {
        [
            "opencode_native_routes",
            "opencode_native_actions",
            "opencode_native_claims",
            "inject_queue",
            "opencode_outbox_sessions",
            "opencode_outbox_targets",
            "opencode_outbox",
        ]
        .into_iter()
        .map(|table| {
            let mut stmt = c
                .prepare(&format!("SELECT * FROM {table} ORDER BY rowid"))
                .unwrap();
            let count = stmt.column_count();
            stmt.query_map([], |row| {
                Ok((0..count)
                    .map(|i| format!("{:?}", row.get_ref(i).unwrap()))
                    .collect())
            })
            .unwrap()
            .map(Result::unwrap)
            .collect()
        })
        .collect()
    })
}

fn request(old: &str, version: i64, candidate: &Value) -> String {
    json!({"sessionId":"ses-1", "bindingToken":old, "routeVersion":version,
        "candidateToken":candidate["previewToken"]})
    .to_string()
}

fn assert_stale(status: axum::http::StatusCode, body: &str) {
    assert_eq!(status, 409, "候选或旧路由变化应受控拒绝：{body}");
    assert!(
        body.contains("OC_BINDING_STALE"),
        "必须返回绑定过期族错误：{body}"
    );
}

#[tokio::test]
async fn route_preview_zero_side_effects_preserves_active_outbox() {
    let (f, old, _) = established().await;
    let b = Peer::start(&f.dir).await;
    register(&f, &b, "candidate-b");
    let before = snapshot(&f);
    let writes = (f.peer.writes(), b.writes());
    let candidate = preview(&f, COOKIE).await;
    assert_eq!(candidate["candidate"]["serviceId"], "candidate-b");
    assert_eq!(candidate["candidate"]["directory"], f.dir.to_str().unwrap());
    assert_eq!(candidate["candidate"]["pid"], PEER_PID);
    assert_eq!(candidate["candidate"]["startedAtMs"], PEER_STARTED_AT_MS);
    assert!(b.requests() > 0, "候选服务必须实际被读取");
    assert_eq!((f.peer.writes(), b.writes()), writes);
    assert_eq!(
        snapshot(&f),
        before,
        "预览不得改变路由、账本、队列或 outbox"
    );
    let (status, body) =
        get_bound(&f.app, "/m/api/v1/opencode/outbox?session_id=ses-1", &old).await;
    assert_eq!(status, 200, "预览 B 后 A 的 outbox 仍可读：{body}");
}

#[tokio::test]
async fn route_preview_a_changed_candidate_b_to_c_is_rejected() {
    let (f, old, version) = established().await;
    let b = Peer::start(&f.dir).await;
    let c = Peer::start(&f.dir).await;
    register(&f, &b, "candidate-b");
    let candidate = preview(&f, COOKIE).await;
    register(&f, &c, "candidate-c");
    let before = snapshot(&f);
    let (status, body) = post(&f.app, NATIVE, COOKIE, request(&old, version, &candidate)).await;
    assert_eq!(snapshot(&f), before, "拒绝须保留旧路由及时间戳和账本");
    assert_eq!((f.peer.writes(), b.writes(), c.writes()), (0, 0, 0));
    assert_stale(status, &body);
}

#[tokio::test]
async fn route_preview_b_two_devices_only_one_confirmation_commits() {
    let (f, old, version) = established().await;
    let b = Peer::start(&f.dir).await;
    register(&f, &b, "candidate-b");
    let p1 = preview(&f, COOKIE).await;
    let p2 = preview(&f, COOKIE_OTHER).await;
    assert_ne!(
        p1["previewToken"], p2["previewToken"],
        "各设备必须获得独立确认凭据"
    );
    assert_eq!(
        p1["candidate"]["bindingToken"],
        p2["candidate"]["bindingToken"]
    );
    let (r1, r2) = tokio::join!(
        post(&f.app, NATIVE, COOKIE, request(&old, version, &p1)),
        post(&f.app, NATIVE, COOKIE_OTHER, request(&old, version, &p2))
    );
    let results = [r1, r2];
    assert_eq!(
        results.iter().filter(|r| r.0 == 200).count(),
        1,
        "恰好一个成功：{results:?}"
    );
    let committed: Value =
        serde_json::from_str(&results.iter().find(|r| r.0 == 200).unwrap().1).unwrap();
    assert_eq!(committed["bindingToken"], p1["candidate"]["bindingToken"]);
    let rejected = results.iter().find(|r| r.0 != 200).unwrap();
    assert_stale(rejected.0, &rejected.1);
    assert_eq!(route_version_snapshot(&f).5, version + 1);
    assert_eq!(
        f.state.store.with(|c| c
            .query_row("SELECT COUNT(*) FROM opencode_native_actions", [], |r| r
                .get::<_, i64>(0))
            .unwrap()),
        0,
        "路由选择不得产生重复动作"
    );
    assert_eq!((f.peer.writes(), b.writes()), (0, 0));
}

#[tokio::test]
async fn route_preview_d_success_receipt_matches_committed_binding_and_version() {
    let (f, old, version) = established().await;
    let b = Peer::start(&f.dir).await;
    register(&f, &b, "candidate-b");
    let candidate = preview(&f, COOKIE).await;
    let (status, body) = post(&f.app, NATIVE, COOKIE, request(&old, version, &candidate)).await;
    assert_eq!(status, 200, "确认成功：{body}");
    let receipt: Value = serde_json::from_str(&body).unwrap();
    let token = receipt["bindingToken"]
        .as_str()
        .expect("成功回执必须包含 bindingToken");
    let committed_version = receipt["routeVersion"]
        .as_i64()
        .expect("成功回执必须包含 routeVersion");
    assert_eq!(token, candidate["candidate"]["bindingToken"]);
    let row = route_version_snapshot(&f);
    assert_eq!(row.0, "candidate-b");
    assert_eq!(committed_version, row.5);
    let (status, body) =
        get_bound(&f.app, "/m/api/v1/opencode/outbox?session_id=ses-1", token).await;
    assert_eq!(status, 200, "回执绑定必须能读取落库目标的 outbox：{body}");
}

const CHECK: &str = "/m/api/v1/opencode/route/check?sessionId=ses-1";

#[tokio::test]
async fn route_check_returns_confirmed_active_binding() {
    let (f, _b, _, receipt) = confirm_for_view().await;
    let (status, body) = get_bound(&f.app, CHECK, receipt["bindingToken"].as_str().unwrap()).await;
    assert_eq!(status, 200, "核对已确认路由：{body}");
    assert_eq!(body["sessionId"], "ses-1");
    assert_eq!(body["routeState"], "active");
    assert_eq!(body["bindingToken"], receipt["bindingToken"]);
    assert_eq!(body["routeVersion"], receipt["routeVersion"]);
}

#[tokio::test]
async fn route_check_returns_draining_binding() {
    let (f, token, version) = established().await;
    seed_historical_draining(&f);
    let draining_version = route_version_snapshot(&f).5;
    assert_eq!(draining_version, version + 1);
    let (status, body) = get_bound(&f.app, CHECK, &token).await;
    assert_eq!(status, 200, "核对 draining 路由：{body}");
    assert_eq!(body["sessionId"], "ses-1");
    assert_eq!(body["routeState"], "draining");
    assert_eq!(body["bindingToken"], token);
    assert_eq!(body["routeVersion"].as_i64(), Some(draining_version));
}

#[tokio::test]
async fn route_check_does_not_change_routes_actions_or_service_writes() {
    let (f, b, _, receipt) = confirm_for_view().await;
    f.seed_action("check-snapshot", "ses-1", "acknowledged");
    let before = snapshot(&f);
    let writes = (f.peer.writes(), b.writes());
    let (status, body) = get_bound(&f.app, CHECK, receipt["bindingToken"].as_str().unwrap()).await;
    assert_eq!(status, 200, "核对路由：{body}");
    assert_eq!(
        snapshot(&f),
        before,
        "核对不得改变路由行、动作账本及其他状态"
    );
    assert_eq!((f.peer.writes(), b.writes()), writes);
}

#[tokio::test]
async fn same_service_preview_401_preserves_registered_binding() {
    let (f, token, _) = established().await;
    f.peer
        .shared
        .unauthorized_next
        .store(true, Ordering::SeqCst);
    let (status, body) = get_as(&f, PREVIEW, COOKIE).await;
    assert_ne!(status, 200, "401 的预览不得成功：{body}");
    assert!(
        !f.peer.shared.unauthorized_next.load(Ordering::SeqCst),
        "预览必须实际收到 401"
    );
    let (status, body) =
        get_bound(&f.app, "/m/api/v1/opencode/outbox?session_id=ses-1", &token).await;
    assert_eq!(status, 200, "预览 401 后原绑定的 outbox 仍可读：{body}");
}

async fn confirm_for_view() -> (Fixture, Peer, String, Value) {
    let (f, old, version) = established().await;
    let b = Peer::start(&f.dir).await;
    register(&f, &b, "candidate-b");
    let candidate = preview(&f, COOKIE).await;
    let (status, body) = post(&f.app, NATIVE, COOKIE, request(&old, version, &candidate)).await;
    assert_eq!(status, 200, "确认必须提交：{body}");
    let receipt: Value = serde_json::from_str(&body).unwrap();
    assert!(receipt["bindingToken"].as_str().is_some());
    assert_eq!(
        receipt["bindingToken"],
        candidate["candidate"]["bindingToken"]
    );
    assert!(receipt["routeVersion"].as_i64().is_some());
    (f, b, old, receipt)
}

#[tokio::test]
async fn route_preview_e_confirmed_binding_view_recovers_committed_route() {
    let (f, _b, _old, receipt) = confirm_for_view().await;
    let token = receipt["bindingToken"].as_str().unwrap();
    let (status, view) = get_bound(&f.app, "/m/api/v1/opencode/view?session_id=ses-1", token).await;
    assert_eq!(status, 200, "已提交绑定可读视图：{view}");
    assert_eq!(view["bindingToken"], receipt["bindingToken"]);
    assert_eq!(view["routeVersion"], receipt["routeVersion"]);
    assert_eq!(view["routeVersion"], route_version_snapshot(&f).5);
}

#[tokio::test]
async fn route_preview_e_old_binding_view_is_stale_after_confirmation() {
    let (f, _b, old, _receipt) = confirm_for_view().await;
    let before = snapshot(&f);
    let (status, body) = get_bound(&f.app, "/m/api/v1/opencode/view?session_id=ses-1", &old).await;
    assert_eq!(snapshot(&f), before, "旧绑定读回不得改变已提交路由");
    assert_stale(status, &body.to_string());
}

#[tokio::test]
async fn route_preview_f_expired_candidate_is_rejected_without_mutation() {
    let (f, old, version) = established().await;
    f.state
        .opencode_native
        .set_candidate_preview_ttl_ms_for_tests(1500);
    let b = Peer::start(&f.dir).await;
    register(&f, &b, "candidate-b");
    let candidate = preview(&f, COOKIE).await;
    let expiration = candidate["expiresAtMs"].as_i64().unwrap();
    let wait = (expiration - chrono::Utc::now().timestamp_millis() + 10).max(0) as u64;
    tokio::time::sleep(std::time::Duration::from_millis(wait)).await;
    let before = snapshot(&f);
    let (status, body) = post(&f.app, NATIVE, COOKIE, request(&old, version, &candidate)).await;
    assert_eq!(snapshot(&f), before);
    assert_eq!((f.peer.writes(), b.writes()), (0, 0));
    assert_stale(status, &body);
}
