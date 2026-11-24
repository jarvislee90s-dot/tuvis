//! 真实 UDS 凭据交付与实例关闭的交错回归。
//! 生产挂载和测试探针是进程级 OnceLock，故本文件是独立的集成测试进程。

#![cfg(all(unix, feature = "dsh-test-probes"))]

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use app::remote::{console_lifecycle, console_mount};
use app::services::dsh_control::auth::{
    self, BindingChallengeMessage, DshAuthService, HomeBinding, RustSigningKey, BINDING_PROTOCOL,
    CHALLENGE_TTL_MS,
};
use app::services::dsh_control::error::{codes, DshError};
use app::services::dsh_control::identity;
use app::services::dsh_control::instances::{instance_id_for, HostKind, InstanceRegistry};
use app::services::dsh_control::model::{
    AuthorizationState, CommandRequest, DshClock, DshCommandRunner, DshEnvironment, DshFileSource,
    DshListener, DshProcess, DshProcessSource, RunOutcome, TestUnobservableEnvironments,
};
use app::services::dsh_control::production;
use app::services::dsh_control::switch::{
    RemoteSwitchService, SwitchPhase, SwitchRecord, UpgradeEvidenceFacts,
};
use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use multi_agents_manager_lib as app;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{unix::OwnedReadHalf, unix::OwnedWriteHalf, TcpListener, UnixStream};
use tokio::sync::{mpsc, oneshot};
use tower::ServiceExt;

const EXE: &str = "/late-credential-fixture/DeepSeek Harness.app/Contents/MacOS/DeepSeek Harness";
const NOW: i64 = 1_760_000_000_000;
const DEVICE: &str = "late-fixture-device";
const WAIT: Duration = Duration::from_secs(5);

struct ProfileFiles(BTreeSet<PathBuf>);
impl DshFileSource for ProfileFiles {
    fn exists(&self, path: &Path) -> bool {
        self.0.contains(path)
    }
    fn list_dir(&self, _: &Path) -> Vec<PathBuf> {
        Vec::new()
    }
    fn read_small(&self, path: &Path, max: u64) -> Result<Vec<u8>, DshError> {
        if self.0.contains(path)
            && path.file_name().is_some_and(|s| s == "package.json")
            && max >= 2
        {
            Ok(b"{}".to_vec())
        } else {
            Err(DshError::new(
                codes::UNSUPPORTED,
                "fixture file unavailable",
            ))
        }
    }
    fn read_link_target(&self, _: &Path) -> Option<PathBuf> {
        None
    }
    fn is_readable_regular_file(&self, path: &Path) -> bool {
        self.0.contains(path) && path.file_name().is_some_and(|s| s == "package.json")
    }
}

struct Processes {
    process: DshProcess,
    authority: String,
}
impl DshProcessSource for Processes {
    fn list_processes(&self) -> Vec<DshProcess> {
        vec![self.process.clone()]
    }
    fn listening_sockets(&self) -> Vec<DshListener> {
        vec![DshListener {
            owner_pid: self.process.pid,
            authority: self.authority.clone(),
        }]
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
        Err(DshError::new(
            codes::UNSUPPORTED,
            "fixture has no host commands",
        ))
    }
}

async fn read_frame(reader: &mut BufReader<OwnedReadHalf>) -> Value {
    tokio::time::timeout(WAIT, async {
        let mut line = String::new();
        assert!(
            reader.read_line(&mut line).await.unwrap() > 0,
            "IPC 提前关闭"
        );
        serde_json::from_str(&line).expect("IPC 返回有效 JSON")
    })
    .await
    .expect("等待 IPC 帧超时")
}

async fn send_frame(writer: &mut OwnedWriteHalf, frame: Value) {
    tokio::time::timeout(WAIT, writer.write_all(format!("{frame}\n").as_bytes()))
        .await
        .expect("发送 IPC 帧超时")
        .expect("发送 IPC 帧成功");
}

#[path = "support/dsh_console.rs"]
mod dsh_console;
use dsh_console::session_capability;

