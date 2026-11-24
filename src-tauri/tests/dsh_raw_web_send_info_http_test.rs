//! raw Web 保守拒绝必须传播到当前会话的 HTTP 发送能力声明。
//!
//! HTTP → 真实 RemoteState/router → DshControlHub → InstanceRegistry；文件、实例
//! 登记、授权账本和动作账本均为用例私有。仅替换 OS 进程快照、IPC 传输、契约
//! 扫描和最终原生发送边界，不替换宿主探针、会话归属解析或 HTTP handler。
//! Desktop 从空候选登记，经真实签名 hello/本地授权意图建立当代信任，再锚定；
//! raw Web 不登记、不写 Bound 行、不预置 verified/home 证明。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::http::StatusCode;
use multi_agents_manager_lib::remote::pairing::{persist_device, DeviceStore, NewDevice};
use multi_agents_manager_lib::remote::server::{router, RemoteState};
use multi_agents_manager_lib::services::dsh_control::actions::{NativeDispatch, NativeSendOutcome};
use multi_agents_manager_lib::services::dsh_control::auth::{
    self, BindingChallengeMessage, DshAuthService, HomeBinding, RustSigningKey,
};
use multi_agents_manager_lib::services::dsh_control::contract::{
    ContractReadReport, CONTRACT_RULE_VERSION,
};
use multi_agents_manager_lib::services::dsh_control::error::{codes, DshError};
use multi_agents_manager_lib::services::dsh_control::identity::discover_detailed;
use multi_agents_manager_lib::services::dsh_control::instances::{
    HostKind, InstanceIdentityProbe, InstanceRegistry,
};
use multi_agents_manager_lib::services::dsh_control::local_ipc::mem::{
    mem_listener, MemClient, MemClientFactory,
};
use multi_agents_manager_lib::services::dsh_control::local_ipc::PeerOsIdentity;
use multi_agents_manager_lib::services::dsh_control::model::{
    AuthorizationState, CommandRequest, ContractEvidence, DshClock, DshCommandRunner,
    DshEnvironment, DshListener, DshProcess, DshProcessSource, Operation, OperationContractMatch,
    RunOutcome, SendPayload, Target,
};
use multi_agents_manager_lib::services::dsh_control::production::{
    self, BindingFingerprintProbe, FsFileSource,
};
use multi_agents_manager_lib::services::dsh_control::{DshControlHub, DshDb};
use multi_agents_manager_lib::session::model::{AgentType, ProcessForm, Session, SessionStatus};
use multi_agents_manager_lib::session::SessionsResponse;
use serde_json::{json, Value};

const SESSION: &str = "session-cccccccc";
const DESKTOP_PID: u32 = 101;
const WEB_PID: u32 = 202;
const NOW: i64 = 1_760_000_000_000;
const STARTED: i64 = NOW - 10_000;
const TEXT: &str = "当前会话的归属必须可证";
const ACTION: &str = "ownership-http-action";
const COOKIE: &str = "mam_device=ownership-http-device";
const WAIT: Duration = Duration::from_secs(5);

struct ProcessSnapshot {
    hosts: Vec<DshProcess>,
    listeners: Vec<DshListener>,
}
impl DshProcessSource for ProcessSnapshot {
    fn list_processes(&self) -> Vec<DshProcess> {
        self.hosts.clone()
    }
    fn listening_sockets(&self) -> Vec<DshListener> {
        self.listeners.clone()
    }
}
struct Clock;
impl DshClock for Clock {
    fn now_ms(&self) -> i64 {
        NOW
    }
}
struct NoCommands;
impl DshCommandRunner for NoCommands {
    fn run(&self, _: &CommandRequest) -> Result<RunOutcome, DshError> {
        panic!("隔离回归不得执行宿主命令")
    }
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

#[derive(Default)]
struct NativeBoundary {
    calls: Mutex<Vec<(Target, String, String)>>,
}
impl NativeDispatch for NativeBoundary {
    fn send_message<'a>(
        &'a self,
        target: &'a Target,
        payload: &'a SendPayload,
        action_id: &'a str,
    ) -> futures::future::BoxFuture<'a, Result<NativeSendOutcome, DshError>> {
        Box::pin(async move {
            self.calls.lock().unwrap().push((
                target.clone(),
                payload.text.clone(),
                action_id.to_owned(),
            ));
            Ok(NativeSendOutcome::Accepted {
                native_id: Some("native-ownership-http".into()),
            })
        })
    }
}

