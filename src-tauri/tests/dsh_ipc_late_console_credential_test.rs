//! 真实 UDS 凭据交付与撤权交错回归。此文件单独编译为一个集成测试进程，
//! 因为生产挂载和接收钩子都是进程级 OnceLock。
//! 运行：cargo test --features dsh-test-probes --test dsh_ipc_late_console_credential_test
//! 本用例的进程表、profile、时钟为隔离夹具；不代表真实 DSH 宿主实机验收。

#![cfg(all(unix, feature = "dsh-test-probes"))]

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use app::remote::{console_lifecycle, console_mount};
use app::services::dsh_control::auth::{
    self, BindingChallengeMessage, HomeBinding, RustSigningKey, BINDING_PROTOCOL, CHALLENGE_TTL_MS,
};
use app::services::dsh_control::error::{codes, DshError};
use app::services::dsh_control::identity;
use app::services::dsh_control::instances::{instance_id_for, HostKind};
use app::services::dsh_control::model::{
    AuthorizationState, CommandRequest, DshClock, DshCommandRunner, DshEnvironment, DshFileSource,
    DshListener, DshProcess, DshProcessSource, RunOutcome, TestUnobservableEnvironments,
};
use app::services::dsh_control::production;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use multi_agents_manager_lib as app;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tower::ServiceExt;

const HOME: &str = "/late-credential-fixture/.dsh";
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
        panic!("握手测试不得执行宿主命令")
    }
}