async fn get_console(router: &Router, token: &str) -> StatusCode {
    tokio::time::timeout(
        WAIT,
        router.clone().oneshot(
            Request::builder()
                .uri(format!("/console/{token}/"))
                .body(Body::empty())
                .unwrap(),
        ),
    )
    .await
    .expect("控制台请求超时")
    .unwrap()
    .status()
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Commit {
    Native,
    Console,
}

struct Pause {
    at: Commit,
    reached: oneshot::Sender<()>,
    resume: std::sync::mpsc::Receiver<()>,
}

struct Observation {
    epoch: u64,
    authority: String,
    pause: Option<Pause>,
    handled: mpsc::UnboundedSender<()>,
    queue_advances: usize,
}

fn observations() -> &'static Mutex<HashMap<String, Observation>> {
    static OBSERVATIONS: OnceLock<Mutex<HashMap<String, Observation>>> = OnceLock::new();
    OBSERVATIONS.get_or_init(Default::default)
}

fn pause_at(instance: &str, at: Commit) {
    let pause = {
        let mut all = observations().lock().unwrap();
        all.get_mut(instance).and_then(|observation| {
            if observation
                .pause
                .as_ref()
                .is_some_and(|pause| pause.at == at)
            {
                observation.pause.take()
            } else {
                None
            }
        })
    };
    if let Some(pause) = pause {
        pause.reached.send(()).expect("测试必须到达指定提交断点");
        pause
            .resume
            .recv_timeout(WAIT)
            .expect("测试必须释放提交断点");
    }
}

fn production_mount() -> &'static Arc<console_mount::ConsoleMount> {
    static INIT: OnceLock<()> = OnceLock::new();
    INIT.get_or_init(|| {
        let mount = Arc::new(
            console_mount::ConsoleMount::new()
                .with_device_check(|id| id == DEVICE)
                .with_target_check(|id, epoch, authority| {
                    observations()
                        .lock()
                        .unwrap()
                        .get(id)
                        .is_some_and(|o| o.epoch == epoch && o.authority == authority)
                }),
        );
        assert!(console_mount::init_production_mount_for_test(mount).is_ok());
        auth::init_before_native_credential_commit_hook_for_test(Arc::new(|id| {
            pause_at(id, Commit::Native);
        }));
        console_lifecycle::init_between_console_credential_commits_hook_for_test(Arc::new(|id| {
            pause_at(id, Commit::Console);
        }));
        auth::init_credential_queue_advance_hook_for_test(Arc::new(|id| {
            if let Some(observation) = observations().lock().unwrap().get_mut(id) {
                observation.queue_advances += 1;
            }
        }));
        auth::init_credential_handled_hook_for_test(Arc::new(|id| {
            if let Some(observation) = observations().lock().unwrap().get(id) {
                let _ = observation.handled.send(());
            }
        }));
    });
    console_mount::production()
}

struct Instance {
    _root: tempfile::TempDir,
    id: String,
    authority: String,
    epoch: u64,
    svc: Arc<DshAuthService>,
    switch: Arc<RemoteSwitchService>,
    reader: BufReader<OwnedReadHalf>,
    writer: OwnedWriteHalf,
    binding: String,
    handled: mpsc::UnboundedReceiver<()>,
    requests: mpsc::UnboundedReceiver<String>,
    upstream: tokio::task::JoinHandle<()>,
}