fn seed_profile(home: &Path, kind: &str) -> PathBuf {
    let profile = home.join("profiles").join(kind);
    std::fs::create_dir_all(&profile).unwrap();
    std::fs::write(profile.join("package.json"), b"{}").unwrap();
    std::fs::canonicalize(profile).unwrap()
}
fn seed_session(home: &Path, id: &str) {
    std::fs::create_dir_all(home.join("sessions/project").join(id)).unwrap();
}

/// 这里只隔离包扫描；契约证据不冒充插件身份或会话归属证明。
fn matched_send_contract() -> ContractReadReport {
    ContractReadReport {
        evidence: ContractEvidence {
            package_sha256: "isolated-http-contract".into(),
            rule_version: CONTRACT_RULE_VERSION,
            matches: BTreeMap::from([(
                Operation::Send,
                OperationContractMatch {
                    method: "session/prompt".into(),
                    structural_rule: "structural".into(),
                    semantic_rule: "semantic".into(),
                },
            )]),
        },
        inconclusive: Default::default(),
        unresolved: Default::default(),
        budget_limited: false,
    }
}

struct Bench {
    client: reqwest::Client,
    base_url: String,
    instance_id: String,
    ledger: Arc<Mutex<rusqlite::Connection>>,
    native: Arc<NativeBoundary>,
    http_task: tokio::task::JoinHandle<()>,
    hub: Arc<DshControlHub>,
    env: Arc<DshEnvironment>,
    registry: Arc<InstanceRegistry>,
    // 活授权连接及 accept 源保持到 HTTP 验证结束，避免历史账本冒充当代信任。
    _auth: Arc<DshAuthService>,
    _auth_client: MemClient,
    _auth_factory: Arc<MemClientFactory>,
    root: tempfile::TempDir,
}
impl Drop for Bench {
    fn drop(&mut self) {
        self.http_task.abort();
    }
}
impl Bench {
    async fn start(with_raw_web: bool, duplicate_session: bool) -> Self {
        let scratch = Path::new(env!("CARGO_MANIFEST_DIR")).join("target/tmp");
        std::fs::create_dir_all(&scratch).unwrap();
        let root = tempfile::tempdir_in(scratch).unwrap();
        let project = root.path().join("project");
        std::fs::create_dir(&project).unwrap();
        let profile = seed_profile(&root.path().join("Desktop A"), "desktop");
        let web_profile = seed_profile(&root.path().join("Web B"), "web");
        let home = profile.parent().unwrap().parent().unwrap().to_path_buf();
        let web_home = web_profile
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .to_path_buf();
        seed_session(&home, SESSION);
        seed_session(
            &web_home,
            if duplicate_session {
                SESSION
            } else {
                "session-bbbbbbbb"
            },
        );
        let contents = root.path().join("DeepSeek Harness.app/Contents");
        std::fs::create_dir_all(contents.join("MacOS")).unwrap();
        std::fs::create_dir_all(contents.join("Resources")).unwrap();
        let exe = contents.join("MacOS/DeepSeek Harness");
        std::fs::write(&exe, b"isolated executable marker").unwrap();
        // 能力门的前后 stat 均读取真实临时包文件。
        std::fs::write(contents.join("Resources/app.asar"), b"isolated package").unwrap();
        let entry = contents
            .join("Resources/app.asar/dsh/node_modules/@deepseek-ai/dsh-desktop-host/lib/index.js");
        let mut hosts = vec![DshProcess {
            pid: DESKTOP_PID,
            exe: exe.clone(),
            cmdline: vec![
                exe.to_string_lossy().into_owned(),
                "--expose-internals".into(),
                entry.to_string_lossy().into_owned(),
                profile.to_string_lossy().into_owned(),
            ],
            started_at_ms: STARTED,
        }];
        let mut listeners = vec![DshListener {
            owner_pid: DESKTOP_PID,
            authority: "127.0.0.1:5101".into(),
        }];
        if with_raw_web {
            hosts.push(DshProcess {
                pid: WEB_PID,
                exe: "/isolated-runtime/node".into(),
                // 无 home/profile 参数；env.home 不能成为 Web writer 归属证明。
                cmdline: vec![
                    "/isolated-runtime/node".into(),
                    "/isolated-runtime/bin/dsh".into(),
                    "web".into(),
                ],
                started_at_ms: STARTED + 1000,
            });
            listeners.push(DshListener {
                owner_pid: WEB_PID,
                authority: "127.0.0.1:5202".into(),
            });
        }
        let env = Arc::new(DshEnvironment {
            home: web_home,
            tuvis_home: root.path().join("tuvis-shadow"),
            dsh_state_dir: Some(root.path().join("auth-shadow")),
            processes: Arc::new(ProcessSnapshot { hosts, listeners }),
            files: Arc::new(FsFileSource),
            runner: Arc::new(NoCommands),
            clock: Arc::new(Clock),
            // 测试默认「不可观测」：显式拒绝 Web home 归属（fail-closed），
            // 绝不让假进程源落到真实系统 PID 上。需要 Web 归属的用例自行注入假观测器。
            environments: std::sync::Arc::new(multi_agents_manager_lib::services::dsh_control::model::TestUnobservableEnvironments),
        });
        let probe_slot = Arc::new(Mutex::new(None));
        let probe = Arc::new(BindingFingerprintProbe {
            env: env.clone(),
            service_override: probe_slot.clone(),
        });
        let registry = Arc::new(
            InstanceRegistry::open_with_env(root.path().join("instances.json"), Some(env.clone()))
                .unwrap()
                .with_identity_probe(probe.clone()),
        );
        assert!(registry.registered_ids().unwrap().is_empty());
        let candidate = registry
            .register_candidate(HostKind::Desktop, &profile)
            .unwrap();
        assert!(candidate.identity_fingerprint.is_empty());
        registry.select(&candidate.instance_id).unwrap();
        let discovery = discover_detailed(&env).unwrap();
        assert_eq!(discovery.candidates.len(), 1, "Web 不得被猜成候选");
        assert!(
            discovery.candidates[0].verified,
            "Desktop 由 argv/标记/监听核验"
        );
        assert_eq!(discovery.candidates[0].instance_id, candidate.instance_id);
        assert_eq!(discovery.raw_web_hosts.len(), usize::from(with_raw_web));
        assert_eq!(
            discovery.unattributable_web_hosts,
            usize::from(with_raw_web)
        );
        let host = discovery.candidates[0].host.clone();

        let (listener, factory) = mem_listener(PeerOsIdentity {
            pid: DESKTOP_PID,
            uid: None,
        });
        let svc = Arc::new(
            production::init_instance_auth(&env, &candidate.instance_id)
                .unwrap()
                .with_listener(Box::new(listener)),
        );
        *probe_slot.lock().unwrap() = Some(svc.clone());
        assert_eq!(svc.authorization_state(), AuthorizationState::Unbound);
        assert_eq!(probe.current_fingerprint(&candidate).unwrap(), None);
        assert!(
            registry.resolve_selected().is_err(),
            "授权前不得凭候选登记过门"
        );
        svc.serve().await.unwrap();
        // 使用生产授权意图和签名 hello，禁止直接写 Bound/受信指纹夹具。
        svc.begin_authorization_intent().unwrap();
        let key = RustSigningKey::generate();
        let challenge = BindingChallengeMessage {
            binding_protocol: 2,
            key_digests: [svc.key_digest().into(), auth::key_digest(&key.spki)],
            nonce: auth::b64url_encode(&[31; 32]),
            host_id: candidate.instance_id.clone(),
            epoch: host.epoch.to_string(),
            expiry: NOW + auth::CHALLENGE_TTL_MS,
            home_binding: HomeBinding {
                home_root: home.to_string_lossy().into_owned(),
                host_pid: DESKTOP_PID,
                host_started_at_ms: STARTED,
            },
        };
        let auth_client = factory.connect().await;
        let hello = json!({
            "type": "hello", "bindingProtocol": 2,
            "publicKeySpki": auth::b64url_encode(&key.spki),
            "executable": exe, "pid": DESKTOP_PID, "startedAtMs": STARTED,
            "hostId": candidate.instance_id, "epoch": challenge.epoch,
            "nonce": challenge.nonce, "expiry": challenge.expiry,
            "homeBinding": challenge.home_binding,
            "signature": auth::b64url_encode(&key.sign(&challenge.to_signing_bytes())),
        });
        let reply: Value = tokio::time::timeout(WAIT, async {
            auth_client.send(&hello.to_string()).await.unwrap();
            serde_json::from_str(&auth_client.recv().await.unwrap()).unwrap()
        })
        .await
        .expect("真实授权握手须有界完成");
        assert_eq!(reply["type"], "server_hello", "授权前置条件：{reply}");
        assert_eq!(
            reply["state"], "authorized",
            "不得用拒绝握手伪造 RED：{reply}"
        );
        let fingerprint = probe
            .current_fingerprint(&candidate)
            .unwrap()
            .expect("真实当代握手指纹");
        registry
            .register(HostKind::Desktop, &profile, &fingerprint)
            .unwrap();
        assert_eq!(
            registry.registered_ids().unwrap(),
            vec![candidate.instance_id.clone()]
        );
        assert_eq!(
            registry.resolve_selected().unwrap().host_id,
            candidate.instance_id
        );

        let conn = rusqlite::Connection::open(root.path().join("isolated-ledger.db")).unwrap();
        multi_agents_manager_lib::database::schema::init(&conn).unwrap();
        multi_agents_manager_lib::database::migration::migrate(&conn).unwrap();
        let ledger = Arc::new(Mutex::new(conn));
        let native = Arc::new(NativeBoundary::default());
        let hub = DshControlHub::with_contract_read(
            DshDb::Owned(ledger.clone()),
            native.clone(),
            {
                let registry = registry.clone();
                Arc::new(move || registry.resolve_selected())
            },
            Arc::new(|_| Ok(matched_send_contract())),
            Some({
                let registry = registry.clone();
                Arc::new(move |id, sid| registry.resolve_session(id, sid))
            }),
        );
        hub.set_auto_adapt_source(Arc::new(|| true));
        hub.startup_recovery().unwrap();
        let state = remote_state(
            hub.clone(),
            ledger.clone(),
            &candidate.instance_id,
            &project,
        );
        let socket = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base_url = format!("http://{}/m/api/v1", socket.local_addr().unwrap());
        let http_task = tokio::spawn(async move {
            axum::serve(
                socket,
                router(state).into_make_service_with_connect_info::<std::net::SocketAddr>(),
            )
            .await
            .unwrap();
        });
        Self {
            client: reqwest::Client::builder()
                .no_proxy()
                .timeout(WAIT)
                .build()
                .unwrap(),
            base_url,
            instance_id: candidate.instance_id,
            ledger,
            native,
            http_task,
            hub,
            env,
            registry,
            _auth: svc,
            _auth_client: auth_client,
            _auth_factory: factory,
            root,
        }
    }

