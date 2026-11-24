//! 通过 hub 入口和真实 HTTP 路由验证发送取消、准入代际和同 ID 并发重放。

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc as blocking_channel, Arc, Mutex};
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use futures::future::BoxFuture;
use http_body_util::BodyExt;
use multi_agents_manager_lib::database::{migration, schema};
use multi_agents_manager_lib::remote::pairing::{persist_device, DeviceStore, NewDevice};
use multi_agents_manager_lib::remote::server::{router, RemoteState};
use multi_agents_manager_lib::services::dsh_control::actions::{
    HostProbe, NativeDispatch, NativeSendOutcome,
};
use multi_agents_manager_lib::services::dsh_control::contract::{
    ContractReadReport, CONTRACT_RULE_VERSION,
};
use multi_agents_manager_lib::services::dsh_control::error::{codes, DshError};
use multi_agents_manager_lib::services::dsh_control::model::{
    ActionReceipt, ActionState, ContractEvidence, DeviceContext, LocalHost, Operation,
    OperationContractMatch, SendPayload, Target,
};
use multi_agents_manager_lib::services::dsh_control::production;
use multi_agents_manager_lib::services::dsh_control::store::DshActionStore;
use multi_agents_manager_lib::services::dsh_control::{DshControlHub, DshDb, SessionResolver};
use multi_agents_manager_lib::session::model::{AgentType, ProcessForm, Session, SessionStatus};
use multi_agents_manager_lib::session::SessionsResponse;
use rusqlite::Connection;
use serde_json::{json, Value};
use tokio::sync::{mpsc, Notify};
use tokio::time::{sleep, timeout};
use tower::ServiceExt;

const WAIT: Duration = Duration::from_secs(5);
const HOST: &str = "isolated-http-host";
const REOPENED_HOST: &str = "isolated-http-reopened-host";
const SESSION: &str = "session-aaaaaaaa";
const FIRST: &str = "first-http-action";
const NEXT: &str = "next-http-action";

struct TestNative {
    calls: mpsc::UnboundedSender<String>,
    release_first: Option<Arc<Notify>>,
}

impl NativeDispatch for TestNative {
    fn send_message<'a>(
        &'a self,
        _: &'a Target,
        _: &'a SendPayload,
        action_id: &'a str,
    ) -> BoxFuture<'a, Result<NativeSendOutcome, DshError>> {
        Box::pin(async move {
            self.calls
                .send(action_id.to_owned())
                .expect("native call receiver");
            if let Some(release) = self.release_first.as_ref().filter(|_| action_id == FIRST) {
                release.notified().await;
                // 确定拒绝落终态后，队列无需等额外效果证据即可推进。
                return Ok(NativeSendOutcome::Rejected {
                    code: "fixture_rejected".into(),
                });
            }
            Ok(NativeSendOutcome::Accepted {
                native_id: Some(action_id.into()),
            })
        })
    }
}

fn device() -> DeviceContext {
    DeviceContext {
        device_id: "isolated-http-device".into(),
        device_name: "Isolated HTTP device".into(),
    }
}

fn resolve(host: &str, session: &str) -> Target {
    assert_eq!(session, SESSION);
    Target {
        host_id: host.into(),
        session_id: session.into(),
        epoch: 1,
    }
}