impl Instance {
    async fn bind() -> Self {
        // macOS sun_path 限长，所有状态都留在短临时数据根。
        let root = tempfile::Builder::new()
            .prefix("dsh-late-")
            .tempdir_in("/tmp")
            .unwrap();
        let home = root.path().canonicalize().unwrap().join("dsh");
        let profile = home.join("profiles/desktop");
        std::fs::create_dir_all(&profile).unwrap();
        let registry =
            Arc::new(InstanceRegistry::open(root.path().join("instances.json")).unwrap());
        let selected = registry
            .register_candidate_with_home(HostKind::Desktop, &profile, Some(&home))
            .unwrap();
        let id = instance_id_for(HostKind::Desktop, &profile);
        assert_eq!(selected.instance_id, id);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let authority = listener.local_addr().unwrap().to_string();
        let (request_tx, requests) = mpsc::unbounded_channel();
        let upstream = tokio::spawn(async move {
            let router = Router::new().fallback(move |headers: axum::http::HeaderMap| {
                let request_tx = request_tx.clone();
                async move {
                    let cookie = headers
                        .get(axum::http::header::COOKIE)
                        .and_then(|value| value.to_str().ok())
                        .unwrap_or_default();
                    request_tx.send(cookie.to_owned()).unwrap();
                    StatusCode::OK
                }
            });
            axum::serve(listener, router).await.unwrap();
        });

        let pid = std::process::id();
        let env = Arc::new(DshEnvironment {
            home: home.clone(),
            tuvis_home: root.path().join("tuvis"),
            dsh_state_dir: Some(root.path().join("s")),
            processes: Arc::new(Processes {
                process: DshProcess {
                    pid,
                    exe: EXE.into(),
                    cmdline: vec![EXE.into(),
                        "/late-credential-fixture/node_modules/@deepseek-ai/dsh-desktop-host/lib/index.js".into(),
                        profile.to_string_lossy().into_owned()],
                    started_at_ms: NOW - 10_000,
                },
                authority: authority.clone(),
            }),
            files: Arc::new(ProfileFiles(BTreeSet::from([
                profile.clone(), profile.join("package.json"),
            ]))),
            runner: Arc::new(NoCommands),
            clock: Arc::new(Clock),
            environments: Arc::new(TestUnobservableEnvironments),
        });
        let host = identity::discover_instance(&env, &id).expect("夹具宿主已核验");
        let epoch = host.epoch;
        assert_eq!(host.authority, authority);
        let (handled_tx, handled) = mpsc::unbounded_channel();
        observations().lock().unwrap().insert(
            id.clone(),
            Observation {
                epoch,
                authority: authority.clone(),
                pause: None,
                handled: handled_tx,
                queue_advances: 0,
            },
        );
        let svc = Arc::new(
            production::init_instance_auth(&env, &id)
                .unwrap()
                .without_liveness_watcher(),
        );
        let revoker = Arc::new(production::InstanceBindingRevoker::with_resolver(
            env.clone(),
            id.clone(),
            Arc::new({
                let svc = svc.clone();
                move || Ok(svc.clone())
            }),
        ));
        let manager = Arc::new(production::init_instance_manager(&env, &selected, revoker));
        let switch = Arc::new(
            RemoteSwitchService::new(
                env.clone(),
                registry,
                id.clone(),
                Arc::new({
                    let svc = svc.clone();
                    move || Ok((manager.clone(), svc.clone()))
                }),
                Arc::new(|| UpgradeEvidenceFacts {
                    bound_digest: None,
                    bound_revocation_pending: false,
                    installed: false,
                }),
            )
            .unwrap(),
        );
        let state = auth::instance_state_dir(&env, &id).unwrap();
        std::fs::write(
            state.join("remote-switch.json"),
            serde_json::to_vec(&SwitchRecord {
                enabled: true,
                phase: SwitchPhase::Ready,
                generation: 1,
                ..SwitchRecord::default()
            })
            .unwrap(),
        )
        .unwrap();
        assert!(switch.read().unwrap().enabled);
        svc.serve().await.expect("启动真实 UDS 监听");
        let rendezvous: Value =
            serde_json::from_slice(&std::fs::read(state.join("rendezvous.json")).unwrap()).unwrap();
        let stream = tokio::time::timeout(
            WAIT,
            UnixStream::connect(rendezvous["address"].as_str().unwrap()),
        )
        .await
        .expect("UDS 连接超时")
        .unwrap();
        let (reader, mut writer) = stream.into_split();
        let mut reader = BufReader::new(reader);
        let keys = RustSigningKey::generate();
        let challenge = BindingChallengeMessage {
            binding_protocol: BINDING_PROTOCOL,
            key_digests: [svc.key_digest().into(), auth::key_digest(&keys.spki)],
            nonce: auth::b64url_encode(&[1; 32]),
            host_id: id.clone(),
            epoch: epoch.to_string(),
            expiry: NOW + CHALLENGE_TTL_MS,
            home_binding: HomeBinding {
                home_root: home.to_string_lossy().into_owned(),
                host_pid: pid,
                host_started_at_ms: host.started_at_ms,
            },
        };
        send_frame(&mut writer, json!({"type":"hello", "bindingProtocol":BINDING_PROTOCOL,
            "publicKeySpki":auth::b64url_encode(&keys.spki), "executable":EXE, "pid":pid,
            "startedAtMs":host.started_at_ms, "hostId":id, "epoch":epoch.to_string(),
            "nonce":challenge.nonce, "expiry":challenge.expiry, "homeBinding":challenge.home_binding,
            "signature":auth::b64url_encode(&keys.sign(&challenge.to_signing_bytes()))})).await;
        let reply = read_frame(&mut reader).await;
        assert_eq!(reply["type"], "server_hello");
        assert_eq!(reply["state"], "pending_approval");
        let binding = reply["bindingId"].as_str().unwrap().to_owned();
        tokio::time::timeout(WAIT, svc.approve(&binding))
            .await
            .expect("批准超时")
            .expect("批准成功");
        assert_eq!(read_frame(&mut reader).await["type"], "granted");
        Self {
            _root: root,
            id,
            authority,
            epoch,
            svc,
            switch,
            reader,
            writer,
            binding,
            handled,
            requests,
            upstream,
        }
    }