    async fn send_info(&self) -> (StatusCode, Value) {
        let response = self
            .client
            .get(format!("{}/session-send-info", self.base_url))
            .query(&[
                ("session_id", SESSION),
                ("instanceId", self.instance_id.as_str()),
            ])
            .header("cookie", COOKIE)
            .send()
            .await
            .unwrap();
        decode_http_response(response).await
    }
    async fn send(&self) -> (StatusCode, Value) {
        let response = self
            .client
            .post(format!("{}/session-send", self.base_url))
            .header("cookie", COOKIE)
            .json(&json!({
                "sessionId": SESSION, "instanceId": self.instance_id,
                "actionId": ACTION, "text": TEXT,
            }))
            .send()
            .await
            .unwrap();
        decode_http_response(response).await
    }
    fn action_count(&self) -> i64 {
        self.ledger
            .lock()
            .unwrap()
            .query_row("SELECT COUNT(*) FROM dsh_actions", [], |r| r.get(0))
            .unwrap()
    }
}

const A05_FILENAME: &str = "a05-note.txt";
const A05_CONTENT: &str = "ordinary A05 attachment";
const A05_CLAUDE_SESSION: &str = "a05-claude-session";

impl Bench {
    async fn upload_attachment(
        &self,
        session_id: &str,
        instance_id: Option<&str>,
    ) -> (StatusCode, Value) {
        self.upload_bytes(
            session_id,
            instance_id,
            A05_FILENAME,
            A05_CONTENT.as_bytes(),
        )
        .await
    }