fn fixture(
    instance_id: &'static str,
    native: TestNative,
    before_probe: impl Fn() -> Result<(), DshError> + Send + Sync + 'static,
    resolver: SessionResolver,
) -> (
    Arc<DshControlHub>,
    Arc<Mutex<Connection>>,
    tempfile::TempDir,
) {
    let root = tempfile::tempdir().unwrap();
    let contents = root.path().join("Dsh.app/Contents");
    std::fs::create_dir_all(contents.join("MacOS")).unwrap();
    std::fs::create_dir_all(contents.join("Resources")).unwrap();
    let executable = contents.join("MacOS/dsh");
    std::fs::write(&executable, b"isolated executable").unwrap();
    std::fs::write(contents.join("Resources/app.asar"), b"isolated package").unwrap();
    let profile_dir = root.path().join("profiles/desktop");
    std::fs::create_dir_all(&profile_dir).unwrap();
    let probe: HostProbe = Arc::new(move || {
        before_probe()?;
        Ok(LocalHost {
            host_id: instance_id.into(),
            platform: "macos".into(),
            profile_dir: profile_dir.clone(),
            executable: executable.clone(),
            pid: 1,
            started_at_ms: 1,
            epoch: 1,
            authority: "127.0.0.1:1".into(),
            package_sha256: String::new(),
        })
    });
    let conn = Connection::open_in_memory().unwrap();
    schema::init(&conn).unwrap();
    migration::migrate(&conn).unwrap();
    let ledger = Arc::new(Mutex::new(conn));
    let hub = DshControlHub::with_contract_read(
        DshDb::Owned(Arc::clone(&ledger)),
        Arc::new(native),
        probe,
        Arc::new(|_| {
            Ok(ContractReadReport {
                evidence: ContractEvidence {
                    package_sha256: "isolated-http-package".into(),
                    rule_version: CONTRACT_RULE_VERSION,
                    matches: BTreeMap::from([(
                        Operation::Send,
                        OperationContractMatch {
                            method: "session/prompt".into(),
                            structural_rule: "fixture-structural-match".into(),
                            semantic_rule: "fixture-semantic-match".into(),
                        },
                    )]),
                },
                inconclusive: Default::default(),
                unresolved: Default::default(),
                budget_limited: false,
            })
        }),
        Some(resolver),
    );
    hub.set_auto_adapt_source(Arc::new(|| true));
    hub.open_write_gate();
    (hub, ledger, root)
}

async fn send(hub: Arc<DshControlHub>, id: &str) -> Result<(ActionReceipt, bool), DshError> {
    hub.dispatch_session_send_for_instance(
        device(),
        SESSION,
        id,
        SendPayload {
            text: format!("message for {id}"),
            receipt_ids: vec![],
            queue_only: false,
            bypass_paused_queue: false,
        },
        true,
        Some(HOST),
    )
    .await
}

fn receipt(ledger: &Mutex<Connection>, id: &str) -> ActionReceipt {
    DshActionStore::new(&ledger.lock().unwrap())
        .view(&device(), id)
        .unwrap()
        .receipt
}