async fn read_frame(reader: &mut BufReader<tokio::net::unix::OwnedReadHalf>) -> Value {
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

async fn send_frame(writer: &mut tokio::net::unix::OwnedWriteHalf, frame: Value) {
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn revoked_binding_cannot_install_a_credential_accepted_before_revocation() {
    let hits = Arc::new(AtomicUsize::new(0));
    let observed_cookies = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let upstream = Router::new().fallback({
        let hits = Arc::clone(&hits);
        let observed_cookies = Arc::clone(&observed_cookies);
        move |headers: axum::http::HeaderMap| {
            let hits = Arc::clone(&hits);
            let observed_cookies = Arc::clone(&observed_cookies);
            async move {
                hits.fetch_add(1, Ordering::SeqCst);
                observed_cookies.lock().unwrap().push(
                    headers
                        .get(axum::http::header::COOKIE)
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("")
                        .to_owned(),
                );
                (StatusCode::OK, "fixture console")
            }
        }
    });
    let upstream_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let authority = upstream_listener.local_addr().unwrap().to_string();
    let upstream_task =
        tokio::spawn(async move { axum::serve(upstream_listener, upstream).await.unwrap() });

    // macOS sun_path 限长；整个测试状态保存在短临时目录内。
    let tmp = tempfile::Builder::new()
        .prefix("dsh-late-")
        .tempdir_in("/tmp")
        .unwrap();
    let profile = Path::new(HOME).join("profiles/desktop");
    let instance = instance_id_for(HostKind::Desktop, &profile);
    let pid = std::process::id();
    let env = DshEnvironment {
        home: HOME.into(), tuvis_home: tmp.path().join("tuvis"), dsh_state_dir: Some(tmp.path().join("s")),
        processes: Arc::new(Processes {
            process: DshProcess { pid, exe: EXE.into(), cmdline: vec![EXE.into(),
                "/late-credential-fixture/node_modules/@deepseek-ai/dsh-desktop-host/lib/index.js".into(),
                profile.to_string_lossy().into_owned()], started_at_ms: NOW - 10_000 },
            authority: authority.clone(),
        }),
        files: Arc::new(ProfileFiles(BTreeSet::from([profile.clone(), profile.join("package.json")]))),
        runner: Arc::new(NoCommands), clock: Arc::new(Clock), environments: Arc::new(TestUnobservableEnvironments),
    };
    let host = identity::discover_instance(&env, &instance).expect("夹具宿主已核验");
    assert_eq!(host.authority, authority);
    let target_instance = instance.clone();
    let target_authority = authority.clone();
    let epoch = host.epoch;
    let mount = Arc::new(
        console_mount::ConsoleMount::new()
            .with_device_check(|id| id == DEVICE)
            .with_target_check(move |id, e, a| {
                id == target_instance && e == epoch && a == target_authority
            }),
    );
    assert!(
        console_mount::init_production_mount_for_test(Arc::clone(&mount)).is_ok(),
        "必须在生产挂载首次使用之前注入"
    );
    assert!(
        Arc::ptr_eq(&mount, console_mount::production()),
        "IPC、撤权和路由必须共享同一挂载"
    );

    // 首次交付放行作正向对照；第二次交付仅在成功接收后停在安装前。
    let calls = Arc::new(AtomicUsize::new(0));
    let (reached_tx, mut reached_rx) = tokio::sync::mpsc::channel::<()>(1);
    let release = Arc::new(tokio::sync::Notify::new());
    let hook = Arc::new({
        let calls = Arc::clone(&calls);
        let release = Arc::clone(&release);
        move || -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> {
            let call = calls.fetch_add(1, Ordering::SeqCst);
            let reached = reached_tx.clone();
            let release = Arc::clone(&release);
            Box::pin(async move {
                if call == 1 {
                    let _ = reached.send(()).await;
                    release.notified().await;
                }
            })
        }
    });
    assert!(
        auth::init_credential_accepted_hook_for_test(hook).is_ok(),
        "钩子只能初始化一次"
    );
    let svc = Arc::new(production::init_instance_auth(&env, &instance).unwrap());
    svc.serve().await.expect("启动生产 UDS 监听");
    let state = auth::instance_state_dir(&env, &instance).unwrap();
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
        host_id: instance.clone(),
        epoch: epoch.to_string(),
        expiry: NOW + CHALLENGE_TTL_MS,
        home_binding: HomeBinding {
            home_root: HOME.into(),
            host_pid: pid,
            host_started_at_ms: host.started_at_ms,
        },
    };
    let hello = json!({"type":"hello", "bindingProtocol": BINDING_PROTOCOL,
        "publicKeySpki":auth::b64url_encode(&keys.spki), "executable":EXE, "pid":pid,
        "startedAtMs":host.started_at_ms, "hostId":instance, "epoch":epoch.to_string(),
        "nonce":challenge.nonce, "expiry":challenge.expiry, "homeBinding":challenge.home_binding,
        "signature":auth::b64url_encode(&keys.sign(&challenge.to_signing_bytes()))});
    send_frame(&mut writer, hello).await;
    let reply = read_frame(&mut reader).await;
    assert_eq!(reply["type"], "server_hello");
    assert_eq!(reply["state"], "pending_approval");
    let binding = reply["bindingId"].as_str().unwrap().to_owned();
    tokio::time::timeout(WAIT, svc.approve(&binding))
        .await
        .expect("批准超时")
        .expect("批准成功");
    assert_eq!(read_frame(&mut reader).await["type"], "granted");

    let credential = |browser: &str| {
        json!({"type":"credential", "bindingId":binding,
        "cookie":"dsh-api=fixture", "browserCookie":browser})
    };
    send_frame(&mut writer, credential("dsh-browser=positive")).await;
    assert_eq!(
        read_frame(&mut reader).await["type"],
        "granted",
        "正向凭据须完成真实 IPC 安装"
    );
    let sequence = mount.revocation_sequence();
    mount
        .register_if_current(
            session_capability(DEVICE, &mount, "positive", &instance, epoch, &authority),
            sequence,
        )
        .expect("正向控制台能力须通过设备、目标和凭据校验");
    let router = console_mount::router(Arc::clone(console_mount::production()));
    assert_eq!(get_console(&router, "positive").await, StatusCode::OK);
    assert_eq!(hits.load(Ordering::SeqCst), 1, "正向请求必须到达上游");
    assert_eq!(
        *observed_cookies.lock().unwrap(),
        vec!["dsh-browser=positive"]
    );

    send_frame(&mut writer, credential("dsh-browser=late")).await;
    tokio::time::timeout(WAIT, reached_rx.recv())
        .await
        .expect("等待接收成功钩子超时")
        .expect("必须到达接收成功、安装之前");
    assert_eq!(
        console_lifecycle::upstream_browser_cookie_for(&instance, epoch, &authority).as_deref(),
        Some("dsh-browser=positive"),
        "迟到交付暂停时仍应读取此前提交的凭据"
    );
    // revoke_local 是生产撤权同步核心。暂留返回的 IPC 连接，以便在释放后读取
    // credential_ack，证明安装调用已经返回，而不是只观察到了挂起状态。
    let conn = svc
        .revoke_local(&binding)
        .expect("真实撤权成功")
        .expect("受信连接存在");
    assert_eq!(svc.authorization_state(), AuthorizationState::Revoked);
    assert!(!svc.has_delivered_peer(&binding));
    release.notify_one();
    assert_eq!(
        read_frame(&mut reader).await["type"],
        "error",
        "撤权必须拒绝已经通过校验但尚未提交的交付"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert!(console_lifecycle::upstream_browser_cookie_for(&instance, epoch, &authority).is_none());

    // 撤权会主动清能力。重新登记同目标会话，避免仅靠“令牌不存在”得到假阳性；
    // 保持设备与目标检查和正向对照完全相同，唯一变化是被撤绑定的凭据。
    mount
        .register(session_capability(
            DEVICE, &mount, "after", &instance, epoch, &authority,
        ))
        .unwrap();
    assert_eq!(get_console(&router, "after").await, StatusCode::FORBIDDEN);
    assert_eq!(
        hits.load(Ordering::SeqCst),
        1,
        "撤权释放后的旧上游请求数必须为零"
    );
    assert_eq!(
        *observed_cookies.lock().unwrap(),
        vec!["dsh-browser=positive"]
    );
    conn.close();
    upstream_task.abort();
}