    async fn upload_bytes(
        &self,
        session_id: &str,
        instance_id: Option<&str>,
        name: &str,
        bytes: &[u8],
    ) -> (StatusCode, Value) {
        let mut query = vec![("session_id", session_id), ("name", name)];
        query.extend(instance_id.map(|id| ("instanceId", id)));
        let response = self
            .client
            .post(format!("{}/session-attachment", self.base_url))
            .query(&query)
            .header("cookie", COOKIE)
            .header("content-type", "application/octet-stream")
            .body(bytes.to_vec())
            .send()
            .await
            .unwrap();
        decode_http_response(response).await
    }
}

async fn assert_dsh_upload_publishes_original_bytes(name: &str, bytes: &[u8]) {
    let bench = Bench::start(false, false).await;
    let (status, body) = bench
        .upload_bytes(SESSION, Some(&bench.instance_id), name, bytes)
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["size"], bytes.len());
    let path = Path::new(body["path"].as_str().unwrap());
    let expected = bench
        .root
        .path()
        .join("project/.tuvis-attachments")
        .join(SESSION)
        .join(name);
    assert_eq!(path, expected);
    assert_eq!(std::fs::read(path).unwrap(), bytes);
}

#[tokio::test]
async fn a05_authorized_dsh_upload_publishes_text_in_session_project() {
    assert_dsh_upload_publishes_original_bytes(A05_FILENAME, A05_CONTENT.as_bytes()).await;
}