    async fn deliver(&mut self, credential: &str) {
        send_frame(
            &mut self.writer,
            json!({"type":"credential", "bindingId":self.binding,
            "cookie":format!("dsh-api={credential}"), "browserCookie":format!("dsh-browser={credential}")}),
        )
        .await;
    }

    async fn wait_until_handled(&mut self) {
        tokio::time::timeout(WAIT, self.handled.recv())
            .await
            .expect("IPC 凭据处理必须结束")
            .unwrap();
    }

    fn queue_advances(&self) -> usize {
        observations().lock().unwrap()[&self.id].queue_advances
    }

    fn assert_cache_presence(&self, expected: (bool, bool)) {
        assert_eq!(
            console_lifecycle::credential_cache_presence_for_test(&self.id),
            expected
        );
    }
}

impl Drop for Instance {
    fn drop(&mut self) {
        self.upstream.abort();
        observations().lock().unwrap().remove(&self.id);
    }
}

// 生产挂载由同进程的全部用例共享，隔离各用例自己的关闭交错。
static CASE_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

async fn close_while_delivering(at: Commit) {
    let _case = CASE_LOCK.lock().await;
    let mount = production_mount();
    let mut target = Instance::bind().await;
    let mut other = Instance::bind().await;
    other.deliver("fixture").await;
    other.wait_until_handled().await;
    assert_eq!(read_frame(&mut other.reader).await["type"], "granted");
    assert_eq!(other.queue_advances(), 1, "正向交付必须请求推进队列");
    other.assert_cache_presence((true, true));
    mount
        .register_if_current(
            session_capability(
                DEVICE,
                mount,
                &other.id,
                &other.id,
                other.epoch,
                &other.authority,
            ),
            mount.revocation_sequence(),
        )
        .expect("另一实例控制台能力有效");
    let router = console_mount::router(mount.clone());
    assert_eq!(get_console(&router, &other.id).await, StatusCode::OK);
    assert_eq!(other.requests.try_recv().unwrap(), "dsh-browser=fixture");

    if at == Commit::Console {
        target.deliver("fixture").await;
        target.wait_until_handled().await;
        assert_eq!(read_frame(&mut target.reader).await["type"], "granted");
        target.assert_cache_presence((true, true));
        mount
            .register_if_current(
                session_capability(
                    DEVICE,
                    mount,
                    &target.id,
                    &target.id,
                    target.epoch,
                    &target.authority,
                ),
                mount.revocation_sequence(),
            )
            .expect("目标实例关闭前控制台能力有效");
        assert_eq!(get_console(&router, &target.id).await, StatusCode::OK);
        assert_eq!(target.requests.try_recv().unwrap(), "dsh-browser=fixture");
    }
    let previous_queue_advances = target.queue_advances();
    let (reached, at_commit) = oneshot::channel();
    let (release, resume) = std::sync::mpsc::channel();
    observations()
        .lock()
        .unwrap()
        .get_mut(&target.id)
        .unwrap()
        .pause = Some(Pause {
        at,
        reached,
        resume,
    });
    target.deliver("late-fixture").await;
    tokio::time::timeout(WAIT, at_commit)
        .await
        .expect("必须到达指定提交断点")
        .unwrap();
    let had_credentials = at == Commit::Console;
    assert_eq!(
        target.svc.has_delivered_peer(&target.binding),
        had_credentials
    );
    target.assert_cache_presence((had_credentials, had_credentials));
    if had_credentials {
        assert_eq!(
            console_lifecycle::upstream_credential(&target.id).as_deref(),
            Some("dsh-api=fixture")
        );
        assert_eq!(get_console(&router, &target.id).await, StatusCode::OK);
        assert_eq!(target.requests.try_recv().unwrap(), "dsh-browser=fixture");
    }
    let switch = target.switch.clone();
    let closed = tokio::time::timeout(WAIT, tokio::task::spawn_blocking(move || switch.disable()))
        .await
        .expect("关闭不得等待暂停的提交")
        .unwrap()
        .expect("关闭成功");
    assert!(!closed.enabled);
    assert_eq!(closed.phase, SwitchPhase::WaitingUninstall);
    assert_eq!(
        target.svc.authorization_state(),
        AuthorizationState::Revoked
    );
    target.assert_cache_presence((false, false));

    release.send(()).unwrap();
    // 完整关闭会关掉真实 UDS，等待分支完成探针以免把提前 EOF 误算作完成。
    target.wait_until_handled().await;
    assert!(
        !target.svc.has_delivered_peer(&target.binding),
        "旧原生句柄不可回填"
    );
    target.assert_cache_presence((false, false));
    assert_eq!(
        target.queue_advances(),
        previous_queue_advances,
        "关闭后迟到凭据不可请求推进队列"
    );

    assert_eq!(
        get_console(&router, &target.id).await,
        StatusCode::FORBIDDEN
    );
    assert!(matches!(
        target.requests.try_recv(),
        Err(mpsc::error::TryRecvError::Empty)
    ));
    assert!(other.svc.has_delivered_peer(&other.binding));
    other.assert_cache_presence((true, true));
    assert_eq!(get_console(&router, &other.id).await, StatusCode::OK);
    assert_eq!(other.requests.try_recv().unwrap(), "dsh-browser=fixture");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn closing_instance_rejects_a_credential_paused_before_native_commit() {
    close_while_delivering(Commit::Native).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn closing_instance_prevents_a_late_console_cache_publish() {
    close_while_delivering(Commit::Console).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn closing_one_instance_preserves_another_instances_pending_credential() {
    let _case = CASE_LOCK.lock().await;
    let mount = production_mount();
    let mut a = Instance::bind().await;
    let mut b = Instance::bind().await;
    a.deliver("closing-fixture").await;
    a.wait_until_handled().await;
    assert_eq!(read_frame(&mut a.reader).await["type"], "granted");
    a.assert_cache_presence((true, true));

    let (reached, at_commit) = oneshot::channel();
    let (release, resume) = std::sync::mpsc::channel();
    observations().lock().unwrap().get_mut(&b.id).unwrap().pause = Some(Pause {
        at: Commit::Native,
        reached,
        resume,
    });
    b.deliver("pending-fixture").await;
    tokio::time::timeout(WAIT, at_commit)
        .await
        .expect("B 必须到达原生提交前的断点")
        .unwrap();
    assert!(!b.svc.has_delivered_peer(&b.binding));
    b.assert_cache_presence((false, false));
    assert_eq!(b.queue_advances(), 0);

    let switch = a.switch.clone();
    let closed = tokio::time::timeout(WAIT, tokio::task::spawn_blocking(move || switch.disable()))
        .await
        .expect("关闭 A 不得等待 B 的提交")
        .unwrap()
        .expect("关闭 A 成功");
    assert!(!closed.enabled);
    assert_eq!(closed.phase, SwitchPhase::WaitingUninstall);
    assert_eq!(a.svc.authorization_state(), AuthorizationState::Revoked);
    a.assert_cache_presence((false, false));
    assert!(b.switch.read().unwrap().enabled);
    assert_eq!(b.svc.authorization_state(), AuthorizationState::Bound);

    release.send(()).unwrap();
    b.wait_until_handled().await;
    let reply = read_frame(&mut b.reader).await;
    assert_eq!(
        reply["type"], "granted",
        "关闭 A 后 B 的交付必须成功，收到 {reply}"
    );
    assert!(b.svc.has_delivered_peer(&b.binding));
    b.assert_cache_presence((true, true));
    assert_eq!(b.queue_advances(), 1);
    mount
        .register_if_current(
            session_capability(DEVICE, mount, &b.id, &b.id, b.epoch, &b.authority),
            mount.revocation_sequence(),
        )
        .expect("B 的控制台能力有效");
    let router = console_mount::router(mount.clone());
    assert_eq!(get_console(&router, &b.id).await, StatusCode::OK);
    assert_eq!(
        b.requests.try_recv().unwrap(),
        "dsh-browser=pending-fixture"
    );
}