#[tokio::test]
async fn aborting_reserved_send_still_settles_ledger_and_advances_session_queue() {
    let release = Arc::new(Notify::new());
    let (calls, mut called) = mpsc::unbounded_channel();
    let (hub, ledger, _root) = fixture(
        HOST,
        TestNative {
            calls,
            release_first: Some(Arc::clone(&release)),
        },
        || Ok(()),
        Arc::new(|host, session| Ok(resolve(host, session))),
    );
    let caller = tokio::spawn(send(Arc::clone(&hub), FIRST));
    assert_eq!(
        timeout(WAIT, called.recv())
            .await
            .expect("first native call")
            .as_deref(),
        Some(FIRST)
    );
    assert_eq!(receipt(&ledger, FIRST).state, ActionState::Reserved);
    let (queued, duplicate) = timeout(WAIT, send(Arc::clone(&hub), NEXT))
        .await
        .unwrap()
        .unwrap();
    assert!(!duplicate);
    assert_eq!(queued.state, ActionState::Queued);

    caller.abort();
    assert!(caller.await.unwrap_err().is_cancelled());
    release.notify_one();

    let progress = timeout(WAIT, async {
        loop {
            if receipt(&ledger, FIRST).state == ActionState::Failed
                && receipt(&ledger, NEXT).state == ActionState::Accepted
            {
                break;
            }
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    assert!(
        progress.is_ok(),
        "aborting the caller must not strand the ledger or queue: first={:?}, next={:?}",
        receipt(&ledger, FIRST).state,
        receipt(&ledger, NEXT).state,
    );
    assert_eq!(called.try_recv().unwrap(), NEXT);
    assert!(called.try_recv().is_err(), "unexpected extra native send");
}

#[tokio::test]
async fn duplicate_replays_concurrent_ledger_entry_when_its_host_probe_rejects() {
    let first_in_resolver = Arc::new(Notify::new());
    let retry_in_probe = Arc::new(Notify::new());
    let reject_next_probe = Arc::new(AtomicBool::new(false));
    let (release_first, first_release) = blocking_channel::channel();
    let first_release = Mutex::new(Some(first_release));
    let (release_retry, retry_release) = blocking_channel::channel();
    let retry_release = Mutex::new(retry_release);
    let (calls, mut called) = mpsc::unbounded_channel();
    let (hub, ledger, _root) = fixture(
        HOST,
        TestNative {
            calls,
            release_first: None,
        },
        {
            let entered = Arc::clone(&retry_in_probe);
            let reject = Arc::clone(&reject_next_probe);
            move || {
                if reject.swap(false, Ordering::SeqCst) {
                    entered.notify_one();
                    retry_release.lock().unwrap().recv_timeout(WAIT).unwrap();
                    return Err(DshError::new(
                        codes::HOST_NOT_RUNNING,
                        "fixture host probe refused",
                    ));
                }
                Ok(())
            }
        },
        {
            let entered = Arc::clone(&first_in_resolver);
            Arc::new(move |host, session| {
                let wait = first_release.lock().unwrap().take();
                if let Some(wait) = wait {
                    entered.notify_one();
                    wait.recv_timeout(WAIT).unwrap();
                }
                Ok(resolve(host, session))
            })
        },
    );

    let first = tokio::spawn(send(Arc::clone(&hub), FIRST));
    timeout(WAIT, first_in_resolver.notified())
        .await
        .expect("first request reached resolver before registration");
    reject_next_probe.store(true, Ordering::SeqCst);
    let retry = tokio::spawn(send(hub, FIRST));
    // 进入探针证明重试的首次查账未命中。此时首个请求仍停在登记前。
    timeout(WAIT, retry_in_probe.notified())
        .await
        .expect("retry reached host probe after ledger miss");
    release_first.send(()).unwrap();
    let (first_receipt, first_duplicate) = timeout(WAIT, first).await.unwrap().unwrap().unwrap();
    assert!(!first_duplicate);
    assert_eq!(first_receipt.state, ActionState::Accepted);
    assert_eq!(receipt(&ledger, FIRST).state, first_receipt.state);
    assert_eq!(called.try_recv().unwrap(), FIRST);

    release_retry.send(()).unwrap();
    let (replayed, duplicate) = timeout(WAIT, retry).await.unwrap().unwrap().expect(
        "retry must replay the existing action even though its host probe rejected admission",
    );
    assert!(duplicate);
    assert_eq!(replayed.action_id, first_receipt.action_id);
    assert_eq!(replayed.state, first_receipt.state);
    assert!(
        called.try_recv().is_err(),
        "duplicate issued another native send"
    );
}

struct NoTerminal;
impl multi_agents_manager_lib::inject::engine::Injector for NoTerminal {
    fn name(&self) -> &'static str {
        "forbidden-terminal"
    }
    fn locate_and_inject(&self, _: u32, _: &str) -> Result<(), String> {
        panic!("DSH HTTP 发送不得回退终端注入")
    }
    fn locate_and_send_key_inner(&self, _: u32, _: &str) -> Result<(), String> {
        panic!("DSH HTTP 发送不得回退终端按键")
    }
}

fn remote_state(
    hub: Arc<DshControlHub>,
    ledger: Arc<Mutex<rusqlite::Connection>>,
    instance_id: &str,
    scan: Arc<dyn Fn() -> bool + Send + Sync>,
) -> Arc<RemoteState> {
    let store = DeviceStore::Owned(ledger);
    store.with(|conn| {
        persist_device(
            conn,
            &NewDevice {
                id: "isolated-http-device".into(),
                name: "isolated device".into(),
                ua: "test".into(),
                origin_ip: "127.0.0.1".into(),
                via: "lan".into(),
                // HTTP gate 使用真实时钟检查设备有效期，DSH 的固定时钟仅用于授权代际。
                paired_at: chrono::Utc::now().timestamp_millis(),
            },
        )
        .unwrap()
    });
    let session = Session {
        id: SESSION.into(),
        agent_type: AgentType::Dsh,
        project_name: "isolated project".into(),
        project_path: "/isolated-project".into(),
        title: None,
        git_branch: None,
        github_url: None,
        status: SessionStatus::Processing,
        last_message: None,
        last_message_role: None,
        last_message_subagent_report: false,
        flap_from_subagent_activity: false,
        last_activity_at: "2026-11-06T00:00:00Z".into(),
        pid: 101,
        cpu_usage: 0.0,
        active_subagent_count: 0,
        form: ProcessForm::App,
        jump_supported: false,
        unread: false,
        unread_turned_green_at_ms: None,
        dsh_instance_id: Some(instance_id.into()),
        dsh_subagent: None,
    };
    let snapshot = move || {
        let sessions = if scan() {
            vec![session.clone()]
        } else {
            vec![]
        };
        SessionsResponse {
            total_count: sessions.len(),
            sessions,
            waiting_count: 0,
        }
    };
    Arc::new(RemoteState {
        opencode_native: std::sync::Arc::new(
            multi_agents_manager_lib::services::opencode_native::NativeHub::new(
                multi_agents_manager_lib::remote::pairing::DeviceStore::memory(),
            ),
        ),
        session_source: Box::new(snapshot.clone()),
        fresh_session_source: Box::new(snapshot),
        pairing_counter: Box::new(Vec::new),
        board_hidden_ids: Box::new(Vec::new),
        board_hidden_hide: Arc::new(|_| 0),
        board_hidden_unhide: Arc::new(|_| 0),
        unread_mark_read: Arc::new(|_, _, _| ()),
        session_close: Arc::new(|_| panic!("不应关闭真实进程")),
        store,
        host_source: Box::new(|| json!({})),
        message_source: Box::new(|_, _, _| Err("unused".into())),
        path_source: Box::new(|_, _, _| (vec![], false)),
        watcher_tx: tokio::sync::broadcast::channel(8).0,
        sse_registry: Default::default(),
        max_devices_source: Box::new(|| 3),
        pin_limiter: Mutex::new(multi_agents_manager_lib::remote::pin::PinRateLimiter::new()),
        global_pin_limiter: Mutex::new(
            multi_agents_manager_lib::remote::pin::PinRateLimiter::global(),
        ),
        pin_source: Box::new(|| None),
        now_source: Box::new(|| chrono::Utc::now().timestamp_millis()),
        via_hosts_source: Box::new(|| None),
        rate_bucket_channels_source: Box::new(Vec::new),
        home_source: Box::new(|| None),
        ui_config_source: Box::new(|| None),
        subagent_source: Default::default(),
        subagent_message_source: Default::default(),
        injector: Arc::new(NoTerminal),
        resume_spawner: Arc::new(|_| panic!("不应启动终端")),
        archive_source: Box::new(Vec::new),
        archive_delete: Arc::new(|_| 0),
        confirm_probe: Arc::new(|_, _, _| panic!("DSH 不应走终端确认")),
        dialog_probe: Arc::new(|_, _| None),
        screen_probe: Arc::new(|_, _| None),
        capability_table: multi_agents_manager_lib::inject::capability::new_table(),
        create_hub: Arc::new(multi_agents_manager_lib::remote::server::CreateTaskHub::stub()),
        codex_control: multi_agents_manager_lib::remote::codex_ipc::CodexControlHub::disabled_arc(),
        dsh_control: hub,
        dsh_console_mount: None,
        dsh_console_upstream_source: None,
        dsh_auto_adapt_source: Arc::new(|| true),
        desktop_language: Default::default(),
    })
}

async fn post_send(app: axum::Router, instance_id: &str, id: &str) -> (StatusCode, Value) {
    let request = Request::post("/m/api/v1/session-send")
        .header("content-type", "application/json")
        .header("cookie", "mam_device=isolated-http-device")
        .extension(axum::extract::ConnectInfo(
            "127.0.0.1:30000".parse::<std::net::SocketAddr>().unwrap(),
        ))
        .body(Body::from(
            json!({ "sessionId": SESSION, "instanceId": instance_id, "actionId": id,
                    "text": format!("message for {id}") })
            .to_string(),
        ))
        .unwrap();
    let response = timeout(WAIT, app.oneshot(request)).await.unwrap().unwrap();
    let (parts, body) = response.into_parts();
    let bytes = body.collect().await.unwrap().to_bytes();
    (parts.status, serde_json::from_slice(&bytes).unwrap())
}

struct ScanPause {
    entered: Arc<Notify>,
    release: blocking_channel::Sender<()>,
    scan: Arc<dyn Fn() -> bool + Send + Sync>,
}

impl ScanPause {
    fn once(paused_snapshot_has_session: bool) -> Self {
        let entered = Arc::new(Notify::new());
        let (release, wait) = blocking_channel::channel();
        let wait = Mutex::new(Some(wait));
        let scan = {
            let entered = Arc::clone(&entered);
            Arc::new(move || {
                let receiver = wait.lock().unwrap().take();
                if let Some(receiver) = receiver {
                    entered.notify_one();
                    receiver
                        .recv_timeout(WAIT)
                        .expect("release the paused scan");
                    paused_snapshot_has_session
                } else {
                    true
                }
            })
        };
        Self {
            entered,
            release,
            scan,
        }
    }
}

#[tokio::test]
async fn http_send_scanned_before_instance_reopen_never_reaches_native() {
    let (calls, mut called) = mpsc::unbounded_channel();
    let (hub, ledger, _root) = fixture(
        REOPENED_HOST,
        TestNative {
            calls,
            release_first: None,
        },
        || Ok(()),
        Arc::new(|host, session| Ok(resolve(host, session))),
    );
    let pause = ScanPause::once(true);
    let app = router(remote_state(hub, ledger, REOPENED_HOST, pause.scan));
    let older = tokio::spawn(post_send(app.clone(), REOPENED_HOST, FIRST));
    timeout(WAIT, pause.entered.notified())
        .await
        .expect("old request entered the session scan");
    let epoch = production::mark_closing(REOPENED_HOST);
    assert!(production::clear_closing_and_advance(REOPENED_HOST, epoch));
    pause.release.send(()).unwrap();
    let old_response = older.await.unwrap();
    assert!(
        called.try_recv().is_err(),
        "pre-close request reached native after reopen: {old_response:?}"
    );

    let (status, body) = post_send(app.clone(), REOPENED_HOST, NEXT).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["receipt"]["state"], "accepted", "{body}");
    assert_eq!(called.try_recv().unwrap(), NEXT);
    let (status, body) = post_send(app, REOPENED_HOST, NEXT).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["duplicate"], true, "{body}");
    assert!(
        called.try_recv().is_err(),
        "fresh request dispatched more than once"
    );
}