#[tokio::test]
async fn a05_authorized_dsh_upload_publishes_png_in_session_project() {
    use base64::{engine::general_purpose::STANDARD, Engine as _};
    // 合法的 1×1 PNG；上传回执后须可原样读取，不能只检查扩展名或魔数。
    let bytes = STANDARD
        .decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=")
        .unwrap();
    assert_dsh_upload_publishes_original_bytes("pixel.png", &bytes).await;
}

#[cfg(unix)]
#[tokio::test]
async fn a05_published_dsh_upload_adds_project_git_exclude_once() {
    let bench = Bench::start(false, false).await;
    let project = bench.root.path().join("project");
    let git = project.join(".git");
    for dir in ["info", "objects", "refs/heads"] {
        std::fs::create_dir_all(git.join(dir)).unwrap();
    }
    std::fs::write(git.join("HEAD"), b"ref: refs/heads/main\n").unwrap();
    let exclude = git.join("info/exclude");
    std::fs::write(&exclude, b"# existing patterns\nlocal-cache/\n").unwrap();

    for name in [A05_FILENAME, "another.txt"] {
        let (status, body) = bench
            .upload_bytes(
                SESSION,
                Some(&bench.instance_id),
                name,
                A05_CONTENT.as_bytes(),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(
            std::fs::read(&exclude).unwrap(),
            b"# existing patterns\nlocal-cache/\n.tuvis-attachments/\n"
        );
    }
}

#[cfg(unix)]
#[tokio::test]
async fn a05_published_dsh_upload_skips_linked_git_exclude() {
    let bench = Bench::start(false, false).await;
    let project = bench.root.path().join("project");
    let exclude = project.join(".git/info/exclude");
    std::fs::create_dir_all(exclude.parent().unwrap()).unwrap();
    std::fs::write(project.join(".git/HEAD"), b"ref: refs/heads/main\n").unwrap();
    let note = project.join("local-note.txt");
    std::fs::write(&note, b"ordinary local note\n").unwrap();
    std::os::unix::fs::symlink(&note, &exclude).unwrap();

    let (status, body) = bench
        .upload_attachment(SESSION, Some(&bench.instance_id))
        .await;
    assert_eq!(status, StatusCode::OK, "Git 收尾跳过链接不影响附件：{body}");
    assert_eq!(std::fs::read(&note).unwrap(), b"ordinary local note\n");
    assert_eq!(std::fs::read_link(&exclude).unwrap(), note);
}

#[tokio::test]
async fn a05_non_dsh_session_still_accepts_ordinary_upload() {
    let bench = Bench::start(false, false).await;
    let (status, body) = bench.upload_attachment(A05_CLAUDE_SESSION, None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let path = Path::new(body["path"].as_str().unwrap());
    let attachment_dir = bench
        .root
        .path()
        .join("project/.tuvis-attachments")
        .join(A05_CLAUDE_SESSION);
    assert!(path.starts_with(attachment_dir), "{body}");
    assert_eq!(std::fs::read_to_string(path).unwrap(), A05_CONTENT);
}

#[tokio::test]
async fn a05_authorized_dsh_send_info_advertises_upload_without_creating_directories() {
    let bench = Bench::start(false, false).await;
    let (status, body) = bench.send_info().await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["injectable"], true, "文字发送应保持可用：{body}");
    assert!(body["attachmentUnavailableReasonCode"].is_null(), "{body}");
    assert_eq!(
        bench
            .root
            .path()
            .join("project")
            .read_dir()
            .unwrap()
            .count(),
        0
    );
}

#[tokio::test]
async fn a05_dsh_session_still_previews_ordinary_file() {
    let bench = Bench::start(false, false).await;
    std::fs::write(
        bench.root.path().join("project").join(A05_FILENAME),
        A05_CONTENT,
    )
    .unwrap();
    let response = bench
        .client
        .get(format!("{}/file", bench.base_url))
        .query(&[
            ("session_id", SESSION),
            ("instanceId", bench.instance_id.as_str()),
            ("path", A05_FILENAME),
        ])
        .header("cookie", COOKIE)
        .send()
        .await
        .unwrap();
    let (status, body) = decode_http_response(response).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["content"], A05_CONTENT);
}