#[tokio::test]
async fn http_send_missed_scan_replays_same_id_committed_during_scan() {
    let (calls, mut called) = mpsc::unbounded_channel();
    let (hub, ledger, _root) = fixture(
        HOST,
        TestNative {
            calls,
            release_first: None,
        },
        || Ok(()),
        Arc::new(|host, session| Ok(resolve(host, session))),
    );
    let pause = ScanPause::once(false);
    let app = router(remote_state(hub, ledger, HOST, pause.scan));
    let missed = tokio::spawn(post_send(app.clone(), HOST, FIRST));
    timeout(WAIT, pause.entered.notified())
        .await
        .expect("same-ID request entered the session scan");
    let (status, body) = post_send(app, HOST, FIRST).await;
    pause.release.send(()).unwrap();
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["receipt"]["state"], "accepted", "{body}");

    let (status, replay) = missed.await.unwrap();
    assert_eq!(status, StatusCode::OK, "{replay}");
    assert_eq!(replay["duplicate"], true, "{replay}");
    assert_eq!(replay["receipt"], body["receipt"]);
    assert_eq!(called.try_recv().unwrap(), FIRST);
    assert!(called.try_recv().is_err(), "same action dispatched twice");
}

#[tokio::test]
async fn http_send_missed_scan_without_ledger_preserves_action_id_as_unconfirmed() {
    let (calls, mut called) = mpsc::unbounded_channel();
    let (hub, ledger, _root) = fixture(
        HOST,
        TestNative {
            calls,
            release_first: None,
        },
        || Ok(()),
        Arc::new(|host, session| Ok(resolve(host, session))),
    );
    let app = router(remote_state(hub, ledger, HOST, Arc::new(|| false)));
    let (status, body) = post_send(app, HOST, FIRST).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["status"], "dsh", "{body}");
    assert_eq!(body["receipt"]["actionId"], FIRST, "{body}");
    assert_eq!(body["receipt"]["state"], "unknown_delivery", "{body}");
    assert_eq!(
        body["receipt"]["reasonCode"], "dsh_admission_unconfirmed",
        "{body}"
    );
    assert!(called.try_recv().is_err(), "missing session reached native");
}