async fn decode_http_response(response: reqwest::Response) -> (StatusCode, Value) {
    let status = response.status();
    let url = response.url().to_string();
    let body = response.text().await.unwrap();
    let value = serde_json::from_str(&body)
        .unwrap_or_else(|e| panic!("HTTP 前置条件失败：{url} {status}, body={body:?}: {e}"));
    (status, value)
}

fn remote_state(
    hub: Arc<DshControlHub>,
    ledger: Arc<Mutex<rusqlite::Connection>>,
    instance_id: &str,
    project: &Path,
) -> Arc<RemoteState> {
    let store = DeviceStore::Owned(ledger);
    store.with(|conn| {
        persist_device(
            conn,
            &NewDevice {
                id: "ownership-http-device".into(),
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
        project_path: project.to_str().unwrap().into(),
        title: None,
        git_branch: None,
        github_url: None,
        status: SessionStatus::Processing,
        last_message: None,
        last_message_role: None,
        last_message_subagent_report: false,
        flap_from_subagent_activity: false,
        last_activity_at: "2026-11-06T00:00:00Z".into(),
        pid: DESKTOP_PID,
        cpu_usage: 0.0,
        active_subagent_count: 0,
        form: ProcessForm::App,
        jump_supported: false,
        unread: false,
        unread_turned_green_at_ms: None,
        dsh_instance_id: Some(instance_id.into()),
        dsh_subagent: None,
    };
    let other_session = Session {
        id: A05_CLAUDE_SESSION.into(),
        agent_type: AgentType::Claude,
        dsh_instance_id: None,
        ..session.clone()
    };
    let snapshot = move || SessionsResponse {
        sessions: vec![session.clone(), other_session.clone()],
        total_count: 2,
        waiting_count: 0,
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
        now_source: Box::new(|| NOW),
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

async fn assert_raw_web_send_info_refuses(duplicate_session: bool) {
    let b = Bench::start(true, duplicate_session).await;
    // GET 必须先于发送；不能依赖先请求 Web/失败发送所留下的本地记忆。
    let (info_status, info) = b.send_info().await;
    let (send_status, sent) = b.send().await;
    eprintln!("raw Web duplicate={duplicate_session}: send-info {info_status} {info}; send {send_status} {sent}");
    assert_eq!(info_status, StatusCode::OK, "{info}");
    assert_eq!(
        send_status,
        StatusCode::FORBIDDEN,
        "真实发送应先证明保守拒绝：{sent}"
    );
    assert_eq!(sent["reasonCode"], codes::SESSION_OWNERSHIP_UNPROVEN);
    assert!(
        b.native.calls.lock().unwrap().is_empty(),
        "归属不明不得触达原生发送"
    );
    assert_eq!(b.action_count(), 0, "归属不明不得新建发送动作");
    assert_eq!(
        info["injectable"], false,
        "当前会话已被真实发送拒绝，能力声明不得仍可注入：{info}; {sent}"
    );
    assert_eq!(info["reasonCode"], codes::SESSION_OWNERSHIP_UNPROVEN);
}

#[tokio::test]
async fn selected_desktop_with_raw_web_duplicate_is_not_advertised_as_injectable() {
    assert_raw_web_send_info_refuses(true).await;
}

#[tokio::test]
async fn selected_desktop_with_raw_web_without_visible_collision_is_not_advertised_as_injectable() {
    assert_raw_web_send_info_refuses(false).await;
}

#[tokio::test]
async fn selected_desktop_alone_is_injectable_and_sends_with_an_offline_web_duplicate() {
    let b = Bench::start(false, true).await;
    assert_eq!(b.action_count(), 0);
    let (info_status, info) = b.send_info().await;
    let (send_status, sent) = b.send().await;
    eprintln!("Desktop alone: send-info {info_status} {info}; send {send_status} {sent}");
    assert_eq!(info_status, StatusCode::OK, "{info}");
    assert_eq!(
        info["injectable"], true,
        "离线 Web 目录不得挡住独存 Desktop：{info}"
    );
    assert_eq!(info["channels"], json!(["dsh_desktop"]));
    assert_eq!(
        send_status,
        StatusCode::OK,
        "正对照必须真实进入 hub 发送：{sent}"
    );
    assert_eq!(sent["status"], "dsh");
    assert_eq!(sent["receipt"]["state"], "accepted");
    assert_eq!(sent["receipt"]["actionId"], ACTION);
    assert_eq!(b.action_count(), 1);
    let calls = b.native.calls.lock().unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].0.host_id, b.instance_id);
    assert_eq!(calls[0].0.session_id, SESSION);
    assert!(calls[0].0.epoch > 0);
    assert_eq!(calls[0].1, TEXT);
    assert_eq!(calls[0].2, ACTION);
}

#[tokio::test]
async fn waiting_uninstall_immediately_disables_send_info_for_trusted_dsh_session() {
    use multi_agents_manager_lib::services::dsh_control::switch::{
        RemoteSwitchService, SwitchPhase, SwitchRecord, SWITCH_SCOPE,
    };

    let b = Bench::start(false, false).await;
    let switch = Arc::new(
        RemoteSwitchService::new(
            b.env.clone(),
            b.registry.clone(),
            &b.instance_id,
            Arc::new(|| panic!("执行门不得装配生命周期服务")),
            Arc::new(|| panic!("已有开关记录，不得派生升级证据")),
        )
        .unwrap(),
    );
    let record_path = auth::instance_state_dir(&b.env, &b.instance_id)
        .unwrap()
        .join("remote-switch.json");
    let mut record = SwitchRecord {
        enabled: true,
        phase: SwitchPhase::Ready,
        generation: 1,
        scope: SWITCH_SCOPE.into(),
        updated_at_ms: NOW,
        ..Default::default()
    };
    std::fs::write(&record_path, serde_json::to_vec(&record).unwrap()).unwrap();
    b.hub.set_switch_gate(Some(Arc::new({
        let switch = switch.clone();
        let instance_id = b.instance_id.clone();
        move |id| {
            assert_eq!(id, instance_id);
            switch.execution_gate()
        }
    })));

    let (status, open) = b.send_info().await;
    assert_eq!(status, StatusCode::OK, "{open}");
    assert_eq!(open["injectable"], true, "关闭前必须可发送：{open}");

    // 固定 disable 落盘后、撤权完成前的窗口，保留真实握手与会话归属。
    record.enabled = false;
    record.phase = SwitchPhase::WaitingUninstall;
    record.generation += 1;
    let next = record_path.with_extension("staged");
    std::fs::write(&next, serde_json::to_vec(&record).unwrap()).unwrap();
    std::fs::rename(next, &record_path).unwrap();
    assert_eq!(switch.read_raw().unwrap(), Some(record));
    assert_eq!(switch.execution_gate().unwrap_err().code, codes::SWITCH_OFF);
    assert_eq!(b._auth.authorization_state(), AuthorizationState::Bound);
    assert_eq!(
        b.registry.resolve_selected().unwrap().host_id,
        b.instance_id
    );

    // GET 必须先于发送：关闭后的首个能力查询不能靠失败写请求纠偏。
    let (status, closed) = b.send_info().await;
    let (send_status, sent) = b.send().await;
    if send_status == StatusCode::OK {
        assert_eq!(sent["receipt"]["state"], "failed", "{sent}");
        assert_eq!(
            sent["receipt"]["reasonCode"],
            format!("dsh_switch_off:{}", codes::SWITCH_OFF),
            "{sent}"
        );
    } else {
        assert_eq!(sent["reasonCode"], codes::SWITCH_OFF, "{sent}");
    }
    assert!(b.native.calls.lock().unwrap().is_empty());
    assert_eq!(status, StatusCode::OK, "{closed}");
    assert_eq!(
        closed["injectable"], false,
        "等待卸载仍显示可发送：{closed}"
    );
    assert_eq!(closed["reasonCode"], codes::SWITCH_OFF, "{closed}");
}
