//! T06：本机双向身份授权与可撤绑定（brief 指定四用例；全部经真实生产入口
//! [`DshAuthService`] 驱动，注入 mem IPC 传输 / 假进程源 / 假文件源 / 假时钟 /
//! tempdir `TUVIS_HOME`——不碰真实 `~/.dsh` / `~/.tuvis`，不起真实套接字）。
//!
//! 用例（brief 断言规范 + 逐项行为核验）：
//! 1. `unapproved_client_gets_no_cookie` —— 未批准客户端拿不到任何凭据：
//!    `before_approval.credential().is_none()`，且服务端从未向其写出 cookie 字节、
//!    未接受其 cookie 交付；
//! 2. `challenge_cannot_be_replayed` —— 同一签名挑战二次出现：
//!    `replay.error == "DSH_AUTH_REPLAY"`；
//! 3. `old_epoch_grant_is_rejected` —— 旧代际授权在宿主换代后连不上：
//!    `old_epoch.connect().is_err()`（且错误码 = DSH_AUTH_EPOCH_STALE）；
//! 4. `revoke_closes_trusted_client_peer` —— 撤权关受信 Peer：旧连接写不可用
//!    （`!old_peer.write_available()`），新握手回双签名 `serverHello(state=revoked)`：
//!    墓碑同钥是**永久**撤权，不是可恢复的「尚未批准」。
//!
//! 观测纪律：变量全部来自真实生产入口的返回或测试边界（mem 传输帧、账本文件
//! 字节），不以实现内部状态复制期望。

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use multi_agents_manager_lib::services::dsh_control::auth::{
    self as dsh_auth, BindingChallengeMessage, ClientFrame, CredentialFrame, HelloFrame,
    HomeBinding, PendingBindingView, RustSigningKey, ServerFrame, BINDING_PROTOCOL,
    CHALLENGE_TTL_MS,
};
use multi_agents_manager_lib::services::dsh_control::error::codes;
use multi_agents_manager_lib::services::dsh_control::identity;
use multi_agents_manager_lib::services::dsh_control::instances::{instance_id_for, HostKind};
use multi_agents_manager_lib::services::dsh_control::local_ipc::mem::{
    mem_listener, MemClient, MemClientFactory,
};
use multi_agents_manager_lib::services::dsh_control::local_ipc::PeerOsIdentity;
use multi_agents_manager_lib::services::dsh_control::model::{
    AuthorizationState, ClientIdentity, CommandRequest, DshClock, DshCommandRunner, DshEnvironment,
    DshFileSource, DshListener, DshProcess, DshProcessSource, RunOutcome,
};
use multi_agents_manager_lib::services::dsh_control::transport as dsh_transport;

// ============================================================
// 夹具（与 dsh_commands_test 同源的 Win 形态实测布局）
// ============================================================

const WIN_HOME: &str = r"C:\Users\bunny\.dsh";
const WIN_PROFILE_TOKEN: &str = r"C:\Users\bunny\.dsh\profiles\desktop";
const WIN_INSTALL_DIR: &str = r"D:\Program Files\Deepseek-Harness";
const HOST_PID: u32 = 37096;
const HOST_STARTED_AT: i64 = 1_760_000_000_000;
/// 测试当前用户 UID（mem 传输的 OS 身份投影）。
const TEST_UID: u32 = 501;

#[path = "support/dsh_paths.rs"]
mod dsh_paths;
use dsh_paths::{join_lexical, lexical_parent};

#[derive(Default)]
struct FakeFiles {
    files: BTreeMap<PathBuf, Vec<u8>>,
    dirs: BTreeSet<PathBuf>,
}

impl FakeFiles {
    fn set_file(&mut self, path: impl Into<PathBuf>, content: impl Into<Vec<u8>>) {
        self.files.insert(path.into(), content.into());
    }

    fn add_dir(&mut self, path: impl Into<PathBuf>) {
        self.dirs.insert(path.into());
    }
}

struct StoreSource(Arc<Mutex<FakeFiles>>);

impl DshFileSource for StoreSource {
    fn exists(&self, path: &Path) -> bool {
        let store = self.0.lock().unwrap();
        store.files.contains_key(path) || store.dirs.contains(path)
    }

    fn list_dir(&self, path: &Path) -> Vec<PathBuf> {
        let store = self.0.lock().unwrap();
        let dir = path
            .to_string_lossy()
            .trim_end_matches(['/', '\\'])
            .to_string();
        let mut out: Vec<PathBuf> = store
            .files
            .keys()
            .chain(store.dirs.iter())
            .filter(|p| {
                lexical_parent(&p.to_string_lossy())
                    .map(|parent| parent.trim_end_matches(['/', '\\']) == dir)
                    .unwrap_or(false)
            })
            .cloned()
            .collect();
        out.sort();
        out.dedup();
        out
    }

    fn read_small(&self, path: &Path, max_bytes: u64) -> Result<Vec<u8>, DshError> {
        let store = self.0.lock().unwrap();
        match store.files.get(path) {
            Some(bytes) if bytes.len() as u64 <= max_bytes => Ok(bytes.clone()),
            _ => Err(DshError::new(
                codes::UNSUPPORTED,
                format!("fake: no such file: {path:?}"),
            )),
        }
    }
}

use multi_agents_manager_lib::services::dsh_control::error::DshError;

struct SharedProcs {
    processes: Mutex<Vec<DshProcess>>,
    listeners: Mutex<Vec<DshListener>>,
}

impl DshProcessSource for SharedProcs {
    fn list_processes(&self) -> Vec<DshProcess> {
        self.processes.lock().unwrap().clone()
    }

    fn listening_sockets(&self) -> Vec<DshListener> {
        self.listeners.lock().unwrap().clone()
    }
}

impl SharedProcs {
    fn set_host_started_at(&self, started_at_ms: i64) {
        *self.processes.lock().unwrap() = vec![desktop_host_process(started_at_ms)];
        *self.listeners.lock().unwrap() = vec![DshListener {
            owner_pid: HOST_PID,
            authority: "127.0.0.1:5214".into(),
        }];
    }
}

/// 合格宿主进程（cmdline 双特征：dsh-desktop-host 包令牌 + profile 令牌）。
fn desktop_host_process(started_at_ms: i64) -> DshProcess {
    DshProcess {
        pid: HOST_PID,
        exe: PathBuf::from(format!(r"{WIN_INSTALL_DIR}\DeepSeek Harness.exe")),
        cmdline: vec![
            format!(r"{WIN_INSTALL_DIR}\DeepSeek Harness.exe"),
            "--expose-internals".into(),
            format!(
                r"{WIN_INSTALL_DIR}\resources\app.asar\dsh\node_modules\@deepseek-ai\dsh-desktop-host\lib\index.js"
            ),
            format!(r"{WIN_INSTALL_DIR}\resources\app.asar\dsh"),
            WIN_PROFILE_TOKEN.into(),
            format!(r"{WIN_INSTALL_DIR}\resources\runtime\primary-runtime"),
        ],
        started_at_ms,
    }
}

/// 可变假时钟（撤权/过期相关行为都从注入时钟取时）。
struct VarClock(Mutex<i64>);

impl DshClock for VarClock {
    fn now_ms(&self) -> i64 {
        *self.0.lock().unwrap()
    }
}

impl VarClock {
    /// 推进注入时钟（到期/换代类用例的唯一时间源；不触真实时间）。
    fn set(&self, ms: i64) {
        *self.0.lock().unwrap() = ms;
    }
}

struct NullRunner;

impl DshCommandRunner for NullRunner {
    fn run(&self, _request: &CommandRequest) -> Result<RunOutcome, DshError> {
        Err(DshError::new(codes::UNSUPPORTED, "no runner in auth tests"))
    }
}

struct Bench {
    store: Arc<Mutex<FakeFiles>>,
    procs: Arc<SharedProcs>,
    clock: Arc<VarClock>,
    env: DshEnvironment,
    _tmp: tempfile::TempDir,
}

fn bench() -> Bench {
    let mut files = FakeFiles::default();
    let dir = PathBuf::from(WIN_INSTALL_DIR);
    files.add_dir(dir.clone());
    files.set_file(
        join_lexical(&dir.to_string_lossy(), &["DeepSeek Harness.exe"]),
        b"mz",
    );
    files.set_file(
        join_lexical(&dir.to_string_lossy(), &["resources", "app-update.yml"]),
        b"provider: generic\n",
    );
    files.add_dir(join_lexical(WIN_HOME, &["profiles", "desktop"]));
    files.set_file(join_lexical(WIN_PROFILE_TOKEN, &["package.json"]), b"{}");
    let store = Arc::new(Mutex::new(files));
    let _store = Arc::clone(&store);
    let tmp = tempfile::tempdir().expect("tempdir 可建");
    let procs = Arc::new(SharedProcs {
        processes: Mutex::new(vec![desktop_host_process(HOST_STARTED_AT)]),
        listeners: Mutex::new(vec![DshListener {
            owner_pid: HOST_PID,
            authority: "127.0.0.1:5214".into(),
        }]),
    });
    let clock = Arc::new(VarClock(Mutex::new(HOST_STARTED_AT)));
    let env = DshEnvironment {
        home: PathBuf::from(WIN_HOME),
        tuvis_home: tmp.path().join("tuvis-home"),
        dsh_state_dir: None,
        processes: procs.clone(),
        files: Arc::new(StoreSource(store.clone())),
        runner: Arc::new(NullRunner),
        clock: clock.clone(),
        // 测试默认「不可观测」：显式拒绝 Web home 归属（fail-closed），
        // 绝不让假进程源落到真实系统 PID 上。需要 Web 归属的用例自行注入假观测器。
        environments: std::sync::Arc::new(
            multi_agents_manager_lib::services::dsh_control::model::TestUnobservableEnvironments,
        ),
    };
    Bench {
        store,
        procs,
        clock,
        env,
        _tmp: tmp,
    }
}

// ============================================================
// 测试客户端（握手 harness；观测面 = 真实 wire 帧）
// ============================================================

/// 夹具宿主的稳定实例 ID（H04：discover 不再固定 `local`，hello 的 hostId 必须
/// 回显 discover 的派生值——真实插件从 rendezvous 读同一 hostId）。夹具 profile
/// 与 bench 相同 → 派生值一致。
fn fixture_host_id() -> String {
    instance_id_for(HostKind::Desktop, Path::new(WIN_PROFILE_TOKEN))
}

/// v2 客户端声明来自独立夹具事实，不从服务端发现结果反推 home。
fn fixture_home_binding(home_root: &str, pid: u32, started_at_ms: i64) -> HomeBinding {
    HomeBinding {
        home_root: home_root.to_string(),
        host_pid: pid,
        host_started_at_ms: started_at_ms,
    }
}

/// 撤权决定必须来自钉住的服务端密钥，且挑战与决定两份签名均真实有效。
fn assert_signed_revoked_hello(
    reply: &ServerFrame,
    svc: &dsh_auth::DshAuthService,
    client_keys: &RustSigningKey,
    challenge: &BindingChallengeMessage,
    binding_id: &str,
) {
    let ServerFrame::ServerHello(hello) = reply else {
        panic!("撤权须返回双签名 serverHello，实际 {reply:?}");
    };
    assert_eq!(hello.binding_protocol, challenge.binding_protocol);
    assert_eq!(hello.nonce, challenge.nonce);
    assert_eq!(hello.expiry, challenge.expiry);
    assert_eq!(hello.host_id, challenge.host_id);
    assert_eq!(hello.epoch, challenge.epoch);
    assert_eq!(hello.home_binding, challenge.home_binding);
    assert_eq!(hello.state, "revoked", "已撤旧钥不得恢复授权或进入待批");
    assert_eq!(hello.binding_id.as_deref(), Some(binding_id));
    assert_eq!(hello.display_code, None);
    assert_eq!(hello.pending_expires_at, None);

    let server_spki = dsh_auth::b64url_decode(&hello.public_key_spki).expect("服务端公钥可解码");
    assert_eq!(dsh_auth::key_digest(&server_spki), svc.key_digest());
    let signature = dsh_auth::b64url_decode(&hello.signature).expect("挑战签名可解码");
    client_keys
        .verify_client(&server_spki, &challenge.to_signing_bytes(), &signature)
        .expect("服务端必须真实签署本次完整挑战");

    // 独立构造 wire 合同的决定字节；绑定号、revoked 和空审批字段均参与验签。
    #[derive(serde::Serialize)]
    #[serde(rename_all = "camelCase")]
    struct RevokedDecision<'a> {
        purpose: &'static str,
        #[serde(flatten)]
        challenge: &'a BindingChallengeMessage,
        state: &'static str,
        binding_id: &'a str,
        display_code: Option<&'a str>,
        pending_expires_at: Option<i64>,
    }
    let decision = RevokedDecision {
        purpose: "tuvis.dsh.binding-decision.v2",
        challenge,
        state: "revoked",
        binding_id,
        display_code: None,
        pending_expires_at: None,
    };
    let decision_bytes = serde_json::to_vec(&decision).expect("决定夹具可编码");
    let decision_signature =
        dsh_auth::b64url_decode(&hello.decision_signature).expect("撤权决定签名可解码");
    client_keys
        .verify_client(&server_spki, &decision_bytes, &decision_signature)
        .expect("服务端必须真实签署 revoked 决定，不能只签挑战");
}

/// 测试用插件密钥（真实 Ed25519；RustSigningKey 是通用签名密钥类型）。
struct TestClient {
    keys: RustSigningKey,
    conn: MemClient,
    /// 服务端写来的全部帧（「从未写出 cookie 字节」的观测面）。
    received: Mutex<Vec<String>>,
}

impl TestClient {
    /// 以给定代际/nonce 做一次真实 hello 往返；返回服务端原始帧。
    async fn hello_frame(
        &self,
        svc: &dsh_auth::DshAuthService,
        epoch: u64,
        nonce: [u8; 32],
    ) -> Result<ServerFrame, DshError> {
        let now = self.now(svc);
        let msg = BindingChallengeMessage {
            binding_protocol: BINDING_PROTOCOL,
            key_digests: [
                svc.key_digest().to_string(),
                dsh_auth::key_digest(&self.keys.spki),
            ],
            nonce: dsh_auth::b64url_encode(&nonce),
            host_id: fixture_host_id(),
            epoch: epoch.to_string(),
            expiry: now + CHALLENGE_TTL_MS,
            home_binding: fixture_home_binding(WIN_HOME, HOST_PID, HOST_STARTED_AT),
        };
        let signature = self.keys.sign(&msg.to_signing_bytes());
        let hello = ClientFrame::Hello(HelloFrame {
            binding_protocol: BINDING_PROTOCOL,
            public_key_spki: dsh_auth::b64url_encode(&self.keys.spki),
            executable: format!(r"{WIN_INSTALL_DIR}\DeepSeek Harness.exe"),
            pid: HOST_PID,
            started_at_ms: HOST_STARTED_AT,
            host_id: fixture_host_id(),
            epoch: epoch.to_string(),
            nonce: dsh_auth::b64url_encode(&nonce),
            expiry: now + CHALLENGE_TTL_MS,
            home_binding: msg.home_binding.clone(),
            signature: dsh_auth::b64url_encode(&signature),
        });
        self.conn.send(&dsh_auth::client_frame_line(&hello)).await?;
        let line = self.conn.recv().await?;
        self.received.lock().unwrap().push(line.clone());
        dsh_auth::parse_server_frame(&line)
    }

    fn now(&self, svc: &dsh_auth::DshAuthService) -> i64 {
        // 当前时刻从被测服务同一注入时钟读取（共享 env.clock），避免测试自造时间。
        svc.now_ms()
    }

    /// 一次完整握手（真实生产入口）的原始帧观测。
    async fn connect_frame(
        &self,
        svc: &dsh_auth::DshAuthService,
        epoch: u64,
        nonce: [u8; 32],
    ) -> Result<ServerFrame, DshError> {
        self.hello_frame(svc, epoch, nonce).await
    }

    /// 一次完整握手；**Error 帧折算成 Err**（brief 的 `connect().is_err()` 语义）。
    async fn connect(
        &self,
        svc: &dsh_auth::DshAuthService,
        epoch: u64,
        nonce: [u8; 32],
    ) -> Result<ServerFrame, DshError> {
        match self.hello_frame(svc, epoch, nonce).await? {
            ServerFrame::Error { error, detail } => Err(DshError {
                code: leak(error.as_str()),
                detail,
            }),
            ok => Ok(ok),
        }
    }

    /// 发送凭据交付帧（真实 wire）。
    async fn send_credential(
        &self,
        binding_id: &str,
        cookie: &str,
    ) -> Result<ServerFrame, DshError> {
        let frame = ClientFrame::Credential(CredentialFrame {
            binding_id: binding_id.to_string(),
            cookie: cookie.to_string(),
            browser_cookie: None,
        });
        self.conn.send(&dsh_auth::client_frame_line(&frame)).await?;
        let line = self.conn.recv().await?;
        self.received.lock().unwrap().push(line.clone());
        dsh_auth::parse_server_frame(&line)
    }

    /// 排空服务端此后推来的帧（ granted/revoked 通知）。
    async fn drain(&self) -> Vec<ServerFrame> {
        let mut out = Vec::new();
        while let Some(line) = self.conn.try_recv() {
            self.received.lock().unwrap().push(line.clone());
            if let Ok(frame) = dsh_auth::parse_server_frame(&line) {
                out.push(frame);
            }
        }
        out
    }

    /// 撤权后的写可用性（mem 传输的关闭观测；brief `write_available` 面）。
    fn write_available(&self) -> bool {
        self.conn.write_available()
    }

    /// 客户端收过的全部帧原文（断言「无 cookie 字节」用）。
    fn received_lines(&self) -> Vec<String> {
        self.received.lock().unwrap().clone()
    }
}

/// 测试边界内的错误码落点（ServerFrame::Error → DshError 的折算）。
fn leak(code: &str) -> &'static str {
    // 错误码词表都是 'static 常量：这里按已知词表匹配回 'static 引用，
    // 避免不安全泄漏；未知码只可能来自测试自身笔误。
    for known in [
        codes::AUTH_REPLAY,
        codes::AUTH_SIGNATURE,
        codes::AUTH_PROTOCOL,
        codes::AUTH_EXPIRED,
        codes::AUTH_EPOCH_STALE,
        codes::AUTH_NOT_APPROVED,
        codes::AUTH_NO_PENDING,
        codes::AUTH_UNKNOWN_BINDING,
        codes::PEER_MISMATCH,
        codes::PEER_NOT_HOST,
        codes::PEER_REVOKED,
        codes::UNSUPPORTED,
        codes::HOST_NOT_RUNNING,
        codes::INSTANCE_UNVERIFIED,
    ] {
        if known == code {
            return known;
        }
    }
    panic!("测试外错误码：{code}")
}

/// 当前代际（真实生产入口 identity::discover 的返回）。
fn current_epoch(env: &DshEnvironment) -> u64 {
    identity::discover(env)
        .expect("宿主在跑，discover 可用")
        .epoch
}

/// 装配服务 + mem 传输并启动 IPC 服务循环。
async fn served(bench: &Bench) -> (Arc<dsh_auth::DshAuthService>, Arc<MemClientFactory>) {
    served_with_env(&bench.env).await
}

/// 按给定环境装配服务 + mem 传输（I-6：域坏撤权落盘后「修复并重启」的场景复用）。
async fn served_with_env(
    env: &DshEnvironment,
) -> (Arc<dsh_auth::DshAuthService>, Arc<MemClientFactory>) {
    let peer_os = PeerOsIdentity {
        pid: HOST_PID,
        uid: Some(TEST_UID),
    };
    let (listener, factory) = mem_listener(peer_os);
    let svc = Arc::new(
        dsh_auth::DshAuthService::new(clone_env_of(env))
            .unwrap()
            .with_listener(Box::new(listener)),
    );
    Arc::clone(&svc).serve().await.expect("IPC 服务启动");
    (svc, factory)
}

/// 环境克隆（适配器 Arc 共享；tempdir 由 Bench 持有）。
fn clone_env(bench: &Bench) -> DshEnvironment {
    clone_env_of(&bench.env)
}

fn clone_env_of(env: &DshEnvironment) -> DshEnvironment {
    DshEnvironment {
        home: env.home.clone(),
        tuvis_home: env.tuvis_home.clone(),
        dsh_state_dir: env.dsh_state_dir.clone(),
        processes: env.processes.clone(),
        files: env.files.clone(),
        runner: env.runner.clone(),
        clock: env.clock.clone(),
        // 测试默认「不可观测」：显式拒绝 Web home 归属（fail-closed），
        // 绝不让假进程源落到真实系统 PID 上。需要 Web 归属的用例自行注入假观测器。
        environments: std::sync::Arc::new(
            multi_agents_manager_lib::services::dsh_control::model::TestUnobservableEnvironments,
        ),
    }
}

fn client_identity_of(client: &TestClient) -> ClientIdentity {
    ClientIdentity {
        public_key_spki: client.keys.spki.clone(),
        executable: PathBuf::from(format!(r"{WIN_INSTALL_DIR}\DeepSeek Harness.exe")),
        pid: HOST_PID,
        started_at_ms: HOST_STARTED_AT,
    }
}

// ============================================================
// brief 指定四用例
// ============================================================

/// ① 未批准客户端拿不到任何凭据（`before_approval.credential().is_none()`）。
#[tokio::test]
async fn unapproved_client_gets_no_cookie() {
    let b = bench();
    let (svc, factory) = served(&b).await;
    let client = TestClient {
        keys: RustSigningKey::generate(),
        conn: factory.connect().await,
        received: Mutex::new(Vec::new()),
    };
    let epoch = current_epoch(&b.env);

    // 真实握手：新公钥 → pending_approval + 两端核对码（不做任何授权放行）。
    let reply = client
        .connect_frame(&svc, epoch, [7u8; 32])
        .await
        .expect("握手有应答");
    let ServerFrame::ServerHello(hello) = reply else {
        panic!("新公钥应得 serverHello，得到 {reply:?}")
    };
    assert_eq!(hello.state, "pending_approval", "未批准前绝不 authorized");
    let binding_id = hello.binding_id.clone().expect("pending 必带 binding_id");
    assert_eq!(
        hello.display_code.as_deref().map(str::len),
        Some(6),
        "两端核对码 6 字符"
    );

    // brief 断言规范：批准前凭据恒为 None。
    let before_approval: PendingBindingView = svc.pending_binding().expect("待批绑定在场");
    assert!(
        before_approval.credential().is_none(),
        "批准前 credential 必为 None"
    );
    assert_eq!(before_approval.binding.binding_id, binding_id);

    // 服务端从未向客户端写出 cookie/granted 字节（排空残余帧后核对原文）。
    let _ = client.drain().await;
    for line in client.received_lines() {
        assert!(
            !line.contains("cookie"),
            "服务端向未批准客户端写出了 cookie 形字节：{line}"
        );
        assert!(!line.contains("granted"), "未批准不得放行帧：{line}");
    }

    // 客户端抢跑交付 cookie → 明确拒绝（DSH_AUTH_NOT_APPROVED），凭据仍为 None。
    let ack = client
        .send_credential(&binding_id, "owner-cookie-SECRET")
        .await
        .expect("有应答");
    let ServerFrame::Error { error, .. } = ack else {
        panic!("未批准的凭据交付必须被拒，得到 {ack:?}")
    };
    assert_eq!(error, codes::AUTH_NOT_APPROVED);
    assert!(svc
        .pending_binding()
        .expect("仍待批")
        .credential()
        .is_none());
    assert_eq!(
        svc.authorization_state(),
        AuthorizationState::Unbound,
        "未批准=未绑定"
    );
}

/// ② 同一签名挑战二次出现 → DSH_AUTH_REPLAY（`replay.error == "DSH_AUTH_REPLAY"`）。
#[tokio::test]
async fn challenge_cannot_be_replayed() {
    let b = bench();
    let (svc, factory) = served(&b).await;
    let epoch = current_epoch(&b.env);

    let keys = RustSigningKey::generate();
    let first = TestClient {
        keys: RustSigningKey::from_seed(&keys.seed()),
        conn: factory.connect().await,
        received: Mutex::new(Vec::new()),
    };
    let reply = first
        .connect_frame(&svc, epoch, [9u8; 32])
        .await
        .expect("首次握手有应答");
    let ServerFrame::ServerHello(_) = reply else {
        panic!("首次握手应成功，得到 {reply:?}")
    };

    // 同一 nonce + 同一签名的二次挑战（新连接重放同帧，同一把客户端密钥）。
    let replay_client = TestClient {
        keys: RustSigningKey::from_seed(&keys.seed()),
        conn: factory.connect().await,
        received: Mutex::new(Vec::new()),
    };
    let replay = replay_client
        .connect_frame(&svc, epoch, [9u8; 32])
        .await
        .expect("有应答");
    let ServerFrame::Error { error, .. } = replay else {
        panic!("重放必须被拒，得到 {replay:?}")
    };
    // brief 断言规范（Rust 语法落实）：replay.error == "DSH_AUTH_REPLAY"。
    assert_eq!(error, "DSH_AUTH_REPLAY");
}

/// ③ 旧代际授权在宿主换代后连不上（`old_epoch.connect().is_err()`）。
#[tokio::test]
async fn old_epoch_grant_is_rejected() {
    let b = bench();
    let (svc, factory) = served(&b).await;
    let epoch1 = current_epoch(&b.env);

    // 首绑 + 批准（真实批准入口）。
    let client = TestClient {
        keys: RustSigningKey::generate(),
        conn: factory.connect().await,
        received: Mutex::new(Vec::new()),
    };
    let reply = client
        .connect_frame(&svc, epoch1, [11u8; 32])
        .await
        .expect("握手有应答");
    let ServerFrame::ServerHello(hello) = reply else {
        panic!("首绑应得 serverHello，得到 {reply:?}")
    };
    let binding_id = hello.binding_id.clone().expect("binding_id");
    svc.approve(&binding_id).await.expect("用户批准");
    // 批准放行帧真实到达客户端。
    let granted = client.drain().await;
    assert!(
        granted
            .iter()
            .any(|f| matches!(f, ServerFrame::Granted { .. })),
        "批准后客户端应收到放行帧：{granted:?}"
    );
    assert_eq!(svc.authorization_state(), AuthorizationState::Bound);

    // 宿主换代（进程重启：started_at 变 → discover 代际变）。
    b.procs.set_host_started_at(HOST_STARTED_AT + 5_000);
    let epoch2 = current_epoch(&b.env);
    assert_ne!(epoch1, epoch2, "换代必须换代际");

    // 旧代际授权（签名消息里 epoch=E1）再连接 → 拒绝。
    let old_epoch = TestClient {
        keys: RustSigningKey::from_seed(&client.keys.seed()),
        conn: factory.connect().await,
        received: Mutex::new(Vec::new()),
    };
    let outcome = old_epoch.connect(&svc, epoch1, [12u8; 32]).await;
    assert!(outcome.is_err(), "旧代际授权必须被拒，得到 {:?}", outcome);
    assert_eq!(outcome.unwrap_err().code, codes::AUTH_EPOCH_STALE);
}

/// ④ 撤权关受信 Peer：旧连接写不可用 + 新握手被拒；账本无 cookie 泄漏。
#[tokio::test]
async fn revoke_closes_trusted_client_peer() {
    let b = bench();
    let (svc, factory) = served(&b).await;
    let epoch = current_epoch(&b.env);

    // 完整授权流：hello → 批准 → Granted → cookie 交付（受信 Peer 才被接受）。
    let client = TestClient {
        keys: RustSigningKey::generate(),
        conn: factory.connect().await,
        received: Mutex::new(Vec::new()),
    };
    let reply = client
        .connect_frame(&svc, epoch, [13u8; 32])
        .await
        .expect("握手有应答");
    let ServerFrame::ServerHello(hello) = reply else {
        panic!("首绑应得 serverHello，得到 {reply:?}")
    };
    let binding_id = hello.binding_id.clone().expect("binding_id");
    svc.approve(&binding_id).await.expect("批准");
    let granted = client.drain().await;
    assert!(granted
        .iter()
        .any(|f| matches!(f, ServerFrame::Granted { .. })));
    let ack = client
        .send_credential(&binding_id, "owner-cookie-SECRET")
        .await
        .expect("交付有应答");
    let ServerFrame::Granted { .. } = ack else {
        panic!("受信 Peer 的 cookie 交付应被接受，得到 {ack:?}")
    };
    assert!(client.write_available(), "受信连接在撤权前可写");

    // 撤权（服务入口；§2.3 自由函数 auth::revoke 委托生产单例，测试不触生产）。
    svc.revoke(&binding_id).await.expect("撤权受理");

    // 撤权通知真实到达，随后连接关闭：旧 Peer 写不可用（brief 观测面）。
    let revoked = client.drain().await;
    assert!(
        revoked
            .iter()
            .any(|f| matches!(f, ServerFrame::Revoked { .. })),
        "撤权帧：{revoked:?}"
    );
    let old_peer = &client;
    assert!(!old_peer.write_available(), "撤权后旧 Peer 写不可用");

    // 新握手被拒（禁止自动重连；重新绑定必须换新身份走 TOFU）。
    let reconnect = TestClient {
        keys: RustSigningKey::from_seed(&client.keys.seed()),
        conn: factory.connect().await,
        received: Mutex::new(Vec::new()),
    };
    let challenge = BindingChallengeMessage {
        binding_protocol: BINDING_PROTOCOL,
        key_digests: [
            svc.key_digest().to_string(),
            dsh_auth::key_digest(&reconnect.keys.spki),
        ],
        nonce: dsh_auth::b64url_encode(&[14u8; 32]),
        host_id: fixture_host_id(),
        epoch: epoch.to_string(),
        expiry: svc.now_ms() + CHALLENGE_TTL_MS,
        home_binding: fixture_home_binding(WIN_HOME, HOST_PID, HOST_STARTED_AT),
    };
    let refused = reconnect
        .connect_frame(&svc, epoch, [14u8; 32])
        .await
        .expect("撤权重连有签名应答");
    assert_signed_revoked_hello(&refused, &svc, &reconnect.keys, &challenge, &binding_id);
    let delivery = reconnect
        .send_credential(&binding_id, "revoked-cookie")
        .await
        .expect("撤权交付有拒绝应答");
    assert!(
        matches!(&delivery, ServerFrame::Error { error, .. } if error == codes::AUTH_NOT_APPROVED),
        "{delivery:?}"
    );
    assert!(
        !svc.has_delivered_peer(&binding_id),
        "已撤握手不得重新交付凭据"
    );

    // 状态面：Revoked；活跃绑定清空；账本（非秘密面）绝无 cookie 字节。
    assert_eq!(svc.authorization_state(), AuthorizationState::Revoked);
    assert_eq!(svc.active_binding_id(), None);
    let ledger = std::fs::read_to_string(b.env.tuvis_home.join("dsh").join("bindings.json"))
        .unwrap_or_default();
    assert!(
        !ledger.contains("owner-cookie-SECRET"),
        "cookie 不得落盘：{ledger}"
    );
    let _ = client_identity_of(&client);
    let _ = &b.clock;
    let _ = &b.store;
}

// ============================================================
// R04 回归：待批绑定按客户端公钥幂等（同一未批客户端在 TTL 内重复 hello
// 必须拿到**同一个** binding_id 与 display_code）
// ============================================================

/// R04（审查 `2026-10-29-dsh-desktop-remote-review.md`）：插件每 3 秒重发一次
/// hello（`packages/dsh-remote-access/src/index.ts:53,209`），而服务端
/// `auth.rs:882-900` 每次首绑都 `next_binding_id()` + 新 `display_code()` 并覆盖
/// `pending` ⇒ 两端核对码在人工核对的窗口里一直翻动，用户点批准时批准的是下一轮
/// hello 早已替换掉的 bindingId → `DSH_AUTH_UNKNOWN_BINDING`（批准必然失败）。
///
/// 契约（与 group 3 钉定的 wire 契约）：**未批准**客户端出示**同一把客户端公钥**
/// 且其上一份 pending 挑战**未过期**时，hello 必须返回**同一** binding_id +
/// display_code（不重新生成）；只有到期（60 秒 TTL 不变）才换新。
#[tokio::test]
async fn pending_binding_is_stable_for_the_same_client_until_expiry() {
    let b = bench();
    let (svc, factory) = served(&b).await;
    let epoch = current_epoch(&b.env);

    let keys = RustSigningKey::generate();
    // 第一帧：新公钥 → pending_approval。
    let first = TestClient {
        keys: RustSigningKey::from_seed(&keys.seed()),
        conn: factory.connect().await,
        received: Mutex::new(Vec::new()),
    };
    let reply = first
        .connect_frame(&svc, epoch, [21u8; 32])
        .await
        .expect("首次握手有应答");
    let ServerFrame::ServerHello(hello1) = reply else {
        panic!("首绑应得 serverHello，得到 {reply:?}")
    };
    assert_eq!(hello1.state, "pending_approval");
    let binding_id1 = hello1.binding_id.clone().expect("pending 必带 binding_id");
    let display_code1 = hello1
        .display_code
        .clone()
        .expect("pending 必带 display_code");

    // 第二次 hello（同一把公钥、新的 nonce/expiry，仍在 TTL 内、新连接——
    // 与插件 3 秒重连同形）：必须拿回**同一对** binding_id + display_code。
    let second = TestClient {
        keys: RustSigningKey::from_seed(&keys.seed()),
        conn: factory.connect().await,
        received: Mutex::new(Vec::new()),
    };
    let reply2 = second
        .connect_frame(&svc, epoch, [22u8; 32])
        .await
        .expect("重发握手有应答");
    let ServerFrame::ServerHello(hello2) = reply2 else {
        panic!("同钥重发应得 serverHello，得到 {reply2:?}")
    };
    assert_eq!(hello2.state, "pending_approval");
    assert_eq!(
        hello2.binding_id.as_deref(),
        Some(binding_id1.as_str()),
        "未批准期间同一客户端公钥的 binding_id 必须保持不变（人工核对窗口）"
    );
    assert_eq!(
        hello2.display_code.as_deref(),
        Some(display_code1.as_str()),
        "未批准期间同一客户端公钥的 display_code 必须保持不变（两端核对码不翻动）"
    );

    // 设置卡读到的待批视图也必须是同一对（用户点批准时批的就是屏幕上的那个）。
    let pending = svc.pending_binding().expect("待批绑定在场");
    assert_eq!(pending.binding.binding_id, binding_id1);
    assert_eq!(pending.binding.display_code, display_code1);

    // 人工核对完成后，用设置卡上的 binding_id 批准 → 必须成功（不是 UNKNOWN_BINDING）。
    svc.approve(&binding_id1)
        .await
        .expect("屏幕上的 binding_id 在重复 hello 之后仍可批准");
    assert_eq!(svc.authorization_state(), AuthorizationState::Bound);

    // 批准后该对已被消费：同一公钥再 hello 走 authorized 分支，不再产生 pending。
    let third = TestClient {
        keys: RustSigningKey::from_seed(&keys.seed()),
        conn: factory.connect().await,
        received: Mutex::new(Vec::new()),
    };
    let reply3 = third
        .connect_frame(&svc, epoch, [23u8; 32])
        .await
        .expect("批准后握手有应答");
    let ServerFrame::ServerHello(hello3) = reply3 else {
        panic!("已批准公钥应得 serverHello，得到 {reply3:?}")
    };
    assert_eq!(hello3.state, "authorized");
    assert_eq!(hello3.binding_id.as_deref(), Some(binding_id1.as_str()));
}

/// R04 的第二半：到期（60 秒 TTL）后同一客户端公钥必须换新的一对；不同客户端
/// 公钥各持有自己的一对（不同 key 不得共用/覆盖同一待批记录）。
#[tokio::test]
async fn pending_binding_regenerates_after_expiry_and_is_per_key() {
    let b = bench();
    let (svc, factory) = served(&b).await;
    let epoch = current_epoch(&b.env);

    let keys = RustSigningKey::generate();
    let first = TestClient {
        keys: RustSigningKey::from_seed(&keys.seed()),
        conn: factory.connect().await,
        received: Mutex::new(Vec::new()),
    };
    let reply = first
        .connect_frame(&svc, epoch, [31u8; 32])
        .await
        .expect("首次握手有应答");
    let ServerFrame::ServerHello(hello1) = reply else {
        panic!("首绑应得 serverHello，得到 {reply:?}")
    };
    let binding_id1 = hello1.binding_id.clone().expect("binding_id");
    let display_code1 = hello1.display_code.clone().expect("display_code");

    // 另一把客户端公钥：必须拿到自己的一对（不是覆盖/复用第一把的）。
    let other_keys = RustSigningKey::generate();
    let other = TestClient {
        keys: RustSigningKey::from_seed(&other_keys.seed()),
        conn: factory.connect().await,
        received: Mutex::new(Vec::new()),
    };
    let reply_other = other
        .connect_frame(&svc, epoch, [32u8; 32])
        .await
        .expect("异钥握手有应答");
    let ServerFrame::ServerHello(hello_other) = reply_other else {
        panic!("异钥首绑应得 serverHello，得到 {reply_other:?}")
    };
    assert_ne!(
        hello_other.binding_id.as_deref(),
        Some(binding_id1.as_str()),
        "不同客户端公钥必须各有独立 binding_id"
    );
    assert_ne!(
        hello_other.display_code.as_deref(),
        Some(display_code1.as_str()),
        "不同客户端公钥必须各有独立 display_code"
    );

    // 推进注入时钟越过 60 秒 TTL → 同一公钥再 hello 必须换新的一对。
    let expired_at = b.clock.now_ms() + CHALLENGE_TTL_MS + 1_000;
    b.clock.set(expired_at);
    let after = TestClient {
        keys: RustSigningKey::from_seed(&keys.seed()),
        conn: factory.connect().await,
        received: Mutex::new(Vec::new()),
    };
    let reply_after = after
        .connect_frame(&svc, epoch, [33u8; 32])
        .await
        .expect("到期后握手有应答");
    let ServerFrame::ServerHello(hello_after) = reply_after else {
        panic!("到期后应得 serverHello，得到 {reply_after:?}")
    };
    assert_eq!(hello_after.state, "pending_approval");
    assert_ne!(
        hello_after.binding_id.as_deref(),
        Some(binding_id1.as_str()),
        "过期后同一公钥必须换新 binding_id"
    );
    assert_ne!(
        hello_after.display_code.as_deref(),
        Some(display_code1.as_str()),
        "过期后同一公钥必须换新 display_code"
    );

    // 旧的那一对已被到期回收：用它批准必须失败（不假成功）。
    let stale = svc.approve(&binding_id1).await;
    assert!(stale.is_err(), "过期挑战不得还能批准");
    assert_eq!(stale.unwrap_err().code, codes::AUTH_UNKNOWN_BINDING);
}

// ============================================================
// R06 回归：授权句柄必须绑定真实 authority 与代际（否则真实 transport 拒连）
// ============================================================

/// 起一台**真实**回环假宿主（axum `127.0.0.1:<临时端口>`），只提供
/// `GET /api/remote.mux` 的 WS 升级（升完即挂住，不主动关）。
///
/// 为什么要真服务器：R06 的缺陷只有**真实** `transport::connect` 才看得见——
/// 它先校验 `peer.authority()` 非空、再对 authority 发起 WS 升级。内存假连接
/// 永远绕过这条路径，正是审查 R06 记录的问题面。
async fn spawn_fake_mux_host() -> String {
    use axum::extract::ws::WebSocketUpgrade;
    use axum::routing::get;
    use axum::Router;

    async fn mux(ws: WebSocketUpgrade) -> axum::response::Response {
        ws.on_upgrade(|mut sock| async move {
            // 挂住直到客户端关闭：本用例只验证「能连上」，不需要业务帧。
            while sock.recv().await.is_some() {}
        })
    }

    let app = Router::new().route("/api/remote.mux", get(mux));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind loopback fake host");
    let port = listener.local_addr().expect("local addr").port();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    format!("127.0.0.1:{port}")
}

/// R06（审查 R06 条目 / Mac 补探测 §7）：`auth.rs:930` 曾用
/// `AuthorizedPeer::new("local", …)` 造句柄——authority 为空、epoch 0，而
/// `transport.rs:137-143` 对空 authority 直接 `DSH_CONNECT_FAILED`。链路因此在
/// connect 处必断：授权成功也永远进不了同 Desktop 的 HTTP/Mux。
///
/// 契约：`authorize` 必须**在收到 cookie 的那一点**绑定 discover 的宿主事实——
/// authority 逐字 = 发现到的 LocalHost 回环 authority（`127.0.0.1:<port>`），
/// epoch = 发现到的代际。回归断言：句柄经**真实** `transport::connect` 对真实
/// 回环假宿主升级成功（空 authority 的 CONNECT_FAILED 路径不再被真实句柄命中）。
#[tokio::test]
async fn authorized_peer_binds_discovered_authority_and_epoch() {
    let b = bench();
    // 假宿主先起，再把它的 authority 注入 discover 的监听源（真实端口，非硬编码）。
    let authority = spawn_fake_mux_host().await;
    *b.procs.listeners.lock().unwrap() = vec![DshListener {
        owner_pid: HOST_PID,
        authority: authority.clone(),
    }];
    let (svc, factory) = served(&b).await;
    let epoch = current_epoch(&b.env);

    // 完整授权流：hello → 批准 → 凭据交付。
    let client = TestClient {
        keys: RustSigningKey::generate(),
        conn: factory.connect().await,
        received: Mutex::new(Vec::new()),
    };
    let reply = client
        .connect_frame(&svc, epoch, [41u8; 32])
        .await
        .expect("握手有应答");
    let ServerFrame::ServerHello(hello) = reply else {
        panic!("首绑应得 serverHello，得到 {reply:?}")
    };
    let binding_id = hello.binding_id.clone().expect("binding_id");
    svc.approve(&binding_id).await.expect("批准");
    // 排空批准推来的 Granted（否则它会冒充下面凭据交付的应答）。
    let granted = client.drain().await;
    assert!(
        granted
            .iter()
            .any(|f| matches!(f, ServerFrame::Granted { .. })),
        "批准放行帧：{granted:?}"
    );
    let ack = client
        .send_credential(&binding_id, "dsh-auth-testfake=cookie-value")
        .await
        .expect("有应答");
    assert!(
        matches!(ack, ServerFrame::Granted { .. }),
        "受信 Peer 交付被接受：{ack:?}"
    );

    // 生产入口：`authorize_grant`（§2.3 `auth::authorize` 的服务内实现）。
    let host = identity::discover(&b.env).expect("宿主在跑");
    let peer = svc
        .authorize_grant(&host, &client_identity_of(&client))
        .await
        .expect("已授权客户端可取到句柄");

    // 单元断言：authority/epoch 必须是发现到的真实值（非空/非零），且逐字相等。
    assert_eq!(
        peer.authority(),
        authority.as_str(),
        "authority 必须逐字 = 发现的回环监听"
    );
    assert!(
        peer.authority().starts_with("127.0.0.1:"),
        "authority 必须是回环字面量：{}",
        peer.authority()
    );
    assert_eq!(peer.epoch(), epoch, "epoch 必须是发现到的宿主代际");
    assert_ne!(peer.epoch(), 0, "epoch 不得为 0");
    assert!(!peer.authority().is_empty(), "authority 不得为空");

    // **真实** transport 回归：该句柄对真实回环假宿主能完成 WS 升级。
    let conn = dsh_transport::connect_with(
        peer,
        dsh_transport::TransportConfig {
            unary_timeout: std::time::Duration::from_secs(2),
        },
    )
    .await
    .expect("真实句柄必须能连上（R06 的空 authority 路径不再命中）");
    assert!(conn.write_available(), "升级成功的连接可写");
    conn.close().await;
}

/// R06 的否定面：**未交付 cookie** 的客户端拿不到句柄（不凭空造 authority）。
#[tokio::test]
async fn authorize_without_delivered_credential_still_refuses() {
    let b = bench();
    let authority = spawn_fake_mux_host().await;
    *b.procs.listeners.lock().unwrap() = vec![DshListener {
        owner_pid: HOST_PID,
        authority: authority.clone(),
    }];
    let (svc, factory) = served(&b).await;
    let epoch = current_epoch(&b.env);

    let client = TestClient {
        keys: RustSigningKey::generate(),
        conn: factory.connect().await,
        received: Mutex::new(Vec::new()),
    };
    let reply = client
        .connect_frame(&svc, epoch, [42u8; 32])
        .await
        .expect("握手有应答");
    let ServerFrame::ServerHello(hello) = reply else {
        panic!("首绑应得 serverHello，得到 {reply:?}")
    };
    let binding_id = hello.binding_id.clone().expect("binding_id");
    svc.approve(&binding_id).await.expect("批准");

    let host = identity::discover(&b.env).expect("宿主在跑");
    let err = svc
        .authorize_grant(&host, &client_identity_of(&client))
        .await
        .expect_err("无凭据不得发句柄");
    assert_eq!(err.code, codes::AUTH_NOT_APPROVED);
}

// ============================================================
// H12：换代与凭据迟到的端到端（宿主重启 / 迟到 cookie 不得复活权限）
// ============================================================

/// H12-a：**宿主换代后迟到的凭据不得复活旧代际授权**。
///
/// 时序：首绑 + 批准 → 宿主换代 → 旧代际 Peer 用旧 epoch 重新交付 cookie。
/// 期望：交付被拒（不是「先收下再说」），且不产生可用句柄。
#[tokio::test]
async fn late_credential_across_epoch_cannot_revive_old_grant() {
    let b = bench();
    let (svc, factory) = served(&b).await;
    let epoch1 = current_epoch(&b.env);

    let client = TestClient {
        keys: RustSigningKey::generate(),
        conn: factory.connect().await,
        received: Mutex::new(Vec::new()),
    };
    let reply = client
        .connect_frame(&svc, epoch1, [61u8; 32])
        .await
        .expect("握手有应答");
    let ServerFrame::ServerHello(hello) = reply else {
        panic!("首绑应得 serverHello，得到 {reply:?}")
    };
    let binding_id = hello.binding_id.clone().expect("binding_id");
    svc.approve(&binding_id).await.expect("用户批准");

    // 宿主换代（进程重启 → 代际变）。
    b.procs.set_host_started_at(HOST_STARTED_AT + 7_000);
    let epoch2 = current_epoch(&b.env);
    assert_ne!(epoch1, epoch2, "换代必须换代际");

    // 先消费批准时推来的 `Granted` 通知。不消费的话，下面的 `send_credential`
    // 会读到这条**残留帧**并把它当成交付回执——用例就永远到不了代际检查点。
    let granted = client.drain().await;
    assert!(
        granted
            .iter()
            .any(|f| matches!(f, ServerFrame::Granted { .. })),
        "批准通知必须真实到达（否则后续断言无意义）：{granted:?}"
    );

    // 同一 Peer（握手仍在、已被批准进入 trusted）在换代**之后**才交付 cookie。
    // 刻意不新建连接：新连接会在 accept_credential 之前就因「missing authenticated
    // handshake」被拒，那样测不到代际检查本身。
    let delivery = client
        .send_credential(&binding_id, "late-epoch-cookie")
        .await;
    // 必须是被**代际**拒绝，而不是任意错误（否则删掉代际比较也会绿）。
    match &delivery {
        Ok(ServerFrame::Error { error, .. }) => assert_eq!(
            *error,
            codes::AUTH_EPOCH_STALE,
            "必须因代际不符被拒，实际错误码 {error}"
        ),
        other => panic!("换代后的迟到凭据必须被拒，实际 {other:?}"),
    }
    assert!(
        !svc.has_delivered_peer(&binding_id),
        "迟到凭据不得让旧代际重新变成已交付"
    );

    // 新代际仍需重新走受控握手（不能被旧账本直接放行）。
    let probe = TestClient {
        keys: RustSigningKey::from_seed(&client.keys.seed()),
        conn: factory.connect().await,
        received: Mutex::new(Vec::new()),
    };
    let outcome = probe.connect(&svc, epoch1, [62u8; 32]).await;
    assert!(outcome.is_err(), "旧代际握手必须持续被拒");
    assert_eq!(outcome.unwrap_err().code, codes::AUTH_EPOCH_STALE);
}

// ============================================================
// R08 回归（auth 侧）：撤权必须贯穿到传输层——已交付的句柄连不上，活连接被关
// ============================================================

/// R08 的端到端面（真实 UDS 之外的传输面）：`svc.revoke` 之后，
/// ① 该 binding 的闸门已翻转 → `authorize_grant` 直接拒发句柄（`DSH_PEER_REVOKED`）；
/// ② 撤权前用该句柄建起来的**真实**连接被 socket 层关闭（`write_available` 翻转）；
/// ③ 用撤权前拿到的旧句柄重连 → 也拒绝（句柄自身携带同一闸门）。
#[tokio::test]
async fn revoke_propagates_into_transport_for_delivered_handle() {
    let b = bench();
    let authority = spawn_fake_mux_host().await;
    *b.procs.listeners.lock().unwrap() = vec![DshListener {
        owner_pid: HOST_PID,
        authority: authority.clone(),
    }];
    let (svc, factory) = served(&b).await;
    let epoch = current_epoch(&b.env);

    // 完整授权：hello → 批准 → 凭据交付 → authorize_grant 拿句柄。
    let client = TestClient {
        keys: RustSigningKey::generate(),
        conn: factory.connect().await,
        received: Mutex::new(Vec::new()),
    };
    let reply = client
        .connect_frame(&svc, epoch, [51u8; 32])
        .await
        .expect("握手有应答");
    let ServerFrame::ServerHello(hello) = reply else {
        panic!("首绑应得 serverHello，得到 {reply:?}")
    };
    let binding_id = hello.binding_id.clone().expect("binding_id");
    svc.approve(&binding_id).await.expect("批准");
    let _ = client.drain().await;
    let ack = client
        .send_credential(&binding_id, "dsh-auth-testfake=cookie-value")
        .await
        .expect("有应答");
    assert!(
        matches!(ack, ServerFrame::Granted { .. }),
        "凭据被接受：{ack:?}"
    );

    let host = identity::discover(&b.env).expect("宿主在跑");
    let peer = svc
        .authorize_grant(&host, &client_identity_of(&client))
        .await
        .expect("已授权客户端可取到句柄");
    assert!(peer.gate().is_some(), "生产路径的句柄必须携带撤权闸门");

    // 用该句柄建真实连接（闸门登记这条活连接）。
    let conn = dsh_transport::connect_with(
        peer.clone(),
        dsh_transport::TransportConfig {
            unary_timeout: std::time::Duration::from_secs(2),
        },
    )
    .await
    .expect("撤权前可连");
    assert!(conn.write_available(), "撤权前可写");

    // 撤权（生产入口 `svc.revoke`）。
    svc.revoke(&binding_id).await.expect("撤权受理");

    // ② 活连接被 socket 层关闭。
    assert!(!conn.write_available(), "撤权必须在传输层关掉这条活连接");
    let err = conn
        .request("r-after", "session/list", &serde_json::json!({}))
        .await
        .expect_err("撤权后旧连接请求必须失败");
    assert_eq!(err.code, codes::CONNECTION_CLOSED, "{}", err.detail);

    // ① 撤权后 authorize_grant 拒发句柄。
    let err2 = svc
        .authorize_grant(&host, &client_identity_of(&client))
        .await
        .expect_err("撤权后不得再发句柄");
    assert_eq!(err2.code, codes::PEER_REVOKED, "{}", err2.detail);

    // ③ 用撤权前拿到的旧句柄重连 → 拒绝（同一闸门已 flipped）。
    let err3 = dsh_transport::connect_with(
        peer,
        dsh_transport::TransportConfig {
            unary_timeout: std::time::Duration::from_secs(2),
        },
    )
    .await
    .expect_err("撤权后旧句柄重连必须被拒");
    assert_eq!(err3.code, codes::PEER_REVOKED, "{}", err3.detail);
}

// ============================================================
// R07 回归：真实 UDS 的在飞读必须能被 close() 解除，且对端收到 EOF
// ============================================================
//
// Mac 补探测报告 §5（S2 复刻产品 close()：recv 未解除、对端收不到 EOF；S3
// SHUT_RD 只解本端；S4/S5 SHUT_WR/RDWR 解本端在飞读**并**送达对端 EOF；
// `into_split()` 之后拿不到整条 socket ⇒ 必须留 dup fd / std 句柄；清理路径
// 不得与在飞读共用同一把互斥锁）。
//
// 审查的额外约束（R07 非机械可泛化那条）：**不得**把探针里「对端配合退出」时
// 观察到的 SHUT_WR 效果，泛化成独立取消保证——所以第二个用例是**不配合对端**
// （看到 EOF 后仍存活、写半区不关，甚至继续写更多字节），本端在飞读仍必须解除、
// 本端必须以 closed 终结。
//
// 全部用例走真实 UDS（临时目录下的 unix socket，路径长度遵守 group 1 的 103
// 字节上限），不用内存假连接——R07 的锁等待只有真实套接字才看得见。

#[cfg(unix)]
mod uds_close {
    use super::*;
    use multi_agents_manager_lib::services::dsh_control::local_ipc::{
        production_listener, IpcClientConnection, IpcListener,
    };
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::UnixStream;

    /// 起一个真实 UDS 监听端（临时目录；路径长度必须在 AF_UNIX 上限内）。
    async fn bound_listener() -> (tempfile::TempDir, Box<dyn IpcListener>, String) {
        let tmp = tempfile::tempdir().expect("tempdir 可建");
        let state_dir = tmp.path().join("dsh");
        assert!(
            state_dir.to_string_lossy().len() + "/ipc-0123456789abcdef.sock".len() <= 103,
            "夹具路径必须在 sun_path 上限内：{} 字节",
            state_dir.to_string_lossy().len()
        );
        let listener = production_listener(state_dir);
        let path = listener.bind().await.expect("真实 UDS bind");
        (tmp, listener, path)
    }

    /// 服务端 accept 一个连接（真实 UDS），并取走客户端一侧的流。
    async fn accepted_pair() -> (
        tempfile::TempDir,
        Box<dyn IpcListener>,
        Arc<dyn IpcClientConnection>,
        UnixStream,
    ) {
        let (tmp, listener, path) = bound_listener().await;
        let client = UnixStream::connect(&path).await.expect("客户端连上 UDS");
        let server = listener.accept().await.expect("服务端 accept");
        (tmp, listener, Arc::from(server), client)
    }

    /// ① 空闲对端 + 在飞 recv → `close()` → recv 在有界窗口内解除，且对端看到 EOF。
    ///
    /// 旧实现（`local_ipc.rs:388` recv 持 reader 锁跨 `read_line` await；
    /// `close()` 先抢同一把锁再 shutdown）在此必然超时：teardown 永远拿不到锁。
    #[tokio::test]
    async fn close_unblocks_in_flight_recv_on_idle_peer_and_sends_eof() {
        let (_tmp, _listener, server, mut client) = accepted_pair().await;

        // 在飞 recv（对端空闲，不写任何字节 —— 正是旧实现卡死的场景）。
        let recv_task = {
            let server = Arc::clone(&server);
            tokio::spawn(async move { server.recv().await })
        };
        // 给它一点时间真的进入 read_line（确定性：不做时序假设，只放宽窗口）。
        tokio::time::sleep(Duration::from_millis(120)).await;

        let closer = Arc::clone(&server);
        closer.close();

        // 有界等待：recv 必须自己在窗口内返回 Err（关闭即终结），而不是悬挂。
        let outcome = tokio::time::timeout(Duration::from_secs(2), recv_task)
            .await
            .expect("R07：close() 必须解除在飞 recv（不得悬挂）")
            .expect("recv 任务未 panic");
        assert!(
            outcome.is_err(),
            "关闭后 recv 必须以 Err 终结，得到 {outcome:?}"
        );
        assert!(server.is_closed(), "关闭旗标语义保留");

        // 对端必须收到 EOF（读到 0 字节），且在有界窗口内。
        let mut buf = [0u8; 64];
        let n = tokio::time::timeout(Duration::from_secs(2), client.read(&mut buf))
            .await
            .expect("R07：对端必须在窗口内看到 EOF")
            .expect("对端读不报错");
        assert_eq!(n, 0, "对端读到 EOF（0 字节），不是悬挂也不是残留数据");
    }

    /// ② **不配合对端**（审查强制用例）：对端进程/流保持存活、写半区不关，
    /// 在服务端 close 之后**继续写更多字节**；本端在飞读仍必须解除，本端后续
    /// 操作一律报 closed。
    ///
    /// 这条用例刻意不复用探针「对端看到 EOF 就退出」的配合行为——那会把
    /// 「单次 SHUT_WR 恰好让配合对端退出」误当成独立取消保证。
    #[tokio::test]
    async fn close_unblocks_in_flight_recv_even_when_peer_keeps_writing() {
        let (_tmp, _listener, server, mut client) = accepted_pair().await;

        let recv_task = {
            let server = Arc::clone(&server);
            tokio::spawn(async move { server.recv().await })
        };
        tokio::time::sleep(Duration::from_millis(120)).await;

        let closer = Arc::clone(&server);
        closer.close();

        // 不配合：关闭之后继续猛写（对端写半区保持打开）。写失败也无所谓——
        // 关键是**本端**的在飞读不得因此悬挂，也不得被这些字节续命。
        let peer_writer = tokio::spawn(async move {
            for _ in 0..64 {
                if client.write_all(b"{\"type\":\"hello\"}\n").await.is_err() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            // 故意不 shutdown / 不 drop 得太早：保持写半区直到本端已经解除。
            client
        });

        let outcome = tokio::time::timeout(Duration::from_secs(2), recv_task)
            .await
            .expect("R07：不配合对端也不得让在飞读悬挂")
            .expect("recv 任务未 panic");
        assert!(
            outcome.is_err(),
            "关闭后 recv 必须以 Err 终结，得到 {outcome:?}"
        );

        // 本端后续操作一律报 closed（不假装还活着）。
        assert!(server.is_closed());
        let after_recv = server.recv().await;
        assert!(after_recv.is_err(), "关闭后的 recv 必须直接报错");
        let after_send = server.send("{\"type\":\"error\"}").await;
        assert!(after_send.is_err(), "关闭后的 send 必须直接报错");

        // 不配合对端最终照样能观察到 EOF（服务端 SHUT_WR/RDWR 的面向对端效应）。
        let mut client = peer_writer.await.expect("写任务未 panic");
        let mut buf = [0u8; 64];
        let saw_eof = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                match client.read(&mut buf).await {
                    Ok(0) => return true,
                    Ok(_) => continue, // 服务端可能还有残留写；继续读到 EOF
                    Err(_) => return false,
                }
            }
        })
        .await
        .expect("R07：不配合对端也必须在窗口内看到 EOF");
        assert!(saw_eof, "服务端必须向对端送达 EOF（即使对端一直在写）");
    }

    /// ③ close 幂等：第二次 close 不 panic、不改变结果（重复调用是正常路径：
    /// 撤权 + 卸载都要关同一条连接）。
    #[tokio::test]
    async fn close_is_idempotent() {
        let (_tmp, _listener, server, _client) = accepted_pair().await;
        Arc::clone(&server).close();
        Arc::clone(&server).close();
        Arc::clone(&server).close();
        assert!(server.is_closed());
        assert!(server.recv().await.is_err());
        assert!(server.send("x").await.is_err());
    }

    /// ④ 反面控制：未关闭时 recv 能正常读到一帧（保证上面的解除不是「一直坏」）。
    #[tokio::test]
    async fn recv_still_delivers_frames_while_open() {
        let (_tmp, _listener, server, mut client) = accepted_pair().await;
        client
            .write_all(b"{\"type\":\"hello\"}\n")
            .await
            .expect("对端写一帧");
        let line = tokio::time::timeout(Duration::from_secs(2), server.recv())
            .await
            .expect("窗口内")
            .expect("帧可读");
        assert_eq!(line, "{\"type\":\"hello\"}");
        assert!(!server.is_closed());
    }
}

// ============================================================
// N4 回归：本机套接字路径长度有界且可诊断
// ============================================================

/// Mac 补探测报告 §5 P3 / §6 第 7 条：AF_UNIX `sun_path` 实测上限 **103 字节**，
/// 套接字名 `ipc-<16hex>.sock` 占 25 字节 ⇒ state_dir 必须 ≤ 78 字节。
///
/// 契约：绑定前就要给出专用错误码 `DSH_STATE_PATH_TOO_LONG`（不是裸的
/// `bind ... path must be shorter than SUN_LEN`），且文案指向可换掉的
/// `TUVIS_DSH_STATE_DIR`，让用户知道该动哪个旋钮。
#[cfg(unix)]
#[tokio::test]
async fn deep_state_dir_fails_with_dedicated_path_error() {
    // 深影子目录：state_dir = <deep>/dsh，加 25 字节套接字名后必然 > 103。
    let tmp = tempfile::tempdir().expect("tempdir 可建");
    let mut deep = tmp.path().to_path_buf();
    for _ in 0..6 {
        deep = deep.join("tuvis-dsh-shadow-very-long-directory-name");
    }
    let tuvis_home = deep.join("tuvis-home");
    let state_dir = tuvis_home.join("dsh");
    assert!(
        state_dir.to_string_lossy().len() + "/ipc-0123456789abcdef.sock".len() > 103,
        "夹具必须越过 sun_path 上限：{} 字节",
        state_dir.to_string_lossy().len()
    );

    let b = bench();
    // 复用 bench 的假进程/文件源，只把 TUVIS_HOME 换成深目录。
    let env = DshEnvironment {
        home: b.env.home.clone(),
        tuvis_home: tuvis_home.clone(),
        dsh_state_dir: b.env.dsh_state_dir.clone(),
        processes: b.env.processes.clone(),
        files: b.env.files.clone(),
        runner: b.env.runner.clone(),
        clock: b.env.clock.clone(),
        // 测试默认「不可观测」：显式拒绝 Web home 归属（fail-closed），
        // 绝不让假进程源落到真实系统 PID 上。需要 Web 归属的用例自行注入假观测器。
        environments: std::sync::Arc::new(
            multi_agents_manager_lib::services::dsh_control::model::TestUnobservableEnvironments,
        ),
    };
    let svc = dsh_auth::DshAuthService::new(env).expect("auth 服务可建");
    // 不注入内存传输 ⇒ 走生产 UDS 监听端（真实 bind 语义）。
    let err = Arc::new(svc)
        .serve()
        .await
        .expect_err("深目录必须失败而不是静默");
    assert_eq!(
        err.code,
        codes::STATE_PATH_TOO_LONG,
        "必须是专用错误码，不是裸 bind 失败：{err}"
    );
    assert!(
        err.detail.contains("TUVIS_DSH_STATE_DIR"),
        "文案必须指出可换的变量名（用户可操作）：{}",
        err.detail
    );
    assert!(
        err.detail.contains("103"),
        "文案必须给出上限数值（可诊断）：{}",
        err.detail
    );
}

/// 正面控制：正常长度的 state_dir 不受该策略影响（策略不得误伤常规路径）。
#[cfg(unix)]
#[tokio::test]
async fn normal_state_dir_binds_fine() {
    let b = bench();
    let state_dir = b.env.tuvis_home.join("dsh");
    assert!(
        state_dir.to_string_lossy().len() + "/ipc-0123456789abcdef.sock".len() <= 103,
        "tempdir 形态必须在界内：{} 字节",
        state_dir.to_string_lossy().len()
    );
    let svc = Arc::new(dsh_auth::DshAuthService::new(clone_env(&b)).expect("auth 服务可建"));
    svc.clone().serve().await.expect("常规路径必须能绑定");
    // 端点观测面 = 生产的 rendezvous 落盘（不新增测试专用 API）。
    let rendezvous = state_dir.join("rendezvous.json");
    let raw = std::fs::read_to_string(&rendezvous).expect("绑定后 rendezvous 必须落盘");
    let value: serde_json::Value = serde_json::from_str(&raw).expect("rendezvous 是 JSON");
    let address = value["address"].as_str().expect("rendezvous 含 address");
    assert!(
        address.ends_with(".sock"),
        "端点应为 UDS 套接字路径：{address}"
    );
}

// ============================================================
// R10 回归：状态目录覆盖变量（TUVIS_DSH_STATE_DIR，两端同义）
// ============================================================

/// 覆盖变量已设时，授权面**全部**落盘都进那个目录本身（不再拼 `<tuvis_home>/dsh`）：
/// `authorization.key`（构造时写）、`bindings.json`/`rendezvous.json`（绑定后写）。
///
/// 这条用真文件系统（tempdir）走生产构造与 `serve`，所以它证明的是**接线**，
/// 不是纯函数——纯解析的四支另有 `production::tests` 单测。
#[cfg(unix)]
#[tokio::test]
async fn state_dir_override_is_the_actual_state_directory() {
    let b = bench();
    let override_dir = b._tmp.path().join("dsh-state");
    let mut env = clone_env(&b);
    env.dsh_state_dir = Some(override_dir.clone());

    let svc = Arc::new(dsh_auth::DshAuthService::new(env).expect("auth 服务可建"));
    // 构造期就已写下授权私钥（密钥落点同样受覆盖约束）。
    assert!(
        override_dir.join("authorization.key").exists(),
        "authorization.key 必须落在覆盖目录本身"
    );
    let default_dir = b.env.tuvis_home.join("dsh");
    assert!(
        !default_dir.join("authorization.key").exists(),
        "覆盖生效时不得再往缺省目录写"
    );

    svc.clone().serve().await.expect("覆盖目录必须能绑定");
    assert!(
        override_dir.join("rendezvous.json").exists(),
        "rendezvous 必须落在覆盖目录（插件从同一目录读）"
    );
    assert!(
        !default_dir.exists(),
        "缺省目录不应被创建：{}",
        default_dir.display()
    );

    let raw = std::fs::read_to_string(override_dir.join("rendezvous.json")).expect("可读");
    let value: serde_json::Value = serde_json::from_str(&raw).expect("rendezvous 是 JSON");
    let address = value["address"].as_str().expect("rendezvous 含 address");
    assert!(
        address.starts_with(&override_dir.to_string_lossy().to_string()),
        "套接字也必须建在覆盖目录内：{address}"
    );
}

/// 对照：未设覆盖变量时行为逐字不变（`<tuvis_home>/dsh`）。
#[tokio::test]
async fn unset_override_keeps_default_state_directory() {
    let b = bench();
    let mut env = clone_env(&b);
    env.dsh_state_dir = None;
    let _svc = dsh_auth::DshAuthService::new(env).expect("auth 服务可建");
    assert!(
        b.env
            .tuvis_home
            .join("dsh")
            .join("authorization.key")
            .exists(),
        "未设覆盖时必须沿用 <tuvis_home>/dsh"
    );
}

// ============================================================
// K1 回归（2026-10-29 修复批 I-1）：撤权判定收敛到**本绑定**的闸门
// ============================================================
//
// 缺陷：`authorize_grant` 的撤权判定写成 `inner.gates.values().find(|g| g.is_revoked())`
// ——`gates` 条目**永不删除**（R08 刻意保留永久 revoked 闸门），于是任意一次撤权都会
// 毒化**此后全部**授权请求：另一台刚批准并交付凭据的客户端也拿 `DSH_PEER_REVOKED`，
// 直到进程重启。
//
// 契约：按客户端公钥在账本里解析**调用方自己的** Bound 行（行携带 binding_id），
// 只查 `gates.get(binding_id)`；撤权墓碑同钥仍报 `DSH_PEER_REVOKED`（插件侧靠这个码
// 区分「等批准」与「已撤销」）。

/// 账本路径（`<tuvis_home>/dsh/bindings.json`，非秘密面）。
fn ledger_file(env: &DshEnvironment) -> PathBuf {
    env.tuvis_home.join("dsh").join("bindings.json")
}

/// 完整授权流（hello → 批准 → 凭据交付），返回 binding_id。
async fn bind_and_deliver(
    svc: &Arc<dsh_auth::DshAuthService>,
    factory: &Arc<MemClientFactory>,
    keys: &RustSigningKey,
    epoch: u64,
    nonce: [u8; 32],
) -> String {
    bind_and_deliver_with_cookie(
        svc,
        factory,
        keys,
        epoch,
        nonce,
        "dsh-auth-testfake=cookie-value",
    )
    .await
}

/// 同 [`bind_and_deliver`]，但交付**指定** cookie——T09-G 分槽用例需要 A/B 各自
/// 交付不同 cookie 值，才能经生产 `authorize_grant` 断言「各取其是」。
async fn bind_and_deliver_with_cookie(
    svc: &Arc<dsh_auth::DshAuthService>,
    factory: &Arc<MemClientFactory>,
    keys: &RustSigningKey,
    epoch: u64,
    nonce: [u8; 32],
    cookie: &str,
) -> String {
    let client = TestClient {
        keys: RustSigningKey::from_seed(&keys.seed()),
        conn: factory.connect().await,
        received: Mutex::new(Vec::new()),
    };
    let reply = client
        .connect_frame(svc, epoch, nonce)
        .await
        .expect("握手有应答");
    let ServerFrame::ServerHello(hello) = reply else {
        panic!("首绑应得 serverHello，得到 {reply:?}")
    };
    let binding_id = hello.binding_id.clone().expect("binding_id");
    svc.approve(&binding_id).await.expect("用户批准");
    let _ = client.drain().await;
    let ack = client
        .send_credential(&binding_id, cookie)
        .await
        .expect("交付有应答");
    assert!(
        matches!(ack, ServerFrame::Granted { .. }),
        "凭据被接受：{ack:?}"
    );
    binding_id
}

fn host_identity(keys: &RustSigningKey) -> ClientIdentity {
    ClientIdentity {
        public_key_spki: keys.spki.clone(),
        executable: PathBuf::from(format!(r"{WIN_INSTALL_DIR}\DeepSeek Harness.exe")),
        pid: HOST_PID,
        started_at_ms: HOST_STARTED_AT,
    }
}

/// (a) 撤 A 之后，另一个已批准客户端 B 仍能完成授权（旧实现：全表 any → 毒化 B）。
#[tokio::test]
async fn revoking_one_binding_does_not_poison_another_clients_authorization() {
    let b = bench();
    let (svc, factory) = served(&b).await;
    let epoch = current_epoch(&b.env);

    let keys_a = RustSigningKey::generate();
    let binding_a = bind_and_deliver(&svc, &factory, &keys_a, epoch, [61u8; 32]).await;
    svc.revoke(&binding_a).await.expect("撤权 A");

    // B（不同公钥）在 A 撤权**之后**才绑定并交付凭据。
    let keys_b = RustSigningKey::generate();
    let _binding_b = bind_and_deliver(&svc, &factory, &keys_b, epoch, [62u8; 32]).await;

    let host = identity::discover(&b.env).expect("宿主在跑");
    svc.authorize_grant(&host, &host_identity(&keys_b))
        .await
        .expect("撤另一个绑定不得毒化本绑定的授权（I-1）");
}

/// (b) 同一把客户端公钥以**新绑定**重新批准后仍能授权（旧实现：全表 any → 永久
/// 毒化，同一把公钥的旧绑定被撤后新绑定也拿不到授权）。
///
/// 构造：账本预置同一把公钥的两条 Bound 行——`binding-old`（历史绑定，本用例
/// 在**同一服务实例内**经生产 `revoke` 撤它，于是内存里留下一个永久 revoked 闸门）
/// 与 `binding-new`（重新批准后的新绑定，账本最新一条）。撤旧绑定之后按同一把公钥
/// 重新交付凭据，再取授权：旧实现的 `gates.values().any(is_revoked)` 会命中
/// `binding-old` 的闸门而误报 `DSH_PEER_REVOKED`。
#[tokio::test]
async fn same_client_key_rebound_as_new_binding_can_authorize_again() {
    let b = bench();
    let keys = RustSigningKey::generate();
    let spki_b64 = dsh_auth::b64url_encode(&keys.spki);
    let row = |binding_id: &str| {
        serde_json::json!({
            "bindingId": binding_id,
            "publicKeySpki": spki_b64,
            "revokedKeySpki": "",
            "executable": format!(r"{WIN_INSTALL_DIR}\DeepSeek Harness.exe"),
            "pid": HOST_PID,
            "startedAtMs": HOST_STARTED_AT,
            "profileDir": WIN_PROFILE_TOKEN,
            "createdAtMs": HOST_STARTED_AT,
            "state": "bound",
        })
    };
    let ledger = serde_json::json!({ "bindings": [row("binding-old"), row("binding-new")] });
    let path = ledger_file(&b.env);
    std::fs::create_dir_all(path.parent().unwrap()).expect("状态目录可建");
    std::fs::write(&path, serde_json::to_vec_pretty(&ledger).unwrap()).expect("账本可写");

    let (svc, factory) = served(&b).await;
    let epoch = current_epoch(&b.env);

    // ① 撤历史绑定 `binding-old`：它的闸门从此永久 revoked（内存态，服务不重启）。
    svc.revoke("binding-old").await.expect("撤旧绑定");

    // ② 同一把公钥重新交付凭据：钉公钥分支取账本最新一条 Bound 行（binding-new）。
    let client = TestClient {
        keys: RustSigningKey::from_seed(&keys.seed()),
        conn: factory.connect().await,
        received: Mutex::new(Vec::new()),
    };
    let reply = client
        .connect_frame(&svc, epoch, [64u8; 32])
        .await
        .expect("握手有应答");
    let ServerFrame::ServerHello(hello) = reply else {
        panic!("已批准公钥应得 serverHello，得到 {reply:?}")
    };
    assert_eq!(hello.state, "authorized", "已批准公钥应直接放行：{hello:?}");
    assert_eq!(
        hello.binding_id.as_deref(),
        Some("binding-new"),
        "钉公钥取账本最新一条 Bound 行（不是被撤的历史绑定）"
    );
    let _ = client.drain().await;
    let ack = client
        .send_credential("binding-new", "dsh-auth-testfake=cookie-value")
        .await
        .expect("交付有应答");
    assert!(
        matches!(ack, ServerFrame::Granted { .. }),
        "受信 Peer 交付被接受：{ack:?}"
    );

    // ③ 新绑定必须能取到授权（旧实现在此处被 binding-old 的永久闸门毒化）。
    let host = identity::discover(&b.env).expect("宿主在跑");
    svc.authorize_grant(&host, &host_identity(&keys))
        .await
        .expect("同一公钥的新绑定不得继承旧绑定的撤权闸门（I-1）");
}

/// (c) 真正被撤的绑定仍然拿 `DSH_PEER_REVOKED`（防「直接删判定」的过度修正）。
#[tokio::test]
async fn genuinely_revoked_binding_still_gets_peer_revoked() {
    let b = bench();
    let (svc, factory) = served(&b).await;
    let epoch = current_epoch(&b.env);

    // 另有一台无关客户端先在场（确保不是「只有一个闸门」的退化情形）。
    let keys_b = RustSigningKey::generate();
    let _ = bind_and_deliver(&svc, &factory, &keys_b, epoch, [65u8; 32]).await;

    let keys_a = RustSigningKey::generate();
    let binding_a = bind_and_deliver(&svc, &factory, &keys_a, epoch, [66u8; 32]).await;
    svc.revoke(&binding_a).await.expect("撤权 A");

    let host = identity::discover(&b.env).expect("宿主在跑");
    let err = svc
        .authorize_grant(&host, &host_identity(&keys_a))
        .await
        .expect_err("被撤绑定不得再发句柄");
    assert_eq!(err.code, codes::PEER_REVOKED, "{}", err.detail);
}

// ============================================================
// K10 回归（2026-10-29 复审 I-5，迁移至 v2）：墓碑同钥重发 hello 必须返回
// 双签名 `serverHello(state=revoked)`，插件核验后进入永久撤权态。
// ============================================================
//
// 契约：已钉过又撤的钥不得恢复授权，也不得退回可重试的待批状态；
// 真正从未绑定过的钥仍走 pending_approval。撤权决定必须由服务端真实签名，
// 不能把无签名 Error 当成可信撤权通知。
//
// 观测面：真实 `handle_hello` 经 mem 传输的 wire 帧，同时核验挑战和决定签名。
#[tokio::test]
async fn tombstoned_key_rehello_gets_peer_revoked_on_the_wire() {
    let b = bench();
    let (svc, factory) = served(&b).await;
    let epoch = current_epoch(&b.env);

    let keys = RustSigningKey::generate();
    let binding_id = bind_and_deliver(&svc, &factory, &keys, epoch, [67u8; 32]).await;
    svc.revoke(&binding_id).await.expect("撤权");

    // 同钥新连接重发 hello（撤权的真实到达顺序：EOF 后插件重连一次）。
    let reconnect = TestClient {
        keys: RustSigningKey::from_seed(&keys.seed()),
        conn: factory.connect().await,
        received: Mutex::new(Vec::new()),
    };
    let challenge = BindingChallengeMessage {
        binding_protocol: BINDING_PROTOCOL,
        key_digests: [
            svc.key_digest().to_string(),
            dsh_auth::key_digest(&reconnect.keys.spki),
        ],
        nonce: dsh_auth::b64url_encode(&[68u8; 32]),
        host_id: fixture_host_id(),
        epoch: epoch.to_string(),
        expiry: svc.now_ms() + CHALLENGE_TTL_MS,
        home_binding: fixture_home_binding(WIN_HOME, HOST_PID, HOST_STARTED_AT),
    };
    let reply = reconnect
        .connect_frame(&svc, epoch, [68u8; 32])
        .await
        .expect("墓碑同钥的重连须返回双签名撤权应答");
    assert_signed_revoked_hello(&reply, &svc, &reconnect.keys, &challenge, &binding_id);
    assert_eq!(svc.authorization_state(), AuthorizationState::Revoked);
    assert_eq!(svc.active_binding_id(), None, "已撤旧钥不得恢复 Bound");
    assert!(svc.pending_binding().is_none(), "已撤旧钥不得退回待批");
    let delivery = reconnect
        .send_credential(&binding_id, "revoked-cookie")
        .await
        .expect("撤权交付有拒绝应答");
    assert!(
        matches!(&delivery, ServerFrame::Error { error, .. } if error == codes::AUTH_NOT_APPROVED),
        "{delivery:?}"
    );
    assert!(!svc.has_delivered_peer(&binding_id), "已撤旧钥不得交付凭据");

    // 对照：真正从未绑定过的钥仍是可恢复的「尚未批准」。
    let fresh = TestClient {
        keys: RustSigningKey::generate(),
        conn: factory.connect().await,
        received: Mutex::new(Vec::new()),
    };
    let fresh_reply = fresh
        .connect_frame(&svc, epoch, [69u8; 32])
        .await
        .expect("首绑有应答");
    assert!(
        matches!(fresh_reply, ServerFrame::ServerHello(ref hello) if hello.state == "pending_approval"),
        "从未绑定的钥应进待批（不是撤权拒绝）：{fresh_reply:?}"
    );
}

// ============================================================
// K3 回归（2026-10-29 修复批 I-3）：生产撤权面的卸载出口
// ============================================================
//
// I-3：`apply_uninstall` 恒先 `revoke_profile`；旧生产实现在「bindings.json 里没有该
// profile 的 Bound 行」时直接 `DSH_REVOKE_FAILED`，在「auth 域初始化失败」时原样上抛
// 域错误——于是「装了但从没绑定过」与「域坏了想卸载当修复」两种都必须拿 Failed
// 作业，**堵死了唯一能清掉已写入状态的出口**。
//
// 修正语义：
//  (a) 该 profile 没有 Bound 绑定 → 成功（没有可撤的东西）；
//  (b) 域不可用 → 成功。**论证**：闸门表（`gates`）与受信 Peer 注册只在内存里，
//      域初始化失败意味着本进程从未建立授权通道、也没有活连接，可证明不存在可撤
//      的活授权；
//  (c) 真有 Bound 绑定且撤权动作本身失败 → 仍然 `DSH_REVOKE_FAILED`
//      （「先撤后删」的序与真实性都不放水）。

use multi_agents_manager_lib::services::dsh_control::auth::ProductionBindingRevoker;

/// 一条账本行的 JSON 形态（批准后落账的形状；公钥用占位字节）。
fn ledger_row(binding_id: &str, profile_dir: &str, state: &str) -> serde_json::Value {
    serde_json::json!({
        "bindingId": binding_id,
        "publicKeySpki": dsh_auth::b64url_encode(&[9u8; 44]),
        "revokedKeySpki": "",
        "executable": format!(r"{WIN_INSTALL_DIR}\DeepSeek Harness.exe"),
        "pid": HOST_PID,
        "startedAtMs": HOST_STARTED_AT,
        "profileDir": profile_dir,
        "createdAtMs": HOST_STARTED_AT,
        "state": state,
    })
}

/// 生产撤权面测试台：真实 tempdir 状态目录 + **先落账本再建服务**（构造期加载）。
struct RevokeBench {
    svc: Arc<dsh_auth::DshAuthService>,
    env: DshEnvironment,
    state_dir: PathBuf,
    _tmp: tempfile::TempDir,
}

impl RevokeBench {
    /// `rows` 为空 = 从未绑定过的账本（没有任何记录）。
    fn with_ledger(rows: Vec<serde_json::Value>) -> Self {
        let b = bench();
        let state_dir = b.env.tuvis_home.join("dsh");
        if !rows.is_empty() {
            std::fs::create_dir_all(&state_dir).expect("状态目录可建");
            let ledger = serde_json::json!({ "bindings": rows });
            std::fs::write(
                state_dir.join("bindings.json"),
                serde_json::to_vec_pretty(&ledger).unwrap(),
            )
            .expect("账本可写");
        }
        let env = clone_env(&b);
        let svc = Arc::new(dsh_auth::DshAuthService::new(clone_env(&b)).expect("auth 服务可建"));
        RevokeBench {
            svc,
            env,
            state_dir,
            _tmp: b._tmp,
        }
    }

    fn ledger_states(&self) -> Vec<(String, String)> {
        ledger_states_at(&self.state_dir)
    }
}

/// 读 `<state_dir>/bindings.json` 的 (bindingId, state) 列表（账本文件真值）。
fn ledger_states_at(state_dir: &Path) -> Vec<(String, String)> {
    let raw = std::fs::read_to_string(state_dir.join("bindings.json")).expect("账本可读");
    let ledger: serde_json::Value = serde_json::from_str(&raw).expect("账本是 JSON");
    ledger["bindings"]
        .as_array()
        .expect("bindings 是数组")
        .iter()
        .map(|row| {
            (
                row["bindingId"].as_str().unwrap_or_default().to_string(),
                row["state"].as_str().unwrap_or_default().to_string(),
            )
        })
        .collect()
}

/// (a) 从未绑定过的 profile → 卸载前撤权是成功（没有可撤的东西）。
#[tokio::test]
async fn uninstall_revoke_succeeds_when_profile_was_never_bound() {
    let rb = RevokeBench::with_ledger(Vec::new());
    ProductionBindingRevoker::revoke_with(Ok(Arc::clone(&rb.svc)), &rb.env, WIN_PROFILE_TOKEN)
        .expect("无绑定记录的卸载前撤权必须成功（I-3a）");
    assert_eq!(
        rb.svc.authorization_state(),
        AuthorizationState::Unbound,
        "无绑定 → 状态仍是 Unbound（没有凭空造出撤权墓碑）"
    );
}

/// (b) auth 域不可用 → 撤权放行（可证明没有活的授权通道）。
#[test]
fn uninstall_revoke_succeeds_when_auth_domain_is_unavailable() {
    let rb = RevokeBench::with_ledger(Vec::new());
    let broken = Err(DshError::new(
        codes::IPC_STATE,
        "authorization key file is unreadable: /nonexistent/authorization.key",
    ));
    ProductionBindingRevoker::revoke_with(broken, &rb.env, WIN_PROFILE_TOKEN)
        .expect("域不可用时的卸载前撤权必须放行（闸门只在内存里，I-3b）");
    assert_eq!(
        rb.svc.authorization_state(),
        AuthorizationState::Unbound,
        "没有任何绑定记录时不得凭空造出墓碑"
    );
}

/// (I-6) 域不可用 + 盘上有 Bound 行 → 撤权必须**落盘**，不得留下可复活的授权。
///
/// 缺陷：旧实现在域不可用分支直接 `Ok` 返回、不碰持久账本——Bound 行存活，
/// 修复域并重装后同一把插件钥命中钉公钥分支直接 `authorized`，**未经用户批准**，
/// 违反 §2.5「先撤后删」。
///
/// 契约：域不可用仍放行（不堵卸载出口），但必须做**账本只写**撤销：按两级 profile
/// 口径把 Bound 行改成撤权墓碑（0600 原子落盘）。写不成时留 `revoke-pending.json`
/// 标记，由下一次成功初始化消费。
#[test]
fn uninstall_revoke_when_domain_is_down_still_persists_revocation() {
    let rb = RevokeBench::with_ledger(vec![ledger_row("binding-i6", WIN_PROFILE_TOKEN, "bound")]);
    let broken = Err(DshError::new(
        codes::IPC_STATE,
        "authorization key file is unreadable: /nonexistent/authorization.key",
    ));
    ProductionBindingRevoker::revoke_with(broken, &rb.env, WIN_PROFILE_TOKEN)
        .expect("域不可用不得堵死卸载出口（仍 Ok）");
    assert_eq!(
        rb.ledger_states(),
        vec![("binding-i6".to_string(), "revoked".to_string())],
        "域不可用也必须把盘上的 Bound 行撤成墓碑（否则修复后未批准复活）"
    );
    assert!(
        !rb.state_dir.join("revoke-pending.json").exists(),
        "直写成功就不该留待落盘标记"
    );
}

/// (I-6 兜底) 连账本都写不成 → 留 `revoke-pending.json`；下一次初始化消费它
/// （把行改成墓碑）并清掉标记，仍不阻断卸载。
#[test]
fn uninstall_revoke_when_domain_is_down_and_ledger_write_fails_leaves_a_marker() {
    let rb = RevokeBench::with_ledger(vec![ledger_row("binding-i6b", WIN_PROFILE_TOKEN, "bound")]);
    // 原子写的临时文件位置被目录占住 → 账本只写必然失败（真实 IO 失败）。
    std::fs::create_dir_all(rb.state_dir.join("bindings.tmp")).expect("占位目录可建");
    let broken = Err(DshError::new(
        codes::IPC_STATE,
        "authorization key unreadable",
    ));
    ProductionBindingRevoker::revoke_with(broken, &rb.env, WIN_PROFILE_TOKEN)
        .expect("连账本都写不成也不得堵死卸载出口");
    assert!(
        rb.state_dir.join("revoke-pending.json").exists(),
        "写不成时必须留待落盘标记（下次初始化补刀）"
    );

    // 修复写入条件后重新初始化 → 标记被消费：行变墓碑、标记消失。
    std::fs::remove_dir(rb.state_dir.join("bindings.tmp")).expect("撤掉占位目录");
    let _svc = dsh_auth::DshAuthService::new(clone_env_of(&rb.env)).expect("修复后可建服务");
    assert_eq!(
        rb.ledger_states(),
        vec![("binding-i6b".to_string(), "revoked".to_string())],
        "初始化必须消费待落盘标记并把 Bound 行撤成墓碑"
    );
    assert!(
        !rb.state_dir.join("revoke-pending.json").exists(),
        "标记被消费后必须清除（不得永久残留）"
    );
}

/// (I-6 端到端) 域坏撤权落盘之后：修复并重建服务，同钥重连**不再**被自动授权。
#[tokio::test]
async fn rehello_after_domain_down_revoke_is_not_auto_authorized() {
    let keys = RustSigningKey::generate();
    let spki_b64 = dsh_auth::b64url_encode(&keys.spki);
    let row = serde_json::json!({
        "bindingId": "binding-i6c",
        "publicKeySpki": spki_b64,
        "revokedKeySpki": "",
        "executable": format!(r"{WIN_INSTALL_DIR}\DeepSeek Harness.exe"),
        "pid": HOST_PID,
        "startedAtMs": HOST_STARTED_AT,
        "profileDir": WIN_PROFILE_TOKEN,
        "createdAtMs": HOST_STARTED_AT,
        "state": "bound",
    });
    let rb = RevokeBench::with_ledger(vec![row]);
    let broken = Err(DshError::new(
        codes::IPC_STATE,
        "authorization key unreadable",
    ));
    ProductionBindingRevoker::revoke_with(broken, &rb.env, WIN_PROFILE_TOKEN)
        .expect("域坏撤权放行");

    // 修复后重建服务（模拟下次启动），插件用同一把钥重连。
    let (svc, factory) = served_with_env(&rb.env).await;
    let epoch = current_epoch(&rb.env);
    let reconnect = TestClient {
        keys: RustSigningKey::from_seed(&keys.seed()),
        conn: factory.connect().await,
        received: Mutex::new(Vec::new()),
    };
    let challenge = BindingChallengeMessage {
        binding_protocol: BINDING_PROTOCOL,
        key_digests: [
            svc.key_digest().to_string(),
            dsh_auth::key_digest(&reconnect.keys.spki),
        ],
        nonce: dsh_auth::b64url_encode(&[70u8; 32]),
        host_id: fixture_host_id(),
        epoch: epoch.to_string(),
        expiry: svc.now_ms() + CHALLENGE_TTL_MS,
        home_binding: fixture_home_binding(WIN_HOME, HOST_PID, HOST_STARTED_AT),
    };
    let reply = reconnect
        .connect_frame(&svc, epoch, [70u8; 32])
        .await
        .expect("域修复后须返回双签名撤权应答");
    assert_signed_revoked_hello(&reply, &svc, &reconnect.keys, &challenge, "binding-i6c");
    assert_eq!(
        svc.authorization_state(),
        AuthorizationState::Revoked,
        "修复后旧钥不得恢复 Bound"
    );
    assert_eq!(svc.active_binding_id(), None);
    assert!(svc.pending_binding().is_none(), "墓碑旧钥不得退回待批");
    let delivery = reconnect
        .send_credential("binding-i6c", "revoked-cookie")
        .await
        .expect("撤权交付有拒绝应答");
    assert!(
        matches!(&delivery, ServerFrame::Error { error, .. } if error == codes::AUTH_NOT_APPROVED),
        "{delivery:?}"
    );
    assert!(
        !svc.has_delivered_peer("binding-i6c"),
        "修复后旧钥不得交付凭据"
    );
}

/// (c) 真有 Bound 绑定、而撤权动作本身失败 → 仍然 `DSH_REVOKE_FAILED`。
///
/// 失败注入取「账本原子写的临时文件位置被一个目录占住」：撤权必须把 Bound→Revoked
/// 写回账本，`std::fs::write` 到目录路径必然 `EISDIR`——这是**撤权动作本身**的
/// 真实 IO 失败，不是「域不可用」也不是「没有绑定」，正是唯一必须保持失败的那档。
#[tokio::test]
async fn uninstall_revoke_still_fails_when_a_real_revocation_fails() {
    let rb = RevokeBench::with_ledger(vec![ledger_row("binding-k3c", WIN_PROFILE_TOKEN, "bound")]);
    // `atomic_write_0600` 先写 `<path>.tmp` 再 rename：把该位置占成目录。
    std::fs::create_dir_all(rb.state_dir.join("bindings.tmp")).expect("占位目录可建");
    let err =
        ProductionBindingRevoker::revoke_with(Ok(Arc::clone(&rb.svc)), &rb.env, WIN_PROFILE_TOKEN)
            .expect_err("撤权动作真失败时不得静默放行");
    assert_eq!(
        err.code,
        codes::REVOKE_FAILED,
        "先撤后删：撤权真失败必须如实上报：{err:?}"
    );

    // Round-8 P1-1（Ruling 9，取代旧「内存/磁盘不得分叉」回滚）：内存面先行且
    // **不回滚**——已失效是事实，落盘是记账。内存如实报 Revoked（受信 Peer 已摘、
    // 闸门已翻），盘上该行按「可证明未生效」保持 Bound（复活面由开关执行门与
    // 传输闸门 fail-closed 兜底，落盘由重试收敛）。
    assert_eq!(
        rb.svc.authorization_state(),
        AuthorizationState::Revoked,
        "落盘失败保留内存失效态（授权状态如实反映已失效，不回滚）"
    );
    assert_eq!(
        rb.svc.active_binding_id(),
        None,
        "内存账本已记 Revoked——该绑定不再是可撤的活跃绑定"
    );
    assert_eq!(
        rb.ledger_states(),
        vec![("binding-k3c".to_string(), "bound".to_string())],
        "盘上绑定行按「可证明未生效」保持 Bound（重试撤权收敛落盘）"
    );
}

/// Round-8 P1-1 追加（评审 Important）：卸载**编排层**的重试收敛——`revoke_with`
/// 的收集与收尾校验都必须包含「内存已撤、落盘待收敛」的行。首撤写盘失败后，重试
/// 卸载必须先把墓碑补到盘上再放行 remove；否则内存面已失效的假象会让收尾校验
/// 放行 → 插件文件被删而盘上仍 Bound → 修复重装后同钥命中钉钥分支免批准复活
/// （§2.5/I-6 面）。盘上墓碑 + revokedKeySpki 留存即钉钥分支的不可复活证据
/// （钉钥分支按「内存账本(=重启后从盘装载) 中 state==Bound 且同钥」命中；
/// wire 级墓碑拒绝另见 `tombstoned_key_rehello_gets_peer_revoked_on_the_wire`）。
#[tokio::test]
async fn uninstall_revoke_write_failure_converges_on_retry_and_blocks_revival() {
    let rb = RevokeBench::with_ledger(vec![ledger_row("binding-k5c", WIN_PROFILE_TOKEN, "bound")]);
    // `atomic_write_0600` 先写 `<path>.tmp` 再 rename：把该位置占成目录 → 每次写必败。
    std::fs::create_dir_all(rb.state_dir.join("bindings.tmp")).expect("占位目录可建");
    let err =
        ProductionBindingRevoker::revoke_with(Ok(Arc::clone(&rb.svc)), &rb.env, WIN_PROFILE_TOKEN)
            .expect_err("首撤写盘失败必须中止卸载（先撤后删不放水）");
    assert_eq!(err.code, codes::REVOKE_FAILED, "必须如实上报：{err:?}");
    assert_eq!(
        rb.ledger_states(),
        vec![("binding-k5c".to_string(), "bound".to_string())],
        "前置：首撤失败后盘上仍是 Bound（墓碑未落）"
    );
    assert_eq!(
        rb.svc.authorization_state(),
        AuthorizationState::Revoked,
        "内存已失效（Ruling 9 不回滚）——编排层不得据此跳过落盘"
    );

    // 解除占位 → 重试卸载：待收敛行必须重写墓碑落盘，随后才允许继续 remove。
    std::fs::remove_dir_all(rb.state_dir.join("bindings.tmp")).expect("占位可除");
    ProductionBindingRevoker::revoke_with(Ok(Arc::clone(&rb.svc)), &rb.env, WIN_PROFILE_TOKEN)
        .expect("重试卸载必须补写墓碑后成功");
    let raw = std::fs::read_to_string(rb.state_dir.join("bindings.json")).expect("账本可读");
    let ledger: serde_json::Value = serde_json::from_str(&raw).expect("账本是 JSON");
    let row = &ledger["bindings"][0];
    assert_eq!(
        row["state"], "revoked",
        "重试必须把墓碑写到盘上（不得因内存已撤就跳过落盘直接 remove）"
    );
    assert!(
        !row["revokedKeySpki"]
            .as_str()
            .unwrap_or_default()
            .is_empty(),
        "墓碑必须留存被撤钥——修复重装后同钥重连命中钉钥分支时据此拒绝（不复活）"
    );
    assert_eq!(
        rb.svc.authorization_state(),
        AuthorizationState::Revoked,
        "收敛后授权状态保持 Revoked"
    );
}

/// (c2) 正面控制：真实存在的 Bound 绑定会被真的撤掉（墓碑落账）。
#[tokio::test]
async fn uninstall_revoke_marks_existing_bound_binding_revoked() {
    let rb = RevokeBench::with_ledger(vec![ledger_row("binding-k3c2", WIN_PROFILE_TOKEN, "bound")]);
    ProductionBindingRevoker::revoke_with(Ok(Arc::clone(&rb.svc)), &rb.env, WIN_PROFILE_TOKEN)
        .expect("有绑定时的撤权必须成功");
    assert_eq!(
        rb.ledger_states(),
        vec![("binding-k3c2".to_string(), "revoked".to_string())],
        "撤权必须落账为 revoked 墓碑"
    );
}

/// 反面控制：兄弟 profile（`desktop-2`）不得被当成同一个 profile（多撤比撤不到更糟）。
#[tokio::test]
async fn uninstall_revoke_does_not_match_a_sibling_profile() {
    let sibling = format!("{WIN_PROFILE_TOKEN}-2");
    let rb = RevokeBench::with_ledger(vec![ledger_row("binding-k4c", &sibling, "bound")]);
    ProductionBindingRevoker::revoke_with(Ok(Arc::clone(&rb.svc)), &rb.env, WIN_PROFILE_TOKEN)
        .expect("兄弟 profile 不命中 = 无可撤 = 成功");
    assert_eq!(
        rb.ledger_states(),
        vec![("binding-k4c".to_string(), "bound".to_string())],
        "兄弟 profile 的绑定不得被误撤"
    );
}

// ------------------------------------------------------------
// K4 回归（2026-10-29 修复批 I-4）：撤权匹配口径必须与落账口径一致
// ------------------------------------------------------------
//
// 缺陷：`apply_uninstall` 传 `resolve_installation().profile_dir`
// （= `<env.home>/profiles/desktop`），而 `approve` 落账记的是 `discover()` 的
// **argv 优先** profile_dir。自定义 DSH_HOME（宿主 argv 的 profile 根不同于兔维斯
// env.home）时两者字符串不等 → 卸载撤不掉绑定（静默留下陈旧授权）。
//
// 修正：两级匹配（归一化路径等价 / profile 名尾段 `profiles/<name>`），任一命中
// 即同一 profile；不做前缀/包含式模糊匹配（错撤兄弟 profile 比撤不到更糟）。

/// (I-4) 自定义 DSH_HOME：宿主 argv 的 profile 根与兔维斯 env.home 不同时，卸载
/// 仍然必须撤掉绑定（旧实现按字符串全等 → 静默不撤）。
#[tokio::test]
async fn uninstall_revoke_matches_binding_from_a_different_home_root() {
    // 落账口径 = discover()（argv 优先）：宿主跑在影子 home 下。
    let argv_profile = "/opt/dsh-shadow/home-shadow/profiles/desktop";
    let rb = RevokeBench::with_ledger(vec![ledger_row("binding-k4", argv_profile, "bound")]);
    // 卸载口径 = resolve_installation()（env.home 推导）：另一个根。
    ProductionBindingRevoker::revoke_with(Ok(Arc::clone(&rb.svc)), &rb.env, WIN_PROFILE_TOKEN)
        .expect("不同 home 根但同一 profile 名，撤权必须命中（I-4）");
    assert_eq!(
        rb.ledger_states(),
        vec![("binding-k4".to_string(), "revoked".to_string())],
        "绑定的 Bound 行必须被撤（不得静默留下陈旧授权）"
    );
}

/// (I-4 补) 归一化等价：分隔符/尾斜杠/`.` 段的差异不影响匹配。
#[tokio::test]
async fn uninstall_revoke_matches_normalized_profile_paths() {
    let recorded = format!("{WIN_PROFILE_TOKEN}\\");
    let rb = RevokeBench::with_ledger(vec![ledger_row("binding-k4b", &recorded, "bound")]);
    let requested = format!("{WIN_PROFILE_TOKEN}/./");
    ProductionBindingRevoker::revoke_with(Ok(Arc::clone(&rb.svc)), &rb.env, &requested)
        .expect("归一化后同一 profile 必须命中");
    assert_eq!(
        rb.ledger_states(),
        vec![("binding-k4b".to_string(), "revoked".to_string())]
    );
}

// ============================================================
// K9 回归（2026-10-29 修复批 M-6）：本机 IPC 帧读取必须执行 1 MiB 上限
// ============================================================
//
// 缺陷：`local_ipc::MAX_FRAME_BYTES`（1 MiB）已定义却**从未被读取路径使用**——
// `UdsConn::recv` 直接 `read_line(&mut String)`，无界。当前用户任意进程能连上这条
// UDS（目录 0700 是本机用户的边界，同用户进程不受限），写一行 4 GiB 就能把兔维斯
// 的内存撑到 OOM——声称的硬上限并不存在。
//
// 契约：超限帧 → `DSH_IPC_STATE` 拒绝，连接按既有语义标记关闭（不是无限读下去）；
// 界内帧照常读取（不误伤）。

#[cfg(unix)]
mod frame_cap {
    use super::*;
    use multi_agents_manager_lib::services::dsh_control::local_ipc::{
        production_listener, IpcClientConnection, IpcListener, MAX_FRAME_BYTES,
    };
    use std::time::Duration;
    use tokio::io::AsyncWriteExt;
    use tokio::net::UnixStream;

    /// 起一条真实 UDS 连接（服务端 + 客户端流）。
    async fn pair() -> (
        tempfile::TempDir,
        Box<dyn IpcListener>,
        Arc<dyn IpcClientConnection>,
        UnixStream,
    ) {
        let tmp = tempfile::tempdir().expect("tempdir 可建");
        let state_dir = tmp.path().join("dsh");
        assert!(
            state_dir.to_string_lossy().len() + "/ipc-0123456789abcdef.sock".len() <= 103,
            "夹具路径必须在 sun_path 上限内"
        );
        let listener = production_listener(state_dir);
        let path = listener.bind().await.expect("真实 UDS bind");
        let client = UnixStream::connect(&path).await.expect("客户端连上 UDS");
        let server = listener.accept().await.expect("服务端 accept");
        (tmp, listener, Arc::from(server), client)
    }

    /// 超过 1 MiB 的单行 → `DSH_IPC_STATE` 拒绝（旧实现会一路读进内存）。
    #[tokio::test]
    async fn oversized_frame_is_rejected_with_ipc_state() {
        let (_tmp, _listener, server, mut client) = pair().await;
        let oversized = vec![b'x'; MAX_FRAME_BYTES + 1024];
        // 对端持续写（服务端一旦到上限就该停读并报错，而不是等 EOF）。
        let writer = tokio::spawn(async move {
            let _ = client.write_all(&oversized).await;
            let _ = client.write_all(b"\n").await;
            client
        });

        let outcome = tokio::time::timeout(Duration::from_secs(5), server.recv())
            .await
            .expect("超限帧必须在有界窗口内被拒（不得无限读）");
        // 断言里只报长度（整行原文会把 1 MiB 灌进失败信息）。
        let err = match outcome {
            Err(err) => err,
            Ok(line) => panic!("超限帧必须报错，不得当成有效帧（读到 {} 字节）", line.len()),
        };
        assert_eq!(
            err.code,
            codes::IPC_STATE,
            "超限帧的错误码应为既有连接状态码：{err:?}"
        );
        assert!(
            err.detail.contains("frame"),
            "诊断必须点明是帧上限问题：{}",
            err.detail
        );
        let _ = writer.await;
    }

    /// 界内帧不受影响（上限不得误伤正常握手帧）。
    #[tokio::test]
    async fn frame_within_the_cap_is_delivered() {
        let (_tmp, _listener, server, mut client) = pair().await;
        // 接近上限但仍在界内的一行（1 MiB - 2 字节内容 + '\n'）。
        // 写侧必须并发：1 MiB 远超 socket 缓冲，先写完再读会双方阻塞。
        let writer = tokio::spawn(async move {
            let payload = vec![b'a'; MAX_FRAME_BYTES - 2];
            client.write_all(&payload).await.expect("写界内帧");
            client.write_all(b"\n").await.expect("写行界");
            client
        });
        let line = tokio::time::timeout(Duration::from_secs(5), server.recv())
            .await
            .expect("界内帧必须在窗口内可读")
            .expect("界内帧必须成功");
        assert_eq!(line.len(), MAX_FRAME_BYTES - 2, "整行内容原样交付");
        let _ = writer.await;
    }

    /// (M-9 复审) 上限语义是**内容长度 ≤ MAX**：恰好 `MAX_FRAME_BYTES` 字节内容
    /// 必须交付（旧实现按「读出字节数（含换行）> MAX」判定，恰好到界的帧被误拒）。
    #[tokio::test]
    async fn frame_with_exactly_the_cap_is_delivered() {
        let (_tmp, _listener, server, mut client) = pair().await;
        let writer = tokio::spawn(async move {
            let payload = vec![b'b'; MAX_FRAME_BYTES];
            client.write_all(&payload).await.expect("写恰好到界的帧");
            client.write_all(b"\n").await.expect("写行界");
            client
        });
        let line = tokio::time::timeout(Duration::from_secs(5), server.recv())
            .await
            .expect("恰好到界的帧必须在窗口内可读")
            .expect("恰好 MAX_FRAME_BYTES 字节内容必须被接受（内容 ≤ 上限）");
        assert_eq!(line.len(), MAX_FRAME_BYTES, "恰好到界的内容原样交付");
        let _ = writer.await;
    }

    /// (M-9 复审) 反面：`MAX_FRAME_BYTES + 1` 字节内容 → 仍然 `DSH_IPC_STATE` 拒绝。
    #[tokio::test]
    async fn frame_one_byte_over_the_cap_is_rejected() {
        let (_tmp, _listener, server, mut client) = pair().await;
        let writer = tokio::spawn(async move {
            let payload = vec![b'c'; MAX_FRAME_BYTES + 1];
            client.write_all(&payload).await.expect("写超界 1 字节的帧");
            client.write_all(b"\n").await.expect("写行界");
            client
        });
        let outcome = tokio::time::timeout(Duration::from_secs(5), server.recv())
            .await
            .expect("超界帧必须在有界窗口内被拒");
        let err = match outcome {
            Err(err) => err,
            Ok(line) => panic!("超界 1 字节必须拒绝（读到 {} 字节）", line.len()),
        };
        assert_eq!(
            err.code,
            codes::IPC_STATE,
            "超界帧的错误码应为连接状态码：{err:?}"
        );
        let _ = writer.await;
    }
}

/// G1-Mac 真机首握暴露（2026-10-30）：`ED25519_SPKI_PREFIX` 曾误写 0x42——自身
/// SPKI 畸形且拒绝一切合法客户端（单测两侧共用同一 wrap 常量，从未抓获）。本用例
/// 把**插件侧真实口径**（Node `exportPublicKey({type:"spki",format:"der"})` 的标准
/// 44 字节前缀）钉进回归：unwrap 必须接受、wrap(unwrap) 必须逐字节回到 Node 口径。
#[test]
fn spki_wrap_matches_node_ed25519_export() {
    let node_prefix = [
        0x30u8, 0x2a, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x03, 0x21, 0x00,
    ];
    let mut der = node_prefix.to_vec();
    der.extend_from_slice(&[7u8; 32]);
    let raw = multi_agents_manager_lib::services::dsh_control::auth::spki_unwrap(&der)
        .expect("标准 Node SPKI 必须被接受");
    assert_eq!(raw, [7u8; 32]);
    assert_eq!(
        multi_agents_manager_lib::services::dsh_control::auth::spki_wrap_raw(&raw),
        der,
        "wrap 必须与 Node 口径逐字节一致"
    );
}

// ============================================================
// P1-②（2026-10-30 第一阶段复审 T08 前门槛）：authorize_grant 严格核验
// 凭据所属绑定、Host 与代际——不得把旧代 Cookie 重盖章成新代句柄。
// ============================================================

/// 旧代交付 → 宿主换代 → 新凭据未交付 ⇒ 授权必须拒绝（DSH_AUTH_EPOCH_STALE）；
/// 旧连接交付也须拒绝；该绑定经新代签名握手再交付后才授权，句柄为当代事实。
#[tokio::test]
async fn stale_generation_credential_refuses_until_redispatched() {
    let b = bench();
    let authority = spawn_fake_mux_host().await;
    *b.procs.listeners.lock().unwrap() = vec![DshListener {
        owner_pid: HOST_PID,
        authority: authority.clone(),
    }];
    let (svc, factory) = served(&b).await;
    let epoch1 = current_epoch(&b.env);

    let client = TestClient {
        keys: RustSigningKey::generate(),
        conn: factory.connect().await,
        received: Mutex::new(Vec::new()),
    };
    let reply = client
        .connect_frame(&svc, epoch1, [61u8; 32])
        .await
        .expect("首绑握手有应答");
    let ServerFrame::ServerHello(hello) = reply else {
        panic!("首绑应得 serverHello")
    };
    let binding_id = hello.binding_id.clone().expect("binding_id");
    svc.approve(&binding_id).await.expect("批准");
    let _ = client.drain().await;
    let ack = client
        .send_credential(&binding_id, "dsh-auth-gen1=cookie-old")
        .await
        .expect("交付有应答");
    assert!(matches!(ack, ServerFrame::Granted { .. }));

    // 宿主换代：同 PID 换启动时刻（epoch_of 的输入）→ discover 得到新代际。
    *b.procs.processes.lock().unwrap() = vec![desktop_host_process(HOST_STARTED_AT + 1_000)];
    let epoch2 = current_epoch(&b.env);
    assert_ne!(epoch1, epoch2, "换代模拟必须改变代际");
    let host2 = identity::discover(&b.env).expect("换代后宿主仍在");
    let caller = ClientIdentity {
        public_key_spki: client.keys.spki.clone(),
        executable: host2.executable.clone(),
        pid: host2.pid,
        started_at_ms: host2.started_at_ms,
    };

    // 新代凭据未交付 ⇒ 拒绝（不能拿 gen1 的 Cookie 冒充 gen2 句柄）。
    let err = svc
        .authorize_grant(&host2, &caller)
        .await
        .expect_err("旧代凭据必须被拒绝");
    assert_eq!(err.code, codes::AUTH_EPOCH_STALE, "实际：{err:?}");

    // 即使声称交付新凭据，未完成新代握手的旧连接仍必须拒绝。
    let rejected = client
        .send_credential(&binding_id, "dsh-auth-gen2=cookie-new")
        .await
        .expect("旧连接交付有应答");
    assert!(
        matches!(&rejected, ServerFrame::Error { error, .. } if error == codes::AUTH_EPOCH_STALE),
        "旧连接交付必须拒绝，实际 {rejected:?}"
    );
    let err = svc
        .authorize_grant(&host2, &caller)
        .await
        .expect_err("旧连接的交付不得使旧凭据变成当代句柄");
    assert_eq!(err.code, codes::AUTH_EPOCH_STALE);

    // 同一绑定密钥经新连接发送带真实签名的新代 hello，再交付当代凭据。
    let current_client = InstanceClient::with_keys(
        &factory,
        &client.keys,
        host2.pid,
        host2.started_at_ms,
        host2.host_id.clone(),
    )
    .await;
    let reply = current_client
        .connect(&svc, epoch2, [62u8; 32])
        .await
        .expect("新代签名握手成功");
    let ServerFrame::ServerHello(hello2) = reply else {
        panic!("新代握手应返回 serverHello，实际 {reply:?}");
    };
    assert_eq!(hello2.state, "authorized");
    assert_eq!(hello2.binding_id.as_deref(), Some(binding_id.as_str()));
    assert_eq!(hello2.epoch, epoch2.to_string());
    let ack2 = current_client
        .send_credential(&binding_id, "dsh-auth-gen2=cookie-new")
        .await
        .expect("新代握手后交付有应答");
    assert!(matches!(ack2, ServerFrame::Granted { .. }), "{ack2:?}");
    let peer = svc
        .authorize_grant(&host2, &caller)
        .await
        .expect("当代凭据应可授权");
    assert_eq!(peer.epoch(), epoch2);
    assert_eq!(peer.authority(), authority.as_str());
}

/// （T09-G 改写自 `credential_of_other_binding_is_not_issued_to_this_caller`，
/// 按计划要求改为**正向归属断言**：不得仅断言「A 取句柄返回错误」就当多绑定已支持。）
///
/// A 先交付、B 后交付：A 取到的必须仍是 **A 自己交付的** cookie_a——不是 cookie_b、
/// 也不是错误。单槽 `current_peer` 下后交付者覆盖先交付者，A 只能得
/// `DSH_AUTH_NOT_APPROVED`；分槽后每个绑定各取其是。
/// 「有 Bound 行但从未交付 → `DSH_AUTH_NOT_APPROVED`」的真负例由
/// `authorize_without_delivered_credential_still_refuses` 覆盖。
#[tokio::test]
async fn credential_of_other_binding_is_not_what_this_caller_receives() {
    let b = bench();
    let authority = spawn_fake_mux_host().await;
    *b.procs.listeners.lock().unwrap() = vec![DshListener {
        owner_pid: HOST_PID,
        authority: authority.clone(),
    }];
    let (svc, factory) = served(&b).await;
    let epoch = current_epoch(&b.env);

    let keys_a = RustSigningKey::generate();
    let keys_b = RustSigningKey::generate();
    let cookie_a = "dsh-auth-t09g=cookie-a";
    let cookie_b = "dsh-auth-t09g=cookie-b";
    let binding_a =
        bind_and_deliver_with_cookie(&svc, &factory, &keys_a, epoch, [71u8; 32], cookie_a).await;
    let binding_b =
        bind_and_deliver_with_cookie(&svc, &factory, &keys_b, epoch, [72u8; 32], cookie_b).await;
    assert_ne!(binding_a, binding_b);

    let host = identity::discover(&b.env).expect("宿主在跑");
    let peer_a = svc
        .authorize_grant(&host, &host_identity(&keys_a))
        .await
        .expect("先交付者 A 必须取到自己的凭据（不得因 B 后交付被覆盖成错误）");
    assert_eq!(
        peer_a.binding_id(),
        binding_a,
        "A 的句柄必须钉在 A 的绑定上"
    );
    assert_eq!(
        peer_a.secret(),
        cookie_a.as_bytes(),
        "A 拿到的必须是 A 交付的 cookie"
    );
    assert_ne!(
        peer_a.secret(),
        cookie_b.as_bytes(),
        "A 不得拿到 B 交付的 cookie"
    );
}

// ============================================================
// T09-G 先行安全门（2026-10-30 第二批复审放行节第 3 条）：
// 授权句柄按 binding_id 分槽——多绑定互不串用。
// ============================================================
//
// 旧缺陷：`Inner.current_peer` 单槽——凭据交付**覆盖写**、`revoke_local`
// **无条件清槽**、`authorize_grant` 从单槽取后做归属核验。归属核验挡住了
// 「把 B 的秘密发给 A」，但功能上：后交付者覆盖先交付者（先交付者取句柄得
// `DSH_AUTH_NOT_APPROVED`）；撤任一绑定清空另一绑定已交付的句柄。
//
// 契约：`peers: HashMap<binding_id, AuthorizedPeer>`——交付按 `cred.binding_id`
// 插入（同绑定重交付覆盖自身）；`authorize_grant` 按调用方自己的 Bound 行
// binding_id 取；`revoke_local` 只移除该绑定条目。归属/闸门/代际三类核验一个
// 不少；`peers` 只在内存（owner Cookie 不落盘）。
//
// 观测纪律：全部经生产 `authorize_grant` / `revoke` / wire 交付路径观察；秘密
// 字节比较走 [`AuthorizedPeer::secret`]（transport 组请求头的既有读面，测试不
// 新增任何导出字节的 API）。

/// ① A、B 两把钥分别 hello → 批准 → 交付**不同** cookie：两者各自
/// `authorize_grant` **都成功**，且各拿到自己交付的那份（binding_id 与秘密字节
/// 都对得上、闸门是自己的且未撤）。先交付者不得因后交付者被覆盖。
#[tokio::test]
async fn each_binding_retrieves_its_own_delivered_credential() {
    let b = bench();
    let authority = spawn_fake_mux_host().await;
    *b.procs.listeners.lock().unwrap() = vec![DshListener {
        owner_pid: HOST_PID,
        authority: authority.clone(),
    }];
    let (svc, factory) = served(&b).await;
    let epoch = current_epoch(&b.env);

    let keys_a = RustSigningKey::generate();
    let keys_b = RustSigningKey::generate();
    let cookie_a = "dsh-auth-t09g=cookie-a";
    let cookie_b = "dsh-auth-t09g=cookie-b";
    let binding_a =
        bind_and_deliver_with_cookie(&svc, &factory, &keys_a, epoch, [101u8; 32], cookie_a).await;
    let binding_b =
        bind_and_deliver_with_cookie(&svc, &factory, &keys_b, epoch, [102u8; 32], cookie_b).await;
    assert_ne!(binding_a, binding_b);

    let host = identity::discover(&b.env).expect("宿主在跑");

    // A（先交付者）：单槽下 B 的交付把它覆盖成 AUTH_NOT_APPROVED——这里必须成功。
    let peer_a = svc
        .authorize_grant(&host, &host_identity(&keys_a))
        .await
        .expect("先交付者 A 必须取到自己的句柄");
    assert_eq!(
        peer_a.binding_id(),
        binding_a,
        "A 的句柄必须钉在 A 的绑定上"
    );
    assert_eq!(
        peer_a.secret(),
        cookie_a.as_bytes(),
        "A 拿到的必须是 A 交付的 cookie"
    );
    assert_eq!(
        peer_a.authority(),
        authority.as_str(),
        "A 的句柄钉定发现到的 authority"
    );
    assert_eq!(peer_a.epoch(), epoch, "A 的句柄钉定发现到的代际");
    assert!(
        peer_a.gate().is_some_and(|g| !g.is_revoked()),
        "A 的句柄必须挂着 A 自己的（未撤权）闸门"
    );

    // B（后交付者）：同样各取其是。
    let peer_b = svc
        .authorize_grant(&host, &host_identity(&keys_b))
        .await
        .expect("后交付者 B 必须取到自己的句柄");
    assert_eq!(
        peer_b.binding_id(),
        binding_b,
        "B 的句柄必须钉在 B 的绑定上"
    );
    assert_eq!(
        peer_b.secret(),
        cookie_b.as_bytes(),
        "B 拿到的必须是 B 交付的 cookie"
    );
    assert!(
        peer_b.gate().is_some_and(|g| !g.is_revoked()),
        "B 的句柄必须挂着 B 自己的（未撤权）闸门"
    );
}

/// ② A、B 都已交付后撤 A：A 得 `DSH_PEER_REVOKED`（本绑定自己的闸门决定本绑定）；
/// **B 仍成功且拿到的仍是 B 自己交付的 cookie_b**（旧实现 `revoke_local` 无条件
/// 清单槽 → B 得 `DSH_AUTH_NOT_APPROVED`）。
#[tokio::test]
async fn revoking_a_leaves_b_credential_intact() {
    let b = bench();
    let authority = spawn_fake_mux_host().await;
    *b.procs.listeners.lock().unwrap() = vec![DshListener {
        owner_pid: HOST_PID,
        authority: authority.clone(),
    }];
    let (svc, factory) = served(&b).await;
    let epoch = current_epoch(&b.env);

    let keys_a = RustSigningKey::generate();
    let keys_b = RustSigningKey::generate();
    let cookie_a = "dsh-auth-t09g=cookie-a";
    let cookie_b = "dsh-auth-t09g=cookie-b";
    let binding_a =
        bind_and_deliver_with_cookie(&svc, &factory, &keys_a, epoch, [103u8; 32], cookie_a).await;
    let binding_b =
        bind_and_deliver_with_cookie(&svc, &factory, &keys_b, epoch, [104u8; 32], cookie_b).await;
    assert_ne!(binding_a, binding_b);

    svc.revoke(&binding_a).await.expect("撤权 A");

    let host = identity::discover(&b.env).expect("宿主在跑");
    let err_a = svc
        .authorize_grant(&host, &host_identity(&keys_a))
        .await
        .expect_err("被撤的 A 不得再取句柄");
    assert_eq!(err_a.code, codes::PEER_REVOKED, "实际：{err_a:?}");

    // 撤 A 只摘 A 自己的条目：B 的句柄必须原样可用。
    let peer_b = svc
        .authorize_grant(&host, &host_identity(&keys_b))
        .await
        .expect("撤 A 不得清掉 B 已交付的句柄");
    assert_eq!(
        peer_b.binding_id(),
        binding_b,
        "B 的句柄必须钉在 B 的绑定上"
    );
    assert_eq!(
        peer_b.secret(),
        cookie_b.as_bytes(),
        "B 拿到的必须仍是 B 自己交付的 cookie（不是 A 的，也不是错误）"
    );
}

/// ③ 换代拒旧凭据是**逐绑定**的：A、B 都交付 gen1 凭据 → 宿主换代 → 两者都得
/// `DSH_AUTH_EPOCH_STALE`，两条旧连接重交付也拒绝；只为 A 新代握手并交付 → A 成功，
/// **B 仍 STALE**（不得因 A 的重交付连带放行 B）。
///
/// 换代手法与 [`stale_generation_credential_refuses_until_redispatched`] 相同：
/// 同 PID 换启动时刻（`epoch_of` 的输入）。
#[tokio::test]
async fn stale_generation_refusal_is_per_binding() {
    let b = bench();
    let authority = spawn_fake_mux_host().await;
    *b.procs.listeners.lock().unwrap() = vec![DshListener {
        owner_pid: HOST_PID,
        authority: authority.clone(),
    }];
    let (svc, factory) = served(&b).await;
    let epoch1 = current_epoch(&b.env);

    // A、B 各自完整授权流；保留旧连接，以验证换代后未经新握手的交付会被拒绝。
    let cookie_a1 = "dsh-auth-t09g=gen1-a";
    let cookie_b1 = "dsh-auth-t09g=gen1-b";
    let client_a = TestClient {
        keys: RustSigningKey::generate(),
        conn: factory.connect().await,
        received: Mutex::new(Vec::new()),
    };
    let reply_a = client_a
        .connect_frame(&svc, epoch1, [105u8; 32])
        .await
        .expect("A 首绑握手有应答");
    let ServerFrame::ServerHello(hello_a) = reply_a else {
        panic!("A 首绑应得 serverHello")
    };
    let binding_a = hello_a.binding_id.clone().expect("binding_id");
    svc.approve(&binding_a).await.expect("批准 A");
    let _ = client_a.drain().await;
    let ack_a = client_a
        .send_credential(&binding_a, cookie_a1)
        .await
        .expect("A 交付有应答");
    assert!(matches!(ack_a, ServerFrame::Granted { .. }));

    let client_b = TestClient {
        keys: RustSigningKey::generate(),
        conn: factory.connect().await,
        received: Mutex::new(Vec::new()),
    };
    let reply_b = client_b
        .connect_frame(&svc, epoch1, [106u8; 32])
        .await
        .expect("B 首绑握手有应答");
    let ServerFrame::ServerHello(hello_b) = reply_b else {
        panic!("B 首绑应得 serverHello")
    };
    let binding_b = hello_b.binding_id.clone().expect("binding_id");
    svc.approve(&binding_b).await.expect("批准 B");
    let _ = client_b.drain().await;
    let ack_b = client_b
        .send_credential(&binding_b, cookie_b1)
        .await
        .expect("B 交付有应答");
    assert!(matches!(ack_b, ServerFrame::Granted { .. }));

    // 宿主换代：同 PID 换启动时刻（epoch_of 的输入）→ discover 得到新代际。
    *b.procs.processes.lock().unwrap() = vec![desktop_host_process(HOST_STARTED_AT + 1_000)];
    let epoch2 = current_epoch(&b.env);
    assert_ne!(epoch1, epoch2, "换代模拟必须改变代际");
    let host2 = identity::discover(&b.env).expect("换代后宿主仍在");
    let caller_a = ClientIdentity {
        public_key_spki: client_a.keys.spki.clone(),
        executable: host2.executable.clone(),
        pid: host2.pid,
        started_at_ms: host2.started_at_ms,
    };
    let caller_b = ClientIdentity {
        public_key_spki: client_b.keys.spki.clone(),
        executable: host2.executable.clone(),
        pid: host2.pid,
        started_at_ms: host2.started_at_ms,
    };

    // 换代后 A、B 都必须拒（各自 gen1 凭据不得包装成 gen2 句柄）。
    let err_a = svc
        .authorize_grant(&host2, &caller_a)
        .await
        .expect_err("A 的旧代凭据必须被拒绝");
    assert_eq!(err_a.code, codes::AUTH_EPOCH_STALE, "实际：{err_a:?}");
    let err_b = svc
        .authorize_grant(&host2, &caller_b)
        .await
        .expect_err("B 的旧代凭据必须被拒绝");
    assert_eq!(err_b.code, codes::AUTH_EPOCH_STALE, "实际：{err_b:?}");

    // A、B 的旧连接都不能靠重交付凭据跳过新代握手。
    let cookie_a2 = "dsh-auth-t09g=gen2-a";
    for (old_client, binding) in [(&client_a, &binding_a), (&client_b, &binding_b)] {
        let rejected = old_client
            .send_credential(binding, "dsh-auth-t09g=unverified-gen2")
            .await
            .expect("旧连接交付有应答");
        assert!(
            matches!(&rejected, ServerFrame::Error { error, .. } if error == codes::AUTH_EPOCH_STALE),
            "绑定 {binding} 的旧连接交付必须拒绝，实际 {rejected:?}"
        );
    }

    // 只为 A 使用原密钥，在新连接上完成真实新代签名握手并交付。
    let current_a = InstanceClient::with_keys(
        &factory,
        &client_a.keys,
        host2.pid,
        host2.started_at_ms,
        host2.host_id.clone(),
    )
    .await;
    let reply_a2 = current_a
        .connect(&svc, epoch2, [107u8; 32])
        .await
        .expect("A 新代签名握手成功");
    let ServerFrame::ServerHello(hello_a2) = reply_a2 else {
        panic!("A 新代握手应返回 serverHello，实际 {reply_a2:?}");
    };
    assert_eq!(hello_a2.state, "authorized");
    assert_eq!(hello_a2.binding_id.as_deref(), Some(binding_a.as_str()));
    assert_eq!(hello_a2.epoch, epoch2.to_string());
    let ack_a2 = current_a
        .send_credential(&binding_a, cookie_a2)
        .await
        .expect("A 新代握手后交付有应答");
    assert!(matches!(ack_a2, ServerFrame::Granted { .. }), "{ack_a2:?}");

    // A 成功且句柄即当代事实（不重盖章）；B 未重交付必须仍 STALE——
    // A 的重交付不得连带放行 B。
    let peer_a = svc
        .authorize_grant(&host2, &caller_a)
        .await
        .expect("A 重交付当代凭据后应可授权");
    assert_eq!(peer_a.epoch(), epoch2);
    assert_eq!(peer_a.authority(), authority.as_str());
    assert_eq!(
        peer_a.secret(),
        cookie_a2.as_bytes(),
        "A 拿到的是自己重交付的 cookie"
    );
    let err_b2 = svc
        .authorize_grant(&host2, &caller_b)
        .await
        .expect_err("B 未重交付必须仍拒（不得被 A 的重交付连带放行）");
    assert_eq!(err_b2.code, codes::AUTH_EPOCH_STALE, "实际：{err_b2:?}");
}

// ============================================================
// P1-①（2026-10-30 第一阶段复审 T08 前门槛）：待批到期必须断开旧连接。
// 旧行为：到期记录被 retain 摘掉但旧连接不关——插件挂在 recv 上永不重发
// hello，旧核对码一直展示，用户无法正常重新绑定。
// ============================================================

/// 跨过挑战 TTL 后的同钥新 hello：旧待批对作废（旧码批不得）、旧连接被服务端
/// 关闭、新待批对新码可批准。
#[tokio::test]
async fn expired_pending_is_evicted_and_stale_connection_closed() {
    let b = bench();
    let (svc, factory) = served(&b).await;
    let epoch = current_epoch(&b.env);

    let keys = RustSigningKey::generate();
    let client_a = TestClient {
        keys: RustSigningKey::from_seed(&keys.seed()),
        conn: factory.connect().await,
        received: Mutex::new(Vec::new()),
    };
    let reply_a = client_a
        .connect_frame(&svc, epoch, [81u8; 32])
        .await
        .expect("首次握手有应答");
    let ServerFrame::ServerHello(hello_a) = reply_a else {
        panic!("首绑应得 serverHello")
    };
    let binding_a = hello_a.binding_id.clone().expect("binding_id");
    let code_a = hello_a.display_code.clone().expect("display_code");

    // 跨过 60 秒挑战窗口。
    *b.clock.0.lock().unwrap() += CHALLENGE_TTL_MS + 1_000;

    // 同钥在新连接上重发 hello：作废旧对、开新对。
    let client_b = TestClient {
        keys: RustSigningKey::from_seed(&keys.seed()),
        conn: factory.connect().await,
        received: Mutex::new(Vec::new()),
    };
    let reply_b = client_b
        .connect_frame(&svc, epoch, [82u8; 32])
        .await
        .expect("重握有应答");
    let ServerFrame::ServerHello(hello_b) = reply_b else {
        panic!("重握应得 serverHello")
    };
    let binding_b = hello_b.binding_id.clone().expect("binding_id");
    let code_b = hello_b.display_code.clone().expect("display_code");

    // ① 旧连接被服务端关闭（RED：旧行为不关，插件挂在 recv 上）。
    assert!(
        !client_a.conn.write_available(),
        "到期后旧待批连接必须被服务端关闭"
    );
    assert!(client_b.conn.write_available());
    // ② 新对与旧对不同（码翻新），pending 视图给的是新对。
    assert_ne!(binding_a, binding_b);
    assert_ne!(code_a, code_b);
    let view = svc.pending_binding().expect("新待批在场");
    assert_eq!(view.binding.binding_id, binding_b);
    // ③ 旧码批不得（记录已摘除）。
    assert!(svc.approve(&binding_a).await.is_err(), "过期旧码不得批准");
    // ④ 新码可批准。
    svc.approve(&binding_b).await.expect("新码应可批准");
}

// ============================================================
// F3（复审 Important-3）：复用待批应答必须带上**记录里的真实到期**。
// 旧行为：T+55s 重连的应答把新挑战的 expiry（now+60s）当到期回显，而内存记录的
// 审批截止仍是原始的 T+60s——设置卡上那个码最多有约 55 秒「看得见但批不了」
// （用户点了就是 AUTH_EXPIRED）。
// ============================================================

/// 有效期内同钥重连：bindingId/displayCode 幂等不变，`pendingExpiresAt` 必须等于
/// **记录原始到期**（不是本次挑战的 expiry）。
#[tokio::test]
async fn reused_pending_reply_carries_the_records_true_deadline() {
    let b = bench();
    let (svc, factory) = served(&b).await;
    let epoch = current_epoch(&b.env);

    let keys = RustSigningKey::generate();
    let client_a = TestClient {
        keys: RustSigningKey::from_seed(&keys.seed()),
        conn: factory.connect().await,
        received: Mutex::new(Vec::new()),
    };
    let reply_a = client_a
        .connect_frame(&svc, epoch, [91u8; 32])
        .await
        .expect("首握手有应答");
    let ServerFrame::ServerHello(hello_a) = reply_a else {
        panic!("首绑应得 serverHello")
    };
    let binding_a = hello_a.binding_id.clone().expect("binding_id");
    let record_deadline = hello_a.pending_expires_at.expect("待批应答必须带记录到期");

    // ~T+55s 同钥重连（仍在有效期内，幂等复用同一对）。
    *b.clock.0.lock().unwrap() += 55_000;
    let client_b = TestClient {
        keys: RustSigningKey::from_seed(&keys.seed()),
        conn: factory.connect().await,
        received: Mutex::new(Vec::new()),
    };
    let reply_b = client_b
        .connect_frame(&svc, epoch, [92u8; 32])
        .await
        .expect("重连有应答");
    let ServerFrame::ServerHello(hello_b) = reply_b else {
        panic!("重连应得 serverHello")
    };

    assert_eq!(
        hello_b.binding_id.as_deref(),
        Some(binding_a.as_str()),
        "有效期内幂等同一对"
    );
    assert_eq!(
        hello_b.pending_expires_at,
        Some(record_deadline),
        "复用应答必须回记录原始到期，不得回显本次挑战的新 expiry"
    );
    assert!(
        hello_b.expiry > record_deadline,
        "本次挑战 expiry（{}）本就晚于记录到期（{record_deadline}）——回显它会让码看起来更长寿",
        hello_b.expiry
    );
    // 记录到期时刻仍可批准（显示的真实到期是可操作的真值）。
    svc.approve(&binding_a).await.expect("记录有效期内应可批准");
}

/// 已授权路径不带该字段（只对待批面有意义）。
#[tokio::test]
async fn authorized_reply_has_no_pending_deadline() {
    let b = bench();
    let (svc, factory) = served(&b).await;
    let epoch = current_epoch(&b.env);

    let keys = RustSigningKey::generate();
    let binding_id = bind_and_deliver(&svc, &factory, &keys, epoch, [93u8; 32]).await;

    // 同钥重连（已钉公钥）→ authorized。
    let client = TestClient {
        keys: RustSigningKey::from_seed(&keys.seed()),
        conn: factory.connect().await,
        received: Mutex::new(Vec::new()),
    };
    let reply = client
        .connect_frame(&svc, epoch, [94u8; 32])
        .await
        .expect("重连有应答");
    let ServerFrame::ServerHello(hello) = reply else {
        panic!("应得 serverHello")
    };
    assert_eq!(hello.state, "authorized");
    assert_eq!(hello.binding_id.as_deref(), Some(binding_id.as_str()));
    assert_eq!(hello.pending_expires_at, None, "authorized 不带待批到期");
}

// ============================================================
// F4（复审 Important-4）：待批视图不得给出**已过期**记录的死码。
// 旧行为：`pending_binding()` 直接返回 `last()`，不看过期——`dsh_binding_start`
// 会把一个已经批不了（approve → AUTH_EXPIRED）的核对码显示给用户。
// ============================================================

/// hello → 时钟跨过 TTL → `pending_binding()` 必须返回 None，且该记录的连接被关闭；
/// 随后的全新 hello 建出**新对**（旧记录已清）。
#[tokio::test]
async fn pending_binding_view_hides_expired_records() {
    let b = bench();
    let (svc, factory) = served(&b).await;
    let epoch = current_epoch(&b.env);

    let keys = RustSigningKey::generate();
    let client_a = TestClient {
        keys: RustSigningKey::from_seed(&keys.seed()),
        conn: factory.connect().await,
        received: Mutex::new(Vec::new()),
    };
    let reply_a = client_a
        .connect_frame(&svc, epoch, [95u8; 32])
        .await
        .expect("首握手有应答");
    let ServerFrame::ServerHello(hello_a) = reply_a else {
        panic!("首绑应得 serverHello")
    };
    let binding_a = hello_a.binding_id.clone().expect("binding_id");
    let code_a = hello_a.display_code.clone().expect("display_code");
    assert!(svc.pending_binding().is_some(), "有效期内待批可见");

    // 跨过 TTL（无任何新 hello 触发清扫）。
    *b.clock.0.lock().unwrap() += CHALLENGE_TTL_MS + 1_000;

    // ① 视图不得再给出这条死码。
    assert!(
        svc.pending_binding().is_none(),
        "过期记录不得出现在待批视图（否则设置卡展示不可批准的码）"
    );
    // ② 它的连接被一并关闭（持有旧码的一端被告知作废）。
    assert!(!client_a.conn.write_available(), "过期记录的连接必须被关闭");

    // ③ 全新 hello 建新对（旧记录已不在，不会复用）。
    let client_b = TestClient {
        keys: RustSigningKey::from_seed(&keys.seed()),
        conn: factory.connect().await,
        received: Mutex::new(Vec::new()),
    };
    let reply_b = client_b
        .connect_frame(&svc, epoch, [96u8; 32])
        .await
        .expect("新握手有应答");
    let ServerFrame::ServerHello(hello_b) = reply_b else {
        panic!("新绑应得 serverHello")
    };
    let binding_b = hello_b.binding_id.clone().expect("binding_id");
    assert_ne!(binding_a, binding_b, "过期后必须换新对");
    assert_ne!(code_a, hello_b.display_code.clone().expect("display_code"));
    let view = svc.pending_binding().expect("新待批可见");
    assert_eq!(view.binding.binding_id, binding_b);
}

// ============================================================
// F5（复审 Important-5）：同一条连接上重发 hello 不得把自己关掉。
// 旧行为：幂等复用分支把 `existing.conn`（就是这条 conn）当 stale 关掉——应答线
// 当场断，客户端既收不到 server_hello 也发不出后续帧。
// ============================================================

/// 同一 socket 上连发两次 hello（幂等同一对）：两次都应收到应答，连接保持可用。
#[tokio::test]
async fn repeated_hello_on_the_same_socket_keeps_the_reply_line_open() {
    let b = bench();
    let (svc, factory) = served(&b).await;
    let epoch = current_epoch(&b.env);

    let keys = RustSigningKey::generate();
    let client = TestClient {
        keys: RustSigningKey::from_seed(&keys.seed()),
        conn: factory.connect().await,
        received: Mutex::new(Vec::new()),
    };

    let reply_a = client
        .connect_frame(&svc, epoch, [97u8; 32])
        .await
        .expect("首 hello 有应答");
    let ServerFrame::ServerHello(hello_a) = reply_a else {
        panic!("首绑应得 serverHello")
    };
    let binding_a = hello_a.binding_id.clone().expect("binding_id");
    let code_a = hello_a.display_code.clone().expect("display_code");

    // 同一 socket 再发一次 hello（读循环重试的真实形态）。
    //
    // 超时包裹：修复前这条连接会被自己关掉，`recv` 永远不返回（挂死而不是报错）——
    // 有界等待把它变成一个明确的失败（RED → GREEN）。
    assert!(client.write_available(), "首 hello 后连接必须仍可用");
    let reply_b = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        client.connect_frame(&svc, epoch, [98u8; 32]),
    )
    .await
    .expect("同连接重发 hello 必须在有界窗口内得到应答（不得自关应答线）")
    .expect("同连接重发 hello 必须仍收到应答");
    let ServerFrame::ServerHello(hello_b) = reply_b else {
        panic!("重发应得 serverHello")
    };

    // 幂等同一对（有效期内），且连接仍可写。
    assert_eq!(hello_b.binding_id.as_deref(), Some(binding_a.as_str()));
    assert_eq!(hello_b.display_code.as_deref(), Some(code_a.as_str()));
    assert!(client.write_available(), "同连接重发 hello 不得自关应答线");
    // 仍可批准并交付（线路完整）。
    svc.approve(&binding_a).await.expect("批准");
    let _ = client.drain().await;
    let ack = client
        .send_credential(&binding_a, "dsh-auth-f5=cookie")
        .await
        .expect("交付有应答");
    assert!(
        matches!(ack, ServerFrame::Granted { .. }),
        "凭据被接受：{ack:?}"
    );
}

// ============================================================
// F6（复审 Important-6）→ T09-G 分槽后的形态：句柄按 binding_id 归属——
// 「另一绑定的撤权状态不影响本绑定；本绑定自己的闸门决定本绑定」。
// 旧单槽下归属核验必须先于闸门核验（问 A 不得答 B 的闸门）；分槽后
// `authorize_grant` 取的就是调用方自己的槽，归属核验成为纵深防御断言。
// 本用例守护其行为面：A、B 都已交付时撤 B，A 仍取到 **A 交付的** cookie
// （不被 B 的撤权扣押、也不被 B 的凭据顶替），B 自己被自己的闸门拒绝。
// ============================================================

/// （T09-G 改写自 `ownership_is_checked_before_the_other_bindings_gate`：旧构造
/// 依赖「撤权清单槽」，分槽后该前提不复存在，按计划复核为正向归属断言。）
///
/// A 先交付、B 后交付、撤 B：A 的授权仍成功且秘密仍是 cookie_a；B 的授权得
/// `DSH_PEER_REVOKED`（本绑定自己的闸门决定本绑定）。
#[tokio::test]
async fn other_bindings_revocation_neither_withholds_nor_substitutes_this_bindings_credential() {
    let b = bench();
    let authority = spawn_fake_mux_host().await;
    *b.procs.listeners.lock().unwrap() = vec![DshListener {
        owner_pid: HOST_PID,
        authority: authority.clone(),
    }];
    let (svc, factory) = served(&b).await;
    let epoch = current_epoch(&b.env);

    let keys_a = RustSigningKey::generate();
    let keys_b = RustSigningKey::generate();
    let cookie_a = "dsh-auth-t09g=owner-a";
    let cookie_b = "dsh-auth-t09g=owner-b";
    let binding_a =
        bind_and_deliver_with_cookie(&svc, &factory, &keys_a, epoch, [99u8; 32], cookie_a).await;
    let binding_b =
        bind_and_deliver_with_cookie(&svc, &factory, &keys_b, epoch, [100u8; 32], cookie_b).await;
    assert_ne!(binding_a, binding_b);

    // 撤 B：只翻转 B 的闸门、只摘 B 的句柄条目。
    svc.revoke(&binding_b).await.expect("撤权 B");

    let host = identity::discover(&b.env).expect("宿主在跑");

    // B：本绑定自己的闸门决定本绑定（不得因分槽变成「还在场」）。
    let err_b = svc
        .authorize_grant(&host, &host_identity(&keys_b))
        .await
        .expect_err("B 已被撤，不得再取句柄");
    assert_eq!(err_b.code, codes::PEER_REVOKED, "实际：{err_b:?}");

    // A：另一绑定的撤权既不扣押也不顶替本绑定凭据。
    let peer_a = svc
        .authorize_grant(&host, &host_identity(&keys_a))
        .await
        .expect("撤 B 不得扣押 A 已交付的凭据");
    assert_eq!(
        peer_a.binding_id(),
        binding_a,
        "A 的句柄必须钉在 A 的绑定上"
    );
    assert_eq!(
        peer_a.secret(),
        cookie_a.as_bytes(),
        "A 拿到的必须是 A 交付的 cookie（不是 B 的，也不因撤 B 被清）"
    );
}

// ============================================================
// C-6（T09-G 复审 Nit-3）：撤权槽位观测面
// ============================================================

/// C-6：撤权只移除**该绑定自己**的已交付槽位——经 `has_delivered_peer`
/// （`#[doc(hidden)]` 观测 API，只报存在性、不导出句柄/字节）直接锁定
/// `revoke_local` 的 `peers.remove(binding_id)` 粒度：撤 A 后 A 槽消失、B 槽保留
/// （不是 `peers.clear()`）。与行为面用例 `revoking_a_leaves_b_credential_intact`
/// 互补：那条走 wire 路径观察句柄可用性，这条直接观测槽位存在性。
#[tokio::test]
async fn revoke_removes_the_bindings_delivered_peer() {
    let b = bench();
    let authority = spawn_fake_mux_host().await;
    *b.procs.listeners.lock().unwrap() = vec![DshListener {
        owner_pid: HOST_PID,
        authority: authority.clone(),
    }];
    let (svc, factory) = served(&b).await;
    let epoch = current_epoch(&b.env);

    let keys_a = RustSigningKey::generate();
    let keys_b = RustSigningKey::generate();
    let binding_a = bind_and_deliver_with_cookie(
        &svc,
        &factory,
        &keys_a,
        epoch,
        [201u8; 32],
        "dsh-auth-t09g=revoke-a",
    )
    .await;
    let binding_b = bind_and_deliver_with_cookie(
        &svc,
        &factory,
        &keys_b,
        epoch,
        [202u8; 32],
        "dsh-auth-t09g=revoke-b",
    )
    .await;
    assert_ne!(binding_a, binding_b);

    // 前提：两绑定都已交付（各自槽位在场）。
    assert!(svc.has_delivered_peer(&binding_a), "A 已交付 → 槽位在场");
    assert!(svc.has_delivered_peer(&binding_b), "B 已交付 → 槽位在场");

    svc.revoke(&binding_a).await.expect("撤权 A");

    assert!(
        !svc.has_delivered_peer(&binding_a),
        "撤 A 必须移除 A 自己的已交付槽位"
    );
    assert!(
        svc.has_delivered_peer(&binding_b),
        "撤 A 不得牵连删除 B 的已交付槽位（peers.remove 不是 clear）"
    );
}

// ============================================================
// 第一关 P1-2：卸载撤权撤销**全部**匹配绑定（旧实现只撤最新一条 Bound）
// ============================================================

/// 同 [`bind_and_deliver_with_cookie`]，但返回活连接观测面（撤权关闭断言用）。
async fn bind_and_deliver_keeping_client(
    svc: &Arc<dsh_auth::DshAuthService>,
    factory: &Arc<MemClientFactory>,
    keys: &RustSigningKey,
    epoch: u64,
    nonce: [u8; 32],
    cookie: &str,
) -> (String, TestClient) {
    let client = TestClient {
        keys: RustSigningKey::from_seed(&keys.seed()),
        conn: factory.connect().await,
        received: Mutex::new(Vec::new()),
    };
    let reply = client
        .connect_frame(svc, epoch, nonce)
        .await
        .expect("握手有应答");
    let ServerFrame::ServerHello(hello) = reply else {
        panic!("首绑应得 serverHello，得到 {reply:?}")
    };
    let binding_id = hello.binding_id.clone().expect("binding_id");
    svc.approve(&binding_id).await.expect("用户批准");
    let _ = client.drain().await;
    let ack = client
        .send_credential(&binding_id, cookie)
        .await
        .expect("交付有应答");
    assert!(
        matches!(ack, ServerFrame::Granted { .. }),
        "凭据被接受：{ack:?}"
    );
    (binding_id, client)
}

/// 生产撤权面：A/B 同 profile 两把不同公钥、各自交付不同 Cookie——卸载撤权必须
/// 撤**全部**匹配行（旧实现 `.rev().find()` 只撤最新一条，A 会被漏撤、免除批准
/// 复活），并把两条活连接都关掉。
#[tokio::test]
async fn uninstall_revokes_every_matching_binding_and_closes_all_live_connections() {
    let b = bench();
    let (svc, factory) = served(&b).await;
    let epoch = current_epoch(&b.env);

    let keys_a = RustSigningKey::generate();
    let (binding_a, client_a) =
        bind_and_deliver_keeping_client(&svc, &factory, &keys_a, epoch, [131u8; 32], "cookie-a")
            .await;
    let keys_b = RustSigningKey::generate();
    let (binding_b, client_b) =
        bind_and_deliver_keeping_client(&svc, &factory, &keys_b, epoch, [132u8; 32], "cookie-b")
            .await;

    // 前置：两条绑定都能取到自己的句柄，两条连接都活着。
    let host = identity::discover(&b.env).expect("宿主在跑");
    for (keys, expected) in [(&keys_a, "cookie-a"), (&keys_b, "cookie-b")] {
        let peer = svc
            .authorize_grant(
                &host,
                &ClientIdentity {
                    public_key_spki: keys.spki.clone(),
                    executable: host.executable.clone(),
                    pid: host.pid,
                    started_at_ms: host.started_at_ms,
                },
            )
            .await
            .expect("A/B 各自可取句柄");
        assert_eq!(peer.secret(), expected.as_bytes());
    }
    assert!(client_a.write_available() && client_b.write_available());

    // 卸载撤权（生产入口；域可用路径）。
    ProductionBindingRevoker::revoke_with(
        Ok(Arc::clone(&svc)),
        &b.env,
        &host.profile_dir.to_string_lossy(),
    )
    .expect("撤权必须成功");

    // 全部匹配绑定都成墓碑；两条活连接都被关闭；句柄槽位清空。
    assert_eq!(
        svc.authorization_state(),
        AuthorizationState::Revoked,
        "两行都必须 revoked"
    );
    assert!(!svc.has_delivered_peer(&binding_a), "A 句柄槽必须清");
    assert!(!svc.has_delivered_peer(&binding_b), "B 句柄槽必须清");
    assert!(!client_a.write_available(), "A 的活连接必须被关闭");
    assert!(!client_b.write_available(), "B 的活连接必须被关闭");

    // 旧钥重连一律 PEER_REVOKED。
    for keys in [&keys_a, &keys_b] {
        let err = svc
            .authorize_grant(
                &host,
                &ClientIdentity {
                    public_key_spki: keys.spki.clone(),
                    executable: host.executable.clone(),
                    pid: host.pid,
                    started_at_ms: host.started_at_ms,
                },
            )
            .await
            .expect_err("撤权后旧钥不得再取句柄");
        assert_eq!(err.code, codes::PEER_REVOKED);
    }
    let _ = (binding_a, binding_b);
}

/// 兄弟 profile 的绑定不得被这次卸载牵连（撤全部匹配 ≠ 撤全表）；同一 profile
/// 的多行（A/B）必须**全部**撤（旧实现只撤最新一条）。
#[tokio::test]
async fn uninstall_revokes_all_matching_rows_and_only_those() {
    let sibling = format!("{WIN_PROFILE_TOKEN}-2");
    let rb = RevokeBench::with_ledger(vec![
        ledger_row("binding-a", WIN_PROFILE_TOKEN, "bound"),
        ledger_row("binding-b", WIN_PROFILE_TOKEN, "bound"),
        ledger_row("binding-sibling", &sibling, "bound"),
    ]);
    ProductionBindingRevoker::revoke_with(Ok(Arc::clone(&rb.svc)), &rb.env, WIN_PROFILE_TOKEN)
        .expect("撤权必须成功");

    let states = rb.ledger_states();
    let by_id = |id: &str| {
        states
            .iter()
            .find(|(b, _)| b == id)
            .map(|(_, s)| s.as_str())
    };
    assert_eq!(
        by_id("binding-a"),
        Some("revoked"),
        "A 必须被撤（不能只撤最新一条）"
    );
    assert_eq!(by_id("binding-b"), Some("revoked"), "B 必须被撤");
    assert_eq!(
        by_id("binding-sibling"),
        Some("bound"),
        "兄弟 profile 绑定不得被牵连"
    );
}

/// 域不可用（账本兜底）与域正常两条路径的撤权范围必须一致：同一份 A/B + 兄弟
/// 账本分别走两路，撤后状态逐行相同。
#[tokio::test]
async fn ledger_only_and_live_paths_revoke_the_same_set() {
    let sibling = format!("{WIN_PROFILE_TOKEN}-2");
    let rows = || {
        vec![
            ledger_row("binding-a", WIN_PROFILE_TOKEN, "bound"),
            ledger_row("binding-b", WIN_PROFILE_TOKEN, "bound"),
            ledger_row("binding-sibling", &sibling, "bound"),
        ]
    };

    // 域不可用路径。
    let down = RevokeBench::with_ledger(rows());
    ProductionBindingRevoker::revoke_with(
        Err(DshError::new(codes::IPC_STATE, "domain down (test)")),
        &down.env,
        WIN_PROFILE_TOKEN,
    )
    .expect("域不可用路径不得堵死卸载出口");
    let down_states = down.ledger_states();

    // 域正常路径。
    let live = RevokeBench::with_ledger(rows());
    ProductionBindingRevoker::revoke_with(Ok(Arc::clone(&live.svc)), &live.env, WIN_PROFILE_TOKEN)
        .expect("域正常路径撤权成功");
    let live_states = live.ledger_states();

    assert_eq!(
        down_states, live_states,
        "域不可用与域正常两条路径的撤权范围必须一致"
    );
    assert_eq!(
        down_states
            .iter()
            .filter(|(id, state)| id != "binding-sibling" && state == "revoked")
            .count(),
        2,
        "两行同 profile 绑定都必须 revoked"
    );
}

/// 任一真实撤销失败不得继续到「成功」：A/B 两行、第二行落盘必败（占位
/// `bindings.tmp` 目录使 atomic_write 失败的后一轮不可行——改用 A 撤成功后再
/// 制造失败不可行，故用**只含两行且写入必败**的账本验证失败即 REVOKE_FAILED，
/// 且内存不得谎报已撤完）。
#[tokio::test]
async fn uninstall_fails_when_any_revocation_write_fails() {
    let rb = RevokeBench::with_ledger(vec![
        ledger_row("binding-a", WIN_PROFILE_TOKEN, "bound"),
        ledger_row("binding-b", WIN_PROFILE_TOKEN, "bound"),
    ]);
    // `atomic_write_0600` 先写 `<path>.tmp` 再 rename：把该位置占成目录 → 每次写都失败。
    std::fs::create_dir_all(rb.state_dir.join("bindings.tmp")).expect("占位目录可建");
    let err =
        ProductionBindingRevoker::revoke_with(Ok(Arc::clone(&rb.svc)), &rb.env, WIN_PROFILE_TOKEN)
            .expect_err("撤权真失败时不得静默放行");
    assert_eq!(err.code, codes::REVOKE_FAILED, "必须如实上报：{err:?}");
    // Round-8 Ruling 9：已撤的行内存如实记 Revoked（不回滚）；未撤到的行仍是
    // Bound → 授权状态保持 Bound（不得谎报全撤——A 翻转、B 未撤，总态仍 Bound）。
    assert_eq!(
        rb.svc.authorization_state(),
        AuthorizationState::Bound,
        "撤权未撤完（binding-b 未撤）时总授权状态必须保持 Bound"
    );
}

/// 复审 M-2：**先撤 B 再走卸载**——B 在收集时已不是 Bound，本就不入列表；
/// 该用例锁定的是「已撤的行不挡卸载、A 仍被撤」这一后置条件（对旧实现亦通过，
/// 如实申报为**非 RED** 的语义锁定用例）。
///
/// 真正的「收集后、逐条撤权间被并发撤掉」窗口无法从公开 API 稳定驱动（需要
/// 在 `revoke_with` 内部注入步骤）；M-2 的代码分支已按「`AUTH_UNKNOWN_BINDING`
/// → 重读账本确认非 Bound → 计为已撤」实现并注释申报，测试边界如实记入报告。
#[tokio::test]
async fn pre_revoked_binding_does_not_block_uninstall_and_others_still_revoke() {
    let b = bench();
    let (svc, factory) = served(&b).await;
    let epoch = current_epoch(&b.env);
    let keys_a = RustSigningKey::generate();
    let binding_a =
        bind_and_deliver_with_cookie(&svc, &factory, &keys_a, epoch, [141u8; 32], "cookie-a").await;
    let keys_b = RustSigningKey::generate();
    let binding_b =
        bind_and_deliver_with_cookie(&svc, &factory, &keys_b, epoch, [142u8; 32], "cookie-b").await;

    // 模拟交互撤权先撤掉 B（并发窗口里发生的那一步）。
    svc.revoke(&binding_b).await.expect("先撤 B");

    let profile = identity::discover(&b.env)
        .expect("宿主在跑")
        .profile_dir
        .to_string_lossy()
        .to_string();
    ProductionBindingRevoker::revoke_with(Ok(Arc::clone(&svc)), &b.env, &profile)
        .expect("并发已撤的绑定不得让卸载失败");

    assert_eq!(svc.authorization_state(), AuthorizationState::Revoked);
    assert!(!svc.has_delivered_peer(&binding_a));
    assert!(!svc.has_delivered_peer(&binding_b));
}

// ============================================================
// M2（T10 复审）：peer_for_control 的三道门（闸门/当代/authority）
// ============================================================

/// Bound + 已交付 + 当代 → Some；旧 epoch / authority 漂移 / 撤权 / 未交付 → None。
#[tokio::test]
async fn peer_for_control_gates_on_generation_revocation_and_delivery() {
    let b = bench();
    let (svc, factory) = served(&b).await;
    let epoch = current_epoch(&b.env);
    let keys = RustSigningKey::generate();
    let binding =
        bind_and_deliver_with_cookie(&svc, &factory, &keys, epoch, [151u8; 32], "cookie-ctl").await;

    // 当代 + 已交付 → Some。
    let host = identity::discover(&b.env).expect("宿主在跑");
    let peer = svc.peer_for_control().await.expect("当代已交付应给出句柄");
    assert_eq!(peer.binding_id(), binding);

    // 撤权 → None。
    svc.revoke(&binding).await.expect("撤权");
    assert!(svc.peer_for_control().await.is_none(), "撤权后不得给出句柄");

    // 未交付（已批准但凭据未经 IPC 交付）→ None：批准一个新握手但不 send_credential。
    let client2 = TestClient {
        keys: RustSigningKey::generate(),
        conn: factory.connect().await,
        received: Mutex::new(Vec::new()),
    };
    let reply2 = client2
        .connect_frame(&svc, epoch, [152u8; 32])
        .await
        .expect("握手");
    let ServerFrame::ServerHello(hello2) = reply2 else {
        panic!("应得 serverHello")
    };
    let binding3 = hello2.binding_id.clone().expect("binding_id");
    svc.approve(&binding3).await.expect("批准");
    let _ = client2.drain().await;
    assert!(
        svc.peer_for_control().await.is_none(),
        "撤权后即便存在已批准未交付的行，也不得给出 peer"
    );
    let _ = host;
}

/// 旧代际句柄（换代未重交付）→ None（不重盖章）。
#[tokio::test]
async fn peer_for_control_refuses_stale_generation() {
    let b = bench();
    let (svc, factory) = served(&b).await;
    let epoch = current_epoch(&b.env);
    let keys = RustSigningKey::generate();
    let binding =
        bind_and_deliver_with_cookie(&svc, &factory, &keys, epoch, [153u8; 32], "cookie-old").await;
    assert!(svc.peer_for_control().await.is_some(), "前置：当代可用");

    // 换代：stale_generation 用例同款手法——直接改 bench 进程表里的宿主行启动时刻。
    {
        let mut procs = b.procs.processes.lock().unwrap();
        if let Some(host_row) = procs
            .iter_mut()
            .find(|p| p.exe.to_string_lossy().contains("DeepSeek Harness"))
        {
            host_row.started_at_ms += 7;
        }
    }
    let _ = binding;
    assert!(
        svc.peer_for_control().await.is_none(),
        "换代后未重交付的旧代句柄不得给 hub"
    );
}

// ============================================================
// H05：按实例的插件、授权与就绪生命周期
//
// 双活实例夹具（Desktop A/B，不同 home/profile）+ 实例作用域 auth 域
// （`init_instance_auth` 装配内核；state/profile/peer/gate/授权意图全按实例隔离）。
// 观测纪律不变：变量全部来自真实生产入口返回与 wire 帧实例。
// ============================================================

use multi_agents_manager_lib::services::dsh_control::instances::{
    InstanceIdentityProbe, InstanceRegistry, SelectedInstance,
};
use multi_agents_manager_lib::services::dsh_control::production as dsh_instance_production;

/// 真实实例凭据接线使用进程级控制台单例。不同测试会复用固定 ID，而 legacy
/// 撤权会推进全局序号（ID 唯一也不够）。让需要成功交付的夹具各占一个测试进程；
/// 外层测试仍可并行，同一用例内的 A/B 域仍用同一个真实单例。
fn run_instance_credential_fixture_in_child(test: &str) -> bool {
    const CHILD: &str = "TUVIS_TEST_DSH_AUTH_FIXTURE";
    const CHILD_HOME: &str = "TUVIS_TEST_DSH_AUTH_FIXTURE_HOME";
    // Windows Known Folder 不服从 HOME；没有 debug 覆盖时，启动前即拒绝。
    if cfg!(all(windows, not(debug_assertions))) {
        panic!("Windows 实例授权测试需要 debug assertions 才能隔离数据根");
    }
    if std::env::var(CHILD).as_deref() == Ok(test) {
        let home = PathBuf::from(std::env::var_os(CHILD_HOME).expect("父进程必须传递夹具根"));
        assert!(
            home.is_absolute() && home.is_dir(),
            "夹具根必须是现存的绝对路径"
        );
        // 走实际生产路径解析，在 fixture body（以及凭据触发的全局 DB）之前验根。
        let env = dsh_instance_production::production_env();
        assert_eq!(env.tuvis_home, home.join(".tuvis"));
        assert_eq!(env.home, home.join(".dsh"));
        assert!(env.dsh_state_dir.is_none());
        return false;
    }
    let home = tempfile::tempdir().expect("独立测试 HOME");
    let mut command = std::process::Command::new(std::env::current_exe().expect("测试二进制"));
    command
        .args(["--exact", test, "--color", "never"])
        .env(CHILD, test)
        .env(CHILD_HOME, home.path())
        .env("HOME", home.path())
        .env("CFFIXED_USER_HOME", home.path())
        .env_remove("TUVIS_HOME")
        .env_remove("DSH_HOME")
        .env_remove("TUVIS_DSH_STATE_DIR");
    #[cfg(all(windows, debug_assertions))]
    command
        .env("TUVIS_HOME", home.path())
        .env("DSH_HOME", home.path().join(".dsh"));
    let output = command.output().expect("运行独立实例凭据夹具");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success() && stdout.contains("1 passed; 0 failed;"),
        "{test}: {}\n{stdout}\n{}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    true
}

/// 实例 B 的 home（Desktop 形态；与 A 同 kind 不同 home → 派生不同实例 ID）。
const WIN_HOME_B: &str = r"C:\Users\bunny\.dsh-two";

/// 实例 B 的 profile 令牌（由 home 派生——与 `join_lexical` 同一词法形态，
/// 令牌单源避免两处字面量漂移）。
fn win_profile_b() -> String {
    join_lexical(WIN_HOME_B, &["profiles", "desktop"])
        .to_string_lossy()
        .to_string()
}

/// 宿主进程事实（A 沿用既有 HOST_PID/HOST_STARTED_AT；B 独立 PID/启动时刻）。
const HOST_PID_B: u32 = 41002;
const HOST_B_STARTED_AT: i64 = 1_760_010_000_000;

/// 通用 Desktop 宿主进程行（argv 携带 profile 令牌——identity 候选的权威证据）。
fn desktop_host_process_for(pid: u32, profile_token: &str, started_at_ms: i64) -> DshProcess {
    DshProcess {
        pid,
        exe: PathBuf::from(format!(r"{WIN_INSTALL_DIR}\DeepSeek Harness.exe")),
        cmdline: vec![
            format!(r"{WIN_INSTALL_DIR}\DeepSeek Harness.exe"),
            "--expose-internals".into(),
            format!(
                r"{WIN_INSTALL_DIR}\resources\app.asar\dsh\node_modules\@deepseek-ai\dsh-desktop-host\lib\index.js"
            ),
            format!(r"{WIN_INSTALL_DIR}\resources\app.asar\dsh"),
            profile_token.into(),
            format!(r"{WIN_INSTALL_DIR}\resources\runtime\primary-runtime"),
        ],
        started_at_ms,
    }
}

fn seed_profile_files(files: &mut FakeFiles, profile_token: &str) {
    // profile_token 本身就是 profile 目录（`<home>\profiles\desktop`）；目录键 =
    // 该词法路径，标记文件挂其下（与既有 bench 的 WIN_HOME 种法同形）。
    files.add_dir(PathBuf::from(profile_token.trim_end_matches(['/', '\\'])));
    files.set_file(join_lexical(profile_token, &["package.json"]), b"{}");
}

/// 双活实例夹具：Desktop A（既有 WIN_HOME）+ Desktop B（WIN_HOME_B）同时在场，
/// 各自有回环监听；两份 profile 均初始化。
struct DualBench {
    clock: Arc<VarClock>,
    env: DshEnvironment,
    procs: Arc<SharedProcs>,
    _tmp: tempfile::TempDir,
}

fn dual_bench() -> DualBench {
    let mut files = FakeFiles::default();
    let dir = PathBuf::from(WIN_INSTALL_DIR);
    files.add_dir(dir.clone());
    files.set_file(
        join_lexical(&dir.to_string_lossy(), &["DeepSeek Harness.exe"]),
        b"mz",
    );
    files.set_file(
        join_lexical(&dir.to_string_lossy(), &["resources", "app-update.yml"]),
        b"provider: generic\n",
    );
    seed_profile_files(&mut files, WIN_PROFILE_TOKEN);
    seed_profile_files(&mut files, &win_profile_b());
    let store = Arc::new(Mutex::new(files));
    let _store = Arc::clone(&store);
    let tmp = tempfile::tempdir().expect("tempdir 可建");
    let procs = Arc::new(SharedProcs {
        processes: Mutex::new(vec![
            desktop_host_process_for(HOST_PID, WIN_PROFILE_TOKEN, HOST_STARTED_AT),
            desktop_host_process_for(HOST_PID_B, &win_profile_b(), HOST_B_STARTED_AT),
        ]),
        listeners: Mutex::new(vec![
            DshListener {
                owner_pid: HOST_PID,
                authority: "127.0.0.1:5214".into(),
            },
            DshListener {
                owner_pid: HOST_PID_B,
                authority: "127.0.0.1:5215".into(),
            },
        ]),
    });
    let clock = Arc::new(VarClock(Mutex::new(HOST_STARTED_AT)));
    let env = DshEnvironment {
        home: PathBuf::from(WIN_HOME),
        tuvis_home: tmp.path().join("tuvis-home"),
        dsh_state_dir: None,
        processes: procs.clone(),
        files: Arc::new(StoreSource(store.clone())),
        runner: Arc::new(NullRunner),
        clock: clock.clone(),
        // 测试默认「不可观测」：显式拒绝 Web home 归属（fail-closed），
        // 绝不让假进程源落到真实系统 PID 上。需要 Web 归属的用例自行注入假观测器。
        environments: std::sync::Arc::new(
            multi_agents_manager_lib::services::dsh_control::model::TestUnobservableEnvironments,
        ),
    };
    DualBench {
        clock,
        env,
        procs,
        _tmp: tmp,
    }
}

/// 只推进实例 A 的启动时刻（B 保持原样）——同 exe/profile 的**换代**形态，
/// 不是「换了一个实例」。
fn bump_instance_a_started_at(procs: &SharedProcs, started_at_ms: i64) {
    let mut processes = procs.processes.lock().unwrap();
    let pos = processes
        .iter()
        .position(|p| p.pid == HOST_PID)
        .expect("夹具实例 A 必须在场");
    processes[pos] = desktop_host_process_for(HOST_PID, WIN_PROFILE_TOKEN, started_at_ms);
}

/// 夹具实例 ID（登记侧派生单源；夹具 profile 与 bench 相同 → 派生一致）。
fn fixture_instance_a() -> String {
    instance_id_for(HostKind::Desktop, Path::new(WIN_PROFILE_TOKEN))
}

fn fixture_instance_b() -> String {
    instance_id_for(HostKind::Desktop, Path::new(&win_profile_b()))
}

/// 实例作用域 auth 域装配 + mem 传输（peer OS 身份 = 该实例宿主 PID）。
async fn served_instance(
    env: &DshEnvironment,
    instance_id: &str,
    peer_pid: u32,
) -> (Arc<dsh_auth::DshAuthService>, Arc<MemClientFactory>) {
    let peer_os = PeerOsIdentity {
        pid: peer_pid,
        uid: Some(TEST_UID),
    };
    let (listener, factory) = mem_listener(peer_os);
    let svc = Arc::new(
        dsh_instance_production::init_instance_auth(env, instance_id)
            .expect("实例 auth 域可装配")
            .with_listener(Box::new(listener)),
    );
    Arc::clone(&svc).serve().await.expect("IPC 服务启动");
    (svc, factory)
}

/// 实例当代活体的代际（与被测服务同一解析单源）。
fn instance_epoch(env: &DshEnvironment, instance_id: &str) -> u64 {
    identity::discover_instance(env, instance_id)
        .expect("实例在场")
        .epoch
}

/// 实例作用域测试客户端：hello 事实（pid/started/hostId）按目标实例给定。
struct InstanceClient {
    keys: RustSigningKey,
    conn: MemClient,
    pid: u32,
    started_at_ms: i64,
    host_id: String,
    /// v2 home 声明；A/B 使用各自夹具根，临时目录夹具须显式覆盖。
    home_root: String,
    /// 自报可执行文件（OS 对表用；真实临时目录夹具为 Mac 形态宿主）。
    executable: String,
    received: Mutex<Vec<String>>,
}

impl InstanceClient {
    async fn new(
        factory: &Arc<MemClientFactory>,
        pid: u32,
        started_at_ms: i64,
        host_id: String,
    ) -> Self {
        Self {
            keys: RustSigningKey::generate(),
            conn: factory.connect().await,
            pid,
            started_at_ms,
            home_root: if host_id == fixture_instance_b() {
                WIN_HOME_B
            } else {
                WIN_HOME
            }
            .into(),
            host_id,
            executable: format!(r"{WIN_INSTALL_DIR}\DeepSeek Harness.exe"),
            received: Mutex::new(Vec::new()),
        }
    }

    async fn with_keys(
        factory: &Arc<MemClientFactory>,
        keys: &RustSigningKey,
        pid: u32,
        started_at_ms: i64,
        host_id: String,
    ) -> Self {
        Self {
            keys: RustSigningKey::from_seed(&keys.seed()),
            conn: factory.connect().await,
            pid,
            started_at_ms,
            home_root: if host_id == fixture_instance_b() {
                WIN_HOME_B
            } else {
                WIN_HOME
            }
            .into(),
            host_id,
            executable: format!(r"{WIN_INSTALL_DIR}\DeepSeek Harness.exe"),
            received: Mutex::new(Vec::new()),
        }
    }

    async fn hello_frame(
        &self,
        svc: &dsh_auth::DshAuthService,
        epoch: u64,
        nonce: [u8; 32],
    ) -> Result<ServerFrame, DshError> {
        self.hello_frame_with_protocol(svc, epoch, nonce, BINDING_PROTOCOL)
            .await
    }

    /// 指定协议代际的 hello（协议迁移纪律用例用未来代际构造）。
    async fn hello_frame_with_protocol(
        &self,
        svc: &dsh_auth::DshAuthService,
        epoch: u64,
        nonce: [u8; 32],
        binding_protocol: u32,
    ) -> Result<ServerFrame, DshError> {
        let now = svc.now_ms();
        let msg = BindingChallengeMessage {
            binding_protocol,
            key_digests: [
                svc.key_digest().to_string(),
                dsh_auth::key_digest(&self.keys.spki),
            ],
            nonce: dsh_auth::b64url_encode(&nonce),
            host_id: self.host_id.clone(),
            epoch: epoch.to_string(),
            expiry: now + CHALLENGE_TTL_MS,
            home_binding: fixture_home_binding(&self.home_root, self.pid, self.started_at_ms),
        };
        let signature = self.keys.sign(&msg.to_signing_bytes());
        let hello = ClientFrame::Hello(HelloFrame {
            binding_protocol,
            public_key_spki: dsh_auth::b64url_encode(&self.keys.spki),
            executable: self.executable.clone(),
            pid: self.pid,
            started_at_ms: self.started_at_ms,
            host_id: self.host_id.clone(),
            epoch: epoch.to_string(),
            nonce: dsh_auth::b64url_encode(&nonce),
            expiry: now + CHALLENGE_TTL_MS,
            home_binding: msg.home_binding.clone(),
            signature: dsh_auth::b64url_encode(&signature),
        });
        self.conn.send(&dsh_auth::client_frame_line(&hello)).await?;
        let line = self.conn.recv().await?;
        self.received.lock().unwrap().push(line.clone());
        dsh_auth::parse_server_frame(&line)
    }

    /// 完整握手；Error 帧折算成 Err。
    async fn connect(
        &self,
        svc: &dsh_auth::DshAuthService,
        epoch: u64,
        nonce: [u8; 32],
    ) -> Result<ServerFrame, DshError> {
        match self.hello_frame(svc, epoch, nonce).await? {
            ServerFrame::Error { error, detail } => Err(DshError {
                code: leak(error.as_str()),
                detail,
            }),
            ok => Ok(ok),
        }
    }

    async fn send_credential(
        &self,
        binding_id: &str,
        cookie: &str,
    ) -> Result<ServerFrame, DshError> {
        let frame = ClientFrame::Credential(CredentialFrame {
            binding_id: binding_id.to_string(),
            cookie: cookie.to_string(),
            browser_cookie: None,
        });
        self.conn.send(&dsh_auth::client_frame_line(&frame)).await?;
        let line = self.conn.recv().await?;
        self.received.lock().unwrap().push(line.clone());
        dsh_auth::parse_server_frame(&line)
    }

    async fn drain(&self) -> Vec<ServerFrame> {
        let mut out = Vec::new();
        while let Some(line) = self.conn.try_recv() {
            self.received.lock().unwrap().push(line.clone());
            if let Ok(frame) = dsh_auth::parse_server_frame(&line) {
                out.push(frame);
            }
        }
        out
    }

    fn write_available(&self) -> bool {
        self.conn.write_available()
    }
}

/// 实例 auth 域的绑定账本路径（按实例隔离子目录）。
fn instance_ledger_file(env: &DshEnvironment, instance_id: &str) -> PathBuf {
    env.tuvis_home
        .join("dsh")
        .join("instances")
        .join(instance_id)
        .join("bindings.json")
}

/// 实例状态子目录（rendezvous / 私钥同在）。
fn instance_state_dir(env: &DshEnvironment, instance_id: &str) -> PathBuf {
    env.tuvis_home
        .join("dsh")
        .join("instances")
        .join(instance_id)
}

/// 一次实例作用域的完整绑定（hello → 批准 → Granted → cookie 交付）。
async fn instance_bind_and_deliver(
    svc: &Arc<dsh_auth::DshAuthService>,
    factory: &Arc<MemClientFactory>,
    facts: (u32, i64, String),
    epoch: u64,
    nonce: [u8; 32],
    cookie: &str,
) -> (InstanceClient, String) {
    let (pid, started_at_ms, host_id) = facts;
    let client = InstanceClient::new(factory, pid, started_at_ms, host_id).await;
    let reply = client
        .connect_frame_authorized(svc, epoch, nonce)
        .await
        .expect("握手有应答");
    let ServerFrame::ServerHello(hello) = reply else {
        panic!("首绑应得 serverHello，得到 {reply:?}")
    };
    assert_eq!(hello.state, "pending_approval");
    let binding_id = hello.binding_id.clone().expect("binding_id");
    svc.approve(&binding_id).await.expect("用户批准");
    let _ = client.drain().await;
    let ack = client
        .send_credential(&binding_id, cookie)
        .await
        .expect("交付有应答");
    assert!(
        matches!(ack, ServerFrame::Granted { .. }),
        "凭据被接受：{ack:?}"
    );
    (client, binding_id)
}

impl InstanceClient {
    /// connect 的别名（等待授权态应答）。
    async fn connect_frame_authorized(
        &self,
        svc: &dsh_auth::DshAuthService,
        epoch: u64,
        nonce: [u8; 32],
    ) -> Result<ServerFrame, DshError> {
        self.connect(svc, epoch, nonce).await
    }
}

// --------------------------------------------------------
// H05-RED-1：authorizing_b_never_grants_a
// --------------------------------------------------------

/// 授权 B 绝不给 A：A/B 实例各自 auth 域（不同私钥/账本/rendezvous），B 的
/// 授权意图、钉公钥、已交付 Cookie 只存在于 B 的域内；A 的 gate/Peer 不可用、
/// A 自己的新钥握手仍走待批。
#[tokio::test]
async fn authorizing_b_never_grants_a() {
    if run_instance_credential_fixture_in_child("authorizing_b_never_grants_a") {
        return;
    }
    let b = dual_bench();
    let (a_id, b_id) = (fixture_instance_a(), fixture_instance_b());
    let (svc_a, factory_a) = served_instance(&b.env, &a_id, HOST_PID).await;
    let (svc_b, factory_b) = served_instance(&b.env, &b_id, HOST_PID_B).await;
    let epoch_a = instance_epoch(&b.env, &a_id);
    let epoch_b = instance_epoch(&b.env, &b_id);

    // B：真实绑定 + Cookie 交付（独立钥）。
    let (client_b, binding_b) = instance_bind_and_deliver(
        &svc_b,
        &factory_b,
        (HOST_PID_B, HOST_B_STARTED_AT, b_id.clone()),
        epoch_b,
        [21u8; 32],
        "owner-cookie-B",
    )
    .await;
    assert_eq!(
        svc_b.authorization_state(),
        AuthorizationState::Bound,
        "B 域内绑定成立"
    );
    assert!(svc_b.has_delivered_peer(&binding_b), "B 的凭据已交付");

    // A：完全无感——无绑定、无待批、gate/Peer 不可用。
    assert_eq!(
        svc_a.authorization_state(),
        AuthorizationState::Unbound,
        "A 的授权域不得被 B 的授权波及"
    );
    assert!(svc_a.pending_binding().is_none(), "A 无待批");
    assert_eq!(svc_a.active_binding_id(), None, "A 无活跃绑定");
    assert!(
        svc_a.peer_for_control().await.is_none(),
        "A 的 gate/Peer 不可用（hub 取不到任何句柄）"
    );
    assert!(
        !instance_ledger_file(&b.env, &a_id).exists(),
        "A 的绑定账本不得被创建（B 的授权不写 A 的域）"
    );

    // A 的自己的新钥握手：仍走待批（B 的授权不跨域放行）。
    let client_a = InstanceClient::new(&factory_a, HOST_PID, HOST_STARTED_AT, a_id.clone()).await;
    let reply_a = client_a
        .connect(&svc_a, epoch_a, [22u8; 32])
        .await
        .expect("A 域握手有应答");
    let ServerFrame::ServerHello(hello_a) = reply_a else {
        panic!("A 的新钥应得 serverHello，得到 {reply_a:?}")
    };
    assert_eq!(
        hello_a.state, "pending_approval",
        "B 的授权绝不给 A 的新钥放行"
    );
    assert_eq!(
        svc_a.authorization_state(),
        AuthorizationState::Unbound,
        "待批不等于绑定"
    );

    // 状态目录隔离：两个实例的 rendezvous 文件独立，各自指向自己的实例。
    let rv_a_path = instance_state_dir(&b.env, &a_id).join("rendezvous.json");
    let rv_b_path = instance_state_dir(&b.env, &b_id).join("rendezvous.json");
    assert!(
        rv_a_path.exists() && rv_b_path.exists(),
        "各自发布 rendezvous"
    );
    let rv_a: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&rv_a_path).unwrap()).unwrap();
    let rv_b: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&rv_b_path).unwrap()).unwrap();
    assert_eq!(rv_a["hostId"], a_id, "A 的 rendezvous 指向 A");
    assert_eq!(rv_b["hostId"], b_id, "B 的 rendezvous 指向 B");
    // 两个域的授权私钥相互独立（不同 digest = 不同信任根）。
    assert_ne!(
        svc_a.key_digest(),
        svc_b.key_digest(),
        "实例 auth 域必须各有各的授权私钥"
    );
    let _ = (client_a, client_b);
}

// --------------------------------------------------------
// H05-RED-2：consent_only_accepts_the_selected_verified_peer
// --------------------------------------------------------

/// 授权意图（用户点击启用）一次性且绑定选定实例范围：通过 OS 身份 + 签名挑战的
/// **该次**新钥握手被自动完成绑定；意图即刻消费（后续新钥回到待批、绝不接受
/// 未来所有 pending）；意图不跨实例；过期意图不再自动放行。
#[tokio::test]
async fn consent_only_accepts_the_selected_verified_peer() {
    if run_instance_credential_fixture_in_child("consent_only_accepts_the_selected_verified_peer") {
        return;
    }
    let b = dual_bench();
    let (a_id, b_id) = (fixture_instance_a(), fixture_instance_b());
    let (svc_a, factory_a) = served_instance(&b.env, &a_id, HOST_PID).await;
    let (svc_b, factory_b) = served_instance(&b.env, &b_id, HOST_PID_B).await;
    let epoch_a = instance_epoch(&b.env, &a_id);
    let epoch_b = instance_epoch(&b.env, &b_id);

    // 用户点击「启用」→ 选定实例（B）范围的一次性授权意图。
    svc_b.begin_authorization_intent().expect("意图可布防");

    // key-1：OS 身份（peercred ↔ 进程表三对）+ 签名挑战全部通过 → 自动完成**该次**绑定。
    let client1 =
        InstanceClient::new(&factory_b, HOST_PID_B, HOST_B_STARTED_AT, b_id.clone()).await;
    let reply1 = client1
        .connect(&svc_b, epoch_b, [31u8; 32])
        .await
        .expect("握手有应答");
    let ServerFrame::ServerHello(hello1) = reply1 else {
        panic!("应得 serverHello，得到 {reply1:?}")
    };
    assert_eq!(
        hello1.state, "authorized",
        "布防意图下通过全部核验的对端被自动完成绑定"
    );
    let binding1 = hello1
        .binding_id
        .clone()
        .expect("authorized 必带 binding_id");
    assert_eq!(svc_b.authorization_state(), AuthorizationState::Bound);
    let ack1 = client1
        .send_credential(&binding1, "owner-cookie-1")
        .await
        .expect("交付有应答");
    assert!(matches!(ack1, ServerFrame::Granted { .. }));
    // 受信连接可用（gate 随批准建立）。
    assert!(client1.write_available());

    // 意图一次性：key-2 的新握手不再自动放行 → 待批（不接受未来所有 pending）。
    let client2 =
        InstanceClient::new(&factory_b, HOST_PID_B, HOST_B_STARTED_AT, b_id.clone()).await;
    let reply2 = client2
        .connect(&svc_b, epoch_b, [32u8; 32])
        .await
        .expect("握手有应答");
    let ServerFrame::ServerHello(hello2) = reply2 else {
        panic!("应得 serverHello，得到 {reply2:?}")
    };
    assert_eq!(
        hello2.state, "pending_approval",
        "意图已消费：后续新钥必须回到人工批准"
    );
    // 人工批准路径仍是一次性动作（按 binding_id 显式批准）。
    let binding2 = hello2.binding_id.clone().expect("pending 必带 binding_id");
    svc_b.approve(&binding2).await.expect("显式批准可受理");

    // 意图不跨实例：A 的域从未布防 → key-3 走待批。
    let client3 = InstanceClient::new(&factory_a, HOST_PID, HOST_STARTED_AT, a_id.clone()).await;
    let reply3 = client3
        .connect(&svc_a, epoch_a, [33u8; 32])
        .await
        .expect("握手有应答");
    let ServerFrame::ServerHello(hello3) = reply3 else {
        panic!("应得 serverHello，得到 {reply3:?}")
    };
    assert_eq!(
        hello3.state, "pending_approval",
        "B 的授权意图绝不自动放行 A 的对端"
    );
    assert_eq!(svc_a.authorization_state(), AuthorizationState::Unbound);

    // OS 身份门：连接 B 的通道却自报 A 的 PID → OS 层对表拒绝（不得借道）。
    let client4 =
        InstanceClient::new(&factory_b, HOST_PID_B, HOST_B_STARTED_AT, b_id.clone()).await;
    let spoof = {
        // 直接发一帧自报 A 的 PID 的 hello（签名事实仍按其自身钥）。
        let now = svc_b.now_ms();
        let nonce = [34u8; 32];
        let msg = BindingChallengeMessage {
            binding_protocol: BINDING_PROTOCOL,
            key_digests: [
                svc_b.key_digest().to_string(),
                dsh_auth::key_digest(&client4.keys.spki),
            ],
            nonce: dsh_auth::b64url_encode(&nonce),
            host_id: b_id.clone(),
            epoch: epoch_b.to_string(),
            expiry: now + CHALLENGE_TTL_MS,
            home_binding: fixture_home_binding(WIN_HOME_B, HOST_PID_B, HOST_B_STARTED_AT),
        };
        let signature = client4.keys.sign(&msg.to_signing_bytes());
        let hello = ClientFrame::Hello(HelloFrame {
            binding_protocol: BINDING_PROTOCOL,
            public_key_spki: dsh_auth::b64url_encode(&client4.keys.spki),
            executable: format!(r"{WIN_INSTALL_DIR}\DeepSeek Harness.exe"),
            pid: HOST_PID,
            started_at_ms: HOST_STARTED_AT,
            host_id: b_id.clone(),
            epoch: epoch_b.to_string(),
            nonce: dsh_auth::b64url_encode(&nonce),
            expiry: now + CHALLENGE_TTL_MS,
            home_binding: msg.home_binding.clone(),
            signature: dsh_auth::b64url_encode(&signature),
        });
        client4
            .conn
            .send(&dsh_auth::client_frame_line(&hello))
            .await
            .expect("发送");
        let line = client4.conn.recv().await.expect("有应答");
        dsh_auth::parse_server_frame(&line).expect("帧可解析")
    };
    let ServerFrame::Error { error, .. } = spoof else {
        panic!("自报他实例 PID 必须被拒，得到 {spoof:?}")
    };
    assert_eq!(error, codes::PEER_MISMATCH, "OS 身份对表失败必须拒绝");

    // 过期意图：重新布防后推进时钟超过一个挑战窗口 → 不再自动放行。
    svc_b.begin_authorization_intent().expect("再次布防");
    let client5 =
        InstanceClient::new(&factory_b, HOST_PID_B, HOST_B_STARTED_AT, b_id.clone()).await;
    b.clock
        .set(svc_b.now_ms() + dsh_auth::CHALLENGE_TTL_MS + 2_000);
    let reply5 = client5
        .connect(&svc_b, epoch_b, [35u8; 32])
        .await
        .expect("握手有应答");
    let ServerFrame::ServerHello(hello5) = reply5 else {
        panic!("应得 serverHello，得到 {reply5:?}")
    };
    assert_eq!(
        hello5.state, "pending_approval",
        "过期意图不得自动放行（不受未来所有 pending）"
    );
}

// --------------------------------------------------------
// H05-RED-3：reinstall_does_not_restore_revoked_key
// --------------------------------------------------------

/// 撤权后重装/重启（auth 域按实例重建）旧钥不复活：墓碑随实例账本持久化，
/// 重建后的同钥握手回双签名 revoked，不能再交付凭据；新钥走全新 TOFU 待批。
#[tokio::test]
async fn reinstall_does_not_restore_revoked_key() {
    if run_instance_credential_fixture_in_child("reinstall_does_not_restore_revoked_key") {
        return;
    }
    let b = dual_bench();
    let b_id = fixture_instance_b();
    let (svc, factory) = served_instance(&b.env, &b_id, HOST_PID_B).await;
    let epoch = instance_epoch(&b.env, &b_id);

    // key-1 绑定 + Cookie 交付。
    let (client1, binding1) = instance_bind_and_deliver(
        &svc,
        &factory,
        (HOST_PID_B, HOST_B_STARTED_AT, b_id.clone()),
        epoch,
        [51u8; 32],
        "owner-cookie-1",
    )
    .await;
    assert!(svc.has_delivered_peer(&binding1));

    // 撤权：墓碑落该实例账本（先断在途句柄）。
    svc.revoke(&binding1).await.expect("撤权受理");
    assert_eq!(svc.authorization_state(), AuthorizationState::Revoked);
    assert!(!client1.write_available(), "撤权关受信 Peer");

    // 重装/重启 = auth 域按实例重建（同一实例 ID、全新内存态、账本自盘载）。
    let (svc2, factory2) = served_instance(&b.env, &b_id, HOST_PID_B).await;
    assert_eq!(
        svc2.authorization_state(),
        AuthorizationState::Revoked,
        "墓碑跨重建持久（随实例账本）"
    );
    assert!(
        !svc2.has_delivered_peer(&binding1),
        "重建后凭据槽位为空（不复活）"
    );

    // 旧钥（同一把客户端钥）重连：永久拒绝。
    let old = InstanceClient::with_keys(
        &factory2,
        &client1.keys,
        HOST_PID_B,
        HOST_B_STARTED_AT,
        b_id.clone(),
    )
    .await;
    let challenge = BindingChallengeMessage {
        binding_protocol: BINDING_PROTOCOL,
        key_digests: [
            svc2.key_digest().to_string(),
            dsh_auth::key_digest(&old.keys.spki),
        ],
        nonce: dsh_auth::b64url_encode(&[52u8; 32]),
        host_id: b_id.clone(),
        epoch: epoch.to_string(),
        expiry: svc2.now_ms() + CHALLENGE_TTL_MS,
        home_binding: fixture_home_binding(WIN_HOME_B, HOST_PID_B, HOST_B_STARTED_AT),
    };
    let refused = old
        .hello_frame(&svc2, epoch, [52u8; 32])
        .await
        .expect("重装后旧钥须收到签名撤权应答");
    assert_signed_revoked_hello(&refused, &svc2, &old.keys, &challenge, &binding1);
    assert_eq!(
        svc2.authorization_state(),
        AuthorizationState::Revoked,
        "重装后旧钥不得恢复 Bound"
    );
    assert_eq!(svc2.active_binding_id(), None);
    assert!(svc2.pending_binding().is_none(), "重装后旧钥不得进入待批");
    let delivery = old
        .send_credential(&binding1, "revoked-cookie")
        .await
        .expect("撤权交付有拒绝应答");
    assert!(
        matches!(&delivery, ServerFrame::Error { error, .. } if error == codes::AUTH_NOT_APPROVED),
        "{delivery:?}"
    );
    assert!(
        !svc2.has_delivered_peer(&binding1),
        "重装后已撤旧钥不得交付凭据"
    );

    // 新钥（重装后插件重新生成身份）→ 全新 TOFU 待批，绝不自动恢复。
    let fresh = InstanceClient::new(&factory2, HOST_PID_B, HOST_B_STARTED_AT, b_id.clone()).await;
    let reply = fresh
        .connect(&svc2, epoch, [53u8; 32])
        .await
        .expect("有应答");
    let ServerFrame::ServerHello(hello) = reply else {
        panic!("应得 serverHello，得到 {reply:?}")
    };
    assert_eq!(hello.state, "pending_approval", "重装后新钥走全新待批");
    assert_eq!(svc2.authorization_state(), AuthorizationState::Revoked);
}

// --------------------------------------------------------
// H05 附带：A/B 不同钥和 Cookie；撤 A 后 B 可用
// --------------------------------------------------------

/// A/B 各自完成绑定（不同钥、不同 Cookie）；撤 A 之后 B 的绑定、凭据、gate、
/// 重连全部不受波及（实例域隔离的正面面）。
#[tokio::test]
async fn revoking_a_leaves_b_usable_across_instances() {
    if run_instance_credential_fixture_in_child("revoking_a_leaves_b_usable_across_instances") {
        return;
    }
    let b = dual_bench();
    let (a_id, b_id) = (fixture_instance_a(), fixture_instance_b());
    let (svc_a, factory_a) = served_instance(&b.env, &a_id, HOST_PID).await;
    let (svc_b, factory_b) = served_instance(&b.env, &b_id, HOST_PID_B).await;
    let epoch_a = instance_epoch(&b.env, &a_id);
    let epoch_b = instance_epoch(&b.env, &b_id);

    // A/B 各自绑定（不同钥 + 不同 Cookie）。
    let (client_a, binding_a) = instance_bind_and_deliver(
        &svc_a,
        &factory_a,
        (HOST_PID, HOST_STARTED_AT, a_id.clone()),
        epoch_a,
        [61u8; 32],
        "owner-cookie-A",
    )
    .await;
    let (client_b, binding_b) = instance_bind_and_deliver(
        &svc_b,
        &factory_b,
        (HOST_PID_B, HOST_B_STARTED_AT, b_id.clone()),
        epoch_b,
        [62u8; 32],
        "owner-cookie-B",
    )
    .await;
    assert_ne!(
        dsh_auth::key_digest(&client_a.keys.spki),
        dsh_auth::key_digest(&client_b.keys.spki),
        "A/B 用不同客户端钥"
    );
    assert!(svc_a.has_delivered_peer(&binding_a) && svc_b.has_delivered_peer(&binding_b));
    let peer_a = svc_a.peer_for_control().await.expect("A 的句柄");
    let peer_b = svc_b.peer_for_control().await.expect("B 的句柄");
    // 绑定号在各自域内独立计数（隔离账本可以合法同号）；归属才是断言点：
    // A 的句柄钉 A 域的绑定、B 的句柄钉 B 域的绑定。
    assert_eq!(peer_a.binding_id(), binding_a, "A 的句柄归属 A 域内的绑定");
    assert_eq!(peer_b.binding_id(), binding_b, "B 的句柄归属 B 域内的绑定");

    // 撤 A：A 域内墓碑 + 断线。
    svc_a.revoke(&binding_a).await.expect("撤 A");
    assert!(!client_a.write_available());
    assert_eq!(svc_a.authorization_state(), AuthorizationState::Revoked);

    // B 完全可用：绑定在、凭据在、gate 未翻转、重连 authorized。
    assert_eq!(svc_b.authorization_state(), AuthorizationState::Bound);
    assert!(
        svc_b.has_delivered_peer(&binding_b),
        "B 的凭据不受 A 撤权影响"
    );
    assert!(
        svc_b.peer_for_control().await.is_some(),
        "B 的 hub 句柄仍可用"
    );
    let re_b = InstanceClient::with_keys(
        &factory_b,
        &client_b.keys,
        HOST_PID_B,
        HOST_B_STARTED_AT,
        b_id.clone(),
    )
    .await;
    let reply = re_b
        .connect(&svc_b, epoch_b, [63u8; 32])
        .await
        .expect("有应答");
    let ServerFrame::ServerHello(hello) = reply else {
        panic!("应得 serverHello，得到 {reply:?}")
    };
    assert_eq!(hello.state, "authorized", "B 的钉公钥照常放行");
}

// --------------------------------------------------------
// H05：指纹探针生产接线（绑定账本 = 当代受信插件公钥指纹来源）
// --------------------------------------------------------

/// 真实文件源只读投影（登记面 canonicalize/真实目录语义需要真实 fs）。
struct RealFsFiles;

impl DshFileSource for RealFsFiles {
    fn exists(&self, path: &Path) -> bool {
        std::fs::symlink_metadata(path).is_ok()
    }

    fn is_dir(&self, path: &Path) -> bool {
        std::fs::symlink_metadata(path)
            .map(|m| m.is_dir())
            .unwrap_or(false)
    }

    fn list_dir(&self, path: &Path) -> Vec<PathBuf> {
        std::fs::read_dir(path)
            .map(|entries| entries.flatten().map(|e| e.path()).collect())
            .unwrap_or_default()
    }

    fn read_small(&self, path: &Path, max_bytes: u64) -> Result<Vec<u8>, DshError> {
        let meta = std::fs::metadata(path)
            .map_err(|e| DshError::new(codes::STATE_IO, format!("stat {}: {e}", path.display())))?;
        if meta.len() > max_bytes {
            return Err(DshError::new(
                codes::STATE_IO,
                format!("file over budget: {}", path.display()),
            ));
        }
        std::fs::read(path).map_err(|e| DshError::new(codes::STATE_IO, format!("read: {e}")))
    }
}

/// Mac 形态宿主进程行（真实临时目录夹具用；argv 携带 profile 令牌）。
fn real_desktop_process(pid: u32, profile: &Path, started_at_ms: i64) -> DshProcess {
    DshProcess {
        pid,
        exe: PathBuf::from("/Applications/DeepSeek Harness.app/Contents/MacOS/DeepSeek Harness"),
        cmdline: vec![
            "/Applications/DeepSeek Harness.app/Contents/MacOS/DeepSeek Harness".into(),
            "--expose-internals".into(),
            "/Applications/DeepSeek Harness.app/Contents/Resources/app.asar/dsh/node_modules/@deepseek-ai/dsh-desktop-host/lib/index.js".into(),
            profile.to_string_lossy().to_string(),
        ],
        started_at_ms,
    }
}

/// 指纹探针接线端到端：真实绑定 → 探针给出钉公钥 digest → 登记后 resolve 不再恒报
/// UNVERIFIED；撤权后来源消失 → 不可核验拒绝（fail-closed）；重新授权换新钥 →
/// 指纹不连续 → 显式 rebind 后恢复（H04 指纹门语义保持）。
#[tokio::test]
async fn pinned_binding_fingerprint_feeds_instance_registry() {
    if run_instance_credential_fixture_in_child(
        "pinned_binding_fingerprint_feeds_instance_registry",
    ) {
        return;
    }
    let root = tempfile::tempdir().expect("tempdir 可建");
    let home = root.path().join("home");
    let profile = home.join("profiles").join("desktop");
    std::fs::create_dir_all(&profile).expect("profile 目录");
    std::fs::write(profile.join("package.json"), b"{}").expect("官方标记");
    let pid = 47001u32;
    let started = 1_760_020_000_000i64;
    let procs = Arc::new(SharedProcs {
        processes: Mutex::new(vec![real_desktop_process(pid, &profile, started)]),
        listeners: Mutex::new(vec![DshListener {
            owner_pid: pid,
            authority: "127.0.0.1:53001".into(),
        }]),
    });
    let clock = Arc::new(VarClock(Mutex::new(started)));
    let env = DshEnvironment {
        home: root.path().join("env-home"),
        tuvis_home: root.path().join("tuvis-home"),
        dsh_state_dir: None,
        processes: procs.clone(),
        files: Arc::new(RealFsFiles),
        runner: Arc::new(NullRunner),
        clock: clock.clone(),
        // 测试默认「不可观测」：显式拒绝 Web home 归属（fail-closed），
        // 绝不让假进程源落到真实系统 PID 上。需要 Web 归属的用例自行注入假观测器。
        environments: std::sync::Arc::new(
            multi_agents_manager_lib::services::dsh_control::model::TestUnobservableEnvironments,
        ),
    };
    let instance_id = instance_id_for(HostKind::Desktop, &profile);
    let (svc, factory) = served_instance(&env, &instance_id, pid).await;
    let epoch = instance_epoch(&env, &instance_id);

    // 真实绑定（pending → 人工批准）；自报可执行文件 = 本夹具宿主（Mac 形态）。
    let mut client = InstanceClient::new(&factory, pid, started, instance_id.clone()).await;
    client.executable = "/Applications/DeepSeek Harness.app/Contents/MacOS/DeepSeek Harness".into();
    client.home_root = home.to_string_lossy().to_string();
    let reply = client
        .connect(&svc, epoch, [41u8; 32])
        .await
        .expect("握手有应答");
    let ServerFrame::ServerHello(hello) = reply else {
        panic!("应得 serverHello，得到 {reply:?}")
    };
    assert_eq!(hello.state, "pending_approval");
    let binding1 = hello.binding_id.clone().expect("binding_id");
    svc.approve(&binding1).await.expect("批准");

    // 生产形状探针：指纹 = 该实例 auth 域最新 Bound 行钉公钥的 digest。
    let probe = dsh_instance_production::BindingFingerprintProbe {
        env: Arc::new(clone_env_of(&env)),
        service_override: std::sync::Arc::new(std::sync::Mutex::new(Some(Arc::clone(&svc)))),
    };
    let selected = SelectedInstance {
        instance_id: instance_id.clone(),
        kind: HostKind::Desktop,
        profile_dir: profile.clone(),
        dsh_home: None,
        identity_fingerprint: String::new(),
        read_evidence_fingerprint: String::new(),
    };
    // R02 语义修正：批准 = TOFU 钉公钥 + 当代代际锚定（签名挑战 + peercred 在
    // 本宿主代际内完成）→ 当代指纹应为客户端钥摘要。凭据交付状态不影响身份
    // 连续性判定（交付只决定句柄就绪，不是身份证明的一部分）。
    let fp_after_approve = probe
        .current_fingerprint(&selected)
        .expect("探针不报错")
        .expect("批准即当代核验（R02）");
    assert_eq!(
        fp_after_approve,
        dsh_auth::key_digest(&client.keys.spki),
        "当代指纹 = 被批准客户端钥的摘要"
    );
    let _ = client.drain().await;
    assert!(matches!(
        client
            .send_credential(&binding1, "test-cookie")
            .await
            .unwrap(),
        ServerFrame::Granted { .. }
    ));
    let fp1 = probe
        .current_fingerprint(&selected)
        .expect("探针不报错")
        .expect("绑定后必有来源");
    assert_eq!(
        fp1,
        dsh_auth::key_digest(&client.keys.spki),
        "指纹 = 钉住公钥的 SHA-256 digest"
    );

    // 登记面接线：register → select → resolve 通过（不再恒报 UNVERIFIED）。
    let registry = InstanceRegistry::open_with_env(
        root.path().join("instances.json"),
        Some(Arc::new(clone_env_of(&env))),
    )
    .expect("登记可开")
    .with_identity_probe(Arc::new(dsh_instance_production::BindingFingerprintProbe {
        env: Arc::new(clone_env_of(&env)),
        service_override: std::sync::Arc::new(std::sync::Mutex::new(Some(Arc::clone(&svc)))),
    }));
    registry
        .register(HostKind::Desktop, &profile, &fp1)
        .expect("登记受理");
    registry.select(&instance_id).expect("选择");
    assert_eq!(
        registry.resolve_selected().expect("resolve").host_id,
        instance_id,
        "接线后已登记实例的 resolve 不再恒报 UNVERIFIED"
    );

    // 撤权 → 来源消失 → 不可核验拒绝（隔离环境语义保持：无来源不当作通过）。
    svc.revoke(&binding1).await.expect("撤权");
    assert!(probe.current_fingerprint(&selected).unwrap().is_none());
    let err = registry.resolve_selected().expect_err("无来源必须拒绝");
    assert_eq!(err.code, codes::INSTANCE_UNVERIFIED);

    // 重新授权（新钥）→ 指纹不连续：H04 门语义保持（AUTH_SIGNATURE）；显式 rebind 后恢复。
    let mut client2 = InstanceClient::new(&factory, pid, started, instance_id.clone()).await;
    client2.executable =
        "/Applications/DeepSeek Harness.app/Contents/MacOS/DeepSeek Harness".into();
    client2.home_root = home.to_string_lossy().to_string();
    let reply2 = client2
        .connect(&svc, epoch, [42u8; 32])
        .await
        .expect("握手有应答");
    let ServerFrame::ServerHello(hello2) = reply2 else {
        panic!("应得 serverHello，得到 {reply2:?}")
    };
    assert_eq!(hello2.state, "pending_approval");
    svc.approve(&hello2.binding_id.expect("binding_id"))
        .await
        .expect("批准新绑定");
    let fp2 = probe
        .current_fingerprint(&selected)
        .unwrap()
        .expect("新绑定有来源");
    assert_ne!(fp2, fp1, "换钥必换指纹");
    let err = registry.resolve_selected().expect_err("指纹不连续必须拒绝");
    assert_eq!(err.code, codes::AUTH_SIGNATURE);
    registry
        .rebind_fingerprint(&instance_id, &fp2)
        .expect("显式重绑受理");
    assert_eq!(
        registry.resolve_selected().expect("resolve").host_id,
        instance_id,
        "重绑后 resolve 恢复"
    );
}

// --------------------------------------------------------
// H05：协议迁移纪律（版本/签名字段扩展显式拒绝，不默默漏签）
// --------------------------------------------------------

/// 未来 bindingProtocol 的 hello 必须**显式**拒绝（DSH_AUTH_PROTOCOL）——
/// 旧服务端对新客户端「明确拒绝而非忽略」，签名挑战不漏签。
#[tokio::test]
async fn future_binding_protocol_is_rejected_explicitly() {
    let b = dual_bench();
    let b_id = fixture_instance_b();
    let (svc, factory) = served_instance(&b.env, &b_id, HOST_PID_B).await;
    let epoch = instance_epoch(&b.env, &b_id);
    let client = InstanceClient::new(&factory, HOST_PID_B, HOST_B_STARTED_AT, b_id.clone()).await;
    let reply = client
        .hello_frame_with_protocol(&svc, epoch, [71u8; 32], BINDING_PROTOCOL + 1)
        .await
        .expect("有应答");
    let ServerFrame::Error { error, .. } = reply else {
        panic!("未来协议必须显式拒绝，得到 {reply:?}")
    };
    assert_eq!(error, codes::AUTH_PROTOCOL);
    assert_eq!(
        svc.authorization_state(),
        AuthorizationState::Unbound,
        "拒绝路径不得产生任何绑定"
    );
}

// ============================================================
// H05 独立审查修复回归（C1 / I1 / I2 / I3）
// ============================================================

/// Web 形态宿主 argv（`dsh`+`web` 令牌；H02：Web argv 不含 home）。
fn web_host_process_for(pid: u32, started_at_ms: i64) -> DshProcess {
    DshProcess {
        pid,
        exe: PathBuf::from(r"C:\Node\node.exe"),
        cmdline: vec![
            r"C:\Node\node.exe".into(),
            r"C:\cli\dsh".into(),
            "web".into(),
        ],
        started_at_ms,
    }
}

/// env.home 的 web profile 令牌（Web 候选的锚点——C1 场景的关键：锚点 ≠ 实际 home）。
const WIN_PROFILE_WEB: &str = r"C:\Users\bunny\.dsh\profiles\web";
const WEB_TEST_PID: u32 = 42000;
const WEB_TEST_STARTED_AT: i64 = 1_760_030_000_000;

/// C1 审查修复：**授权完成前的归属证明门**——verified=false 的 Web 候选（profile
/// 锚在 Tuvis env.home 而非运行进程实际 home）不得完成绑定或凭据交付；hello 在
/// 任何绑定/待批产生之前被拒，fail-closed：未核验 Web 不进入握手。
#[tokio::test]
async fn unverified_web_instance_authorization_is_refused() {
    let mut files = FakeFiles::default();
    seed_profile_files(&mut files, WIN_PROFILE_WEB);
    let store = Arc::new(Mutex::new(files));
    let tmp = tempfile::tempdir().expect("tempdir 可建");
    let procs = Arc::new(SharedProcs {
        processes: Mutex::new(vec![web_host_process_for(
            WEB_TEST_PID,
            WEB_TEST_STARTED_AT,
        )]),
        listeners: Mutex::new(vec![DshListener {
            owner_pid: WEB_TEST_PID,
            authority: "127.0.0.1:5219".into(),
        }]),
    });
    let clock = Arc::new(VarClock(Mutex::new(WEB_TEST_STARTED_AT)));
    let env = DshEnvironment {
        home: PathBuf::from(WIN_HOME),
        tuvis_home: tmp.path().join("tuvis-home"),
        dsh_state_dir: None,
        processes: procs.clone(),
        files: Arc::new(StoreSource(store.clone())),
        runner: Arc::new(NullRunner),
        clock: clock.clone(),
        // 测试默认「不可观测」：显式拒绝 Web home 归属（fail-closed），
        // 绝不让假进程源落到真实系统 PID 上。需要 Web 归属的用例自行注入假观测器。
        environments: std::sync::Arc::new(
            multi_agents_manager_lib::services::dsh_control::model::TestUnobservableEnvironments,
        ),
    };
    let web_id = instance_id_for(HostKind::Web, Path::new(WIN_PROFILE_WEB));
    // R01：当前发现结果没有该 Web 实例的候选时，服务在 bind 监听端之后、发布
    // rendezvous 与启动握手处理之前以「不在场」失败，不会进入握手。这里不证明
    // 「存在 verified=false 候选」时的授权门（另有独立场景）。
    let peer_os = PeerOsIdentity {
        pid: WEB_TEST_PID,
        uid: Some(TEST_UID),
    };
    let (listener, _factory) = mem_listener(peer_os);
    let svc = Arc::new(
        dsh_instance_production::init_instance_auth(&env, &web_id)
            .expect("实例 auth 域可装配")
            .with_listener(Box::new(listener)),
    );
    let error = Arc::clone(&svc)
        .serve()
        .await
        .expect_err("未核验归属的 Web 实例不得启动授权服务");
    // P1-1b 之后：有真实 Web 进程但**观测不到启动环境** → `INSTANCE_UNVERIFIED`
    // （带可诊断原因），不再是笼统的「不在场」。两者都是 fail-closed 拒绝；
    // 断言两选一并同时要求「不得产出候选」，避免把错误码变化当成行为放宽。
    assert!(
        matches!(
            error.code,
            codes::INSTANCE_UNVERIFIED | codes::HOST_NOT_RUNNING
        ),
        "未核验归属必须拒绝，实际错误：{error}"
    );
    assert!(
        identity::discover_all(&env)
            .map(|c| c.is_empty())
            .unwrap_or(true),
        "观测不到启动环境时不得产出该 Web 实例的候选（不猜身份）"
    );
    assert_eq!(svc.authorization_state(), AuthorizationState::Unbound);
    assert!(svc.pending_binding().is_none(), "拒绝路径不产生待批记录");
    assert!(
        !instance_ledger_file(&env, &web_id).exists(),
        "拒绝路径不得创建实例账本"
    );
}

/// I1 审查修复：意图自动完成绑定**原子消费同钥旧 pending**（旧核对码不得在自动
/// 绑定后仍可被批准）并关闭旧连接；「pending → 自动绑定 → 撤权 → 批准旧 pending」
/// 必须失败，撤权不复活。
#[tokio::test]
async fn intent_auto_bind_consumes_stale_pending_and_revoke_wins() {
    let b = dual_bench();
    let b_id = fixture_instance_b();
    let (svc, factory) = served_instance(&b.env, &b_id, HOST_PID_B).await;
    let epoch = instance_epoch(&b.env, &b_id);

    // ① key-1 先走一轮 hello → pending（无意图时的正常待批）。
    let client1 = InstanceClient::new(&factory, HOST_PID_B, HOST_B_STARTED_AT, b_id.clone()).await;
    let reply1 = client1
        .connect(&svc, epoch, [81u8; 32])
        .await
        .expect("握手有应答");
    let ServerFrame::ServerHello(hello1) = reply1 else {
        panic!("应得 serverHello，得到 {reply1:?}")
    };
    assert_eq!(hello1.state, "pending_approval");
    let stale_binding = hello1.binding_id.clone().expect("pending 必带 binding_id");
    assert!(client1.write_available(), "待批连接存活（R04 语义）");

    // ② 布防意图 + 同钥重发 hello（新连接）→ 自动完成绑定。
    svc.begin_authorization_intent().expect("布防");
    let client2 = InstanceClient::with_keys(
        &factory,
        &client1.keys,
        HOST_PID_B,
        HOST_B_STARTED_AT,
        b_id.clone(),
    )
    .await;
    let reply2 = client2
        .connect(&svc, epoch, [82u8; 32])
        .await
        .expect("握手有应答");
    let ServerFrame::ServerHello(hello2) = reply2 else {
        panic!("应得 serverHello，得到 {reply2:?}")
    };
    assert_eq!(hello2.state, "authorized");
    let fresh_binding = hello2
        .binding_id
        .clone()
        .expect("authorized 必带 binding_id");

    // ③ 同钥旧 pending 已被原子消费：视图无待批、旧连接被关闭。
    assert!(
        svc.pending_binding().is_none(),
        "自动绑定必须消费同钥旧 pending（旧码不得仍可批准）"
    );
    assert!(!client1.write_available(), "旧 pending 的连接必须被关闭");
    assert_eq!(svc.authorization_state(), AuthorizationState::Bound);

    // ④ 撤权自动绑定 → 墓碑。
    svc.revoke(&fresh_binding).await.expect("撤权受理");
    assert_eq!(svc.authorization_state(), AuthorizationState::Revoked);

    // ⑤ 批准旧 pending 的码 → 必须失败（记录已被消费），旧钥不复活。
    let err = svc
        .approve(&stale_binding)
        .await
        .expect_err("旧码必须不可批");
    assert_eq!(err.code, codes::AUTH_UNKNOWN_BINDING);
    assert_eq!(svc.authorization_state(), AuthorizationState::Revoked);

    // ⑥ 同钥重连 → 双签名 revoked（与 ⑤ 共同封死「经旧码复活」路径）。
    let re = InstanceClient::with_keys(
        &factory,
        &client1.keys,
        HOST_PID_B,
        HOST_B_STARTED_AT,
        b_id.clone(),
    )
    .await;
    let challenge = BindingChallengeMessage {
        binding_protocol: BINDING_PROTOCOL,
        key_digests: [
            svc.key_digest().to_string(),
            dsh_auth::key_digest(&re.keys.spki),
        ],
        nonce: dsh_auth::b64url_encode(&[83u8; 32]),
        host_id: b_id.clone(),
        epoch: epoch.to_string(),
        expiry: svc.now_ms() + CHALLENGE_TTL_MS,
        home_binding: fixture_home_binding(WIN_HOME_B, HOST_PID_B, HOST_B_STARTED_AT),
    };
    let refused = re
        .hello_frame(&svc, epoch, [83u8; 32])
        .await
        .expect("已撤旧钥须收到双签名撤权应答");
    assert_signed_revoked_hello(&refused, &svc, &re.keys, &challenge, &fresh_binding);
    assert_eq!(
        svc.authorization_state(),
        AuthorizationState::Revoked,
        "旧 pending 不得复活已撤绑定"
    );
    assert_eq!(svc.active_binding_id(), None);
    assert!(svc.pending_binding().is_none(), "已撤旧钥不得重建待批");
    let delivery = re
        .send_credential(&fresh_binding, "revoked-cookie")
        .await
        .expect("撤权交付有拒绝应答");
    assert!(
        matches!(&delivery, ServerFrame::Error { error, .. } if error == codes::AUTH_NOT_APPROVED),
        "{delivery:?}"
    );
    assert!(
        !svc.has_delivered_peer(&fresh_binding),
        "旧 pending 复用不得交付凭据"
    );
}

/// I2 审查修复：空状态 → 候选配置登记（无指纹）→ 实例 auth 域可建 → 首次绑定 →
/// 受信身份落账（register 锚定）→ resolve 通过。生产入口不再循环依赖；空指纹
/// 记录在落账前一律不可核验拒绝（不靠占位指纹过登记）。
#[tokio::test]
async fn first_authorization_uses_candidate_registration_then_trust_anchor() {
    let root = tempfile::tempdir().expect("tempdir 可建");
    let home = root.path().join("home");
    let profile = home.join("profiles").join("desktop");
    std::fs::create_dir_all(&profile).expect("profile 目录");
    std::fs::write(profile.join("package.json"), b"{}").expect("官方标记");
    let pid = 48001u32;
    let started = 1_760_025_000_000i64;
    let procs = Arc::new(SharedProcs {
        processes: Mutex::new(vec![real_desktop_process(pid, &profile, started)]),
        listeners: Mutex::new(vec![DshListener {
            owner_pid: pid,
            authority: "127.0.0.1:53002".into(),
        }]),
    });
    let clock = Arc::new(VarClock(Mutex::new(started)));
    let env = DshEnvironment {
        home: root.path().join("env-home"),
        tuvis_home: root.path().join("tuvis-home"),
        dsh_state_dir: None,
        processes: procs.clone(),
        files: Arc::new(RealFsFiles),
        runner: Arc::new(NullRunner),
        clock: clock.clone(),
        // 测试默认「不可观测」：显式拒绝 Web home 归属（fail-closed），
        // 绝不让假进程源落到真实系统 PID 上。需要 Web 归属的用例自行注入假观测器。
        environments: std::sync::Arc::new(
            multi_agents_manager_lib::services::dsh_control::model::TestUnobservableEnvironments,
        ),
    };
    let probe_env = Arc::new(clone_env_of(&env));
    // R02：当代观测槽——registry 探针与测试断言共用同一服务源；svc 建好后回填。
    let probe_slot = std::sync::Arc::new(std::sync::Mutex::new(None));
    let registry = InstanceRegistry::open_with_env(
        root.path().join("instances.json"),
        Some(Arc::new(clone_env_of(&env))),
    )
    .expect("登记可开")
    .with_identity_probe(Arc::new(dsh_instance_production::BindingFingerprintProbe {
        env: Arc::clone(&probe_env),
        service_override: std::sync::Arc::clone(&probe_slot),
    }));

    // ① 候选配置登记（阶段边界：无指纹、不依赖授权）。
    let instance = registry
        .register_candidate(HostKind::Desktop, &profile)
        .expect("候选登记受理");
    assert_eq!(
        instance.identity_fingerprint, "",
        "候选登记不带受信指纹（占位指纹不得过登记）"
    );
    assert!(
        !instance.instance_id.is_empty() && instance.instance_id != "local",
        "实例 ID 仍是登记派生"
    );
    registry.select(&instance.instance_id).expect("选择");

    // ② 实例 auth 域可建（I2：登记 → 授权不再被「必须已有指纹」挡住）。
    let (svc, factory) = served_instance(&env, &instance.instance_id, pid).await;
    // R02：探针槽回填被测服务（当代观测直连它，不经生产缓存）。
    *probe_slot.lock().unwrap() = Some(Arc::clone(&svc));
    let epoch = instance_epoch(&env, &instance.instance_id);

    // ③ 授权完成（真实 TOFU 批准）。
    let mut client =
        InstanceClient::new(&factory, pid, started, instance.instance_id.clone()).await;
    client.executable = "/Applications/DeepSeek Harness.app/Contents/MacOS/DeepSeek Harness".into();
    client.home_root = home.to_string_lossy().to_string();
    let reply = client
        .connect(&svc, epoch, [84u8; 32])
        .await
        .expect("握手有应答");
    let ServerFrame::ServerHello(hello) = reply else {
        panic!("应得 serverHello，得到 {reply:?}")
    };
    assert_eq!(hello.state, "pending_approval");
    svc.approve(&hello.binding_id.expect("binding_id"))
        .await
        .expect("批准");

    // ④ 授权后、落账前：指纹门如实不连续（阶段边界可见，不冒充已通过）。
    let probe = dsh_instance_production::BindingFingerprintProbe {
        env: Arc::clone(&probe_env),
        service_override: std::sync::Arc::clone(&probe_slot),
    };
    let selected = SelectedInstance {
        instance_id: instance.instance_id.clone(),
        kind: HostKind::Desktop,
        profile_dir: profile.clone(),
        dsh_home: None,
        identity_fingerprint: String::new(),
        read_evidence_fingerprint: String::new(),
    };
    let fp = probe
        .current_fingerprint(&selected)
        .unwrap()
        .expect("绑定后有来源");
    let err = registry
        .resolve_selected()
        .expect_err("未落账必须不可核验通过");
    assert_eq!(
        err.code,
        codes::AUTH_SIGNATURE,
        "空指纹 vs 当代指纹 → 不连续"
    );

    // ⑤ 受信身份落账（register 锚定既有候选记录）→ resolve 通过。
    let anchored = registry
        .register(HostKind::Desktop, &profile, &fp)
        .expect("落账受理");
    assert_eq!(anchored.instance_id, instance.instance_id, "同一实例记录");
    assert_eq!(anchored.identity_fingerprint, fp);
    assert_eq!(
        registry.resolve_selected().expect("resolve").host_id,
        instance.instance_id
    );
}

/// I3 审查修复：实例域上线时的 legacy rendezvous 交接——仍在场的 legacy 记录被
/// 改写为 `retired = true`（插件据此忽略 → 同 PID 单匹配），实例域自身记录非停用。
#[tokio::test]
async fn instance_publish_retires_legacy_rendezvous() {
    let b = dual_bench();
    let b_id = fixture_instance_b();
    let base_state = b.env.tuvis_home.join("dsh");
    std::fs::create_dir_all(&base_state).expect("基础状态目录");
    // R04（复审 P2）：交接语义 = 只退休**同宿主代际**的 legacy 记录——B 自己的
    // legacy 残留（同 host_id/pid/启动时刻/epoch）才允许置 retired。种子用 B 的
    // 当代代际字段（与 serve 发布的实例记录同源）。
    let legacy = serde_json::json!({
        "bindingProtocol": BINDING_PROTOCOL,
        "address": "legacy-endpoint",
        "hostId": b_id,
        "epoch": instance_epoch(&b.env, &b_id).to_string(),
        "processIdentity": {
            "platform": "windows",
            "pid": HOST_PID_B,
            "startedAtMs": HOST_B_STARTED_AT,
            "executable": "host",
        },
        "publicKeySpki": "legacy-key",
        "retired": false,
    });
    std::fs::write(
        base_state.join("rendezvous.json"),
        serde_json::to_vec_pretty(&legacy).expect("legacy 记录可序列化"),
    )
    .expect("预置 legacy rendezvous");

    // 实例域上线（serve 发布 + 交接标记）。
    let (svc, _factory) = served_instance(&b.env, &b_id, HOST_PID_B).await;
    let _ = svc.key_digest();

    let after: serde_json::Value = serde_json::from_slice(
        &std::fs::read(base_state.join("rendezvous.json")).expect("legacy 记录在场"),
    )
    .expect("legacy 记录可解析");
    assert_eq!(
        after["retired"], true,
        "同宿主代际的 legacy 记录必须被标记停用（同 PID 不再双匹配）"
    );
    assert_eq!(after["hostId"], b_id, "交接不改写记录其余字段");

    let inst: serde_json::Value = serde_json::from_slice(
        &std::fs::read(instance_state_dir(&b.env, &b_id).join("rendezvous.json"))
            .expect("实例 rendezvous 在场"),
    )
    .expect("实例记录可解析");
    assert_eq!(inst["retired"], false, "实例域记录非停用");
    assert_eq!(inst["hostId"], b_id, "实例记录指向自身");
}

/// R04（复审 P2）反例：异宿主代际的 legacy 记录（A 仍在 legacy 域）——B 的实例
/// 域上线**不得**退休 A 的入口；A 重连后照常可达。
#[tokio::test]
async fn foreign_generation_legacy_rendezvous_is_not_retired() {
    let b = dual_bench();
    let b_id = fixture_instance_b();
    let base_state = b.env.tuvis_home.join("dsh");
    std::fs::create_dir_all(&base_state).expect("基础状态目录");
    // A 的 legacy 记录：host_id/epoch 与 B 不同（异主），pid 也不同。
    let legacy = serde_json::json!({
        "bindingProtocol": BINDING_PROTOCOL,
        "address": "a-legacy-endpoint",
        "hostId": "legacy-desktop",
        "epoch": "1",
        "processIdentity": {
            "platform": "windows",
            "pid": 41999,
            "startedAtMs": 1_760_000_000_000_i64,
            "executable": "host",
        },
        "publicKeySpki": "legacy-key",
        "retired": false,
    });
    std::fs::write(
        base_state.join("rendezvous.json"),
        serde_json::to_vec_pretty(&legacy).expect("legacy 记录可序列化"),
    )
    .expect("预置 legacy rendezvous");

    // B 实例域上线（serve 发布 + retire 尝试）。
    let (svc, _factory) = served_instance(&b.env, &b_id, HOST_PID_B).await;
    let _ = svc.key_digest();

    let after: serde_json::Value = serde_json::from_slice(
        &std::fs::read(base_state.join("rendezvous.json")).expect("legacy 记录在场"),
    )
    .expect("legacy 记录可解析");
    assert_eq!(after["retired"], false, "异主记录不得被 B 的交接退休");
    assert_eq!(after["address"], "a-legacy-endpoint", "记录保持原样");
}

// ============================================================
// H05 修复轮 2 回归（I3-r2 / N1）
// ============================================================

/// I3-r2：交接对旧 publisher **单调生效**——「启动 legacy publisher（真实 serve
/// 任务）→ 实例域交接（根 retired=true）→ 旧 publisher 再 accept（真实 accept
/// 循环刷新 publish）」不得把根 rendezvous 的 retired 刷回 false。
#[tokio::test]
async fn legacy_publisher_reaccept_cannot_undo_handover() {
    let b = bench(); // 单桌面：unscoped discover 可用
    let instance_id = fixture_instance_a();
    let root_path = b.env.tuvis_home.join("dsh").join("rendezvous.json");

    // ① legacy publisher（真实 serve 任务）：根 rendezvous retired=false。
    let peer_os = PeerOsIdentity {
        pid: HOST_PID,
        uid: Some(TEST_UID),
    };
    let (legacy_listener, legacy_factory) = mem_listener(peer_os);
    let legacy = Arc::new(
        dsh_auth::DshAuthService::new(clone_env_of(&b.env))
            .expect("legacy 域可装配")
            .with_listener(Box::new(legacy_listener)),
    );
    Arc::clone(&legacy).serve().await.expect("legacy serve");
    let before: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&root_path).expect("根记录在场")).expect("可解析");
    assert_eq!(before["retired"], false, "legacy 发布非停用");

    // ② 实例域上线 → 发布实例记录 + 交接标记（根 retired=true）。
    let (inst_svc, _inst_factory) = served_instance(&b.env, &instance_id, HOST_PID).await;
    let _ = inst_svc.key_digest();
    let mid: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&root_path).expect("根记录在场")).expect("可解析");
    assert_eq!(mid["retired"], true, "交接标记生效");

    // ③ 旧 publisher 再 accept（真实 accept 循环 → publish 刷新尝试）：
    // 同宿主代际（pid 相同）→ retired 必须保持 true（单调，不得撤销交接）。
    let _client = legacy_factory.connect().await;
    for _ in 0..10 {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let now: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&root_path).expect("根记录在场"))
                .expect("可解析");
        assert_eq!(
            now["retired"], true,
            "旧 publisher 的重发布不得撤销交接（观察到复活即失败）"
        );
    }
}

/// N1：同连接 pending → 布防意图 → **同一连接**重发 hello → authorized 应答必须
/// 送达（连接不被清理路径关掉）且凭据交付在同一条线上完成。
#[tokio::test]
async fn intent_auto_bind_on_the_same_connection_keeps_the_reply_line() {
    if run_instance_credential_fixture_in_child(
        "intent_auto_bind_on_the_same_connection_keeps_the_reply_line",
    ) {
        return;
    }
    let b = dual_bench();
    let b_id = fixture_instance_b();
    let (svc, factory) = served_instance(&b.env, &b_id, HOST_PID_B).await;
    let epoch = instance_epoch(&b.env, &b_id);

    // ① key-1 hello → pending（连接 C）。
    let client = InstanceClient::new(&factory, HOST_PID_B, HOST_B_STARTED_AT, b_id.clone()).await;
    let reply1 = client
        .connect(&svc, epoch, [86u8; 32])
        .await
        .expect("握手有应答");
    let ServerFrame::ServerHello(hello1) = reply1 else {
        panic!("应得 serverHello，得到 {reply1:?}")
    };
    assert_eq!(hello1.state, "pending_approval");

    // ② 布防意图 + **同一连接**（同 client/conn）重发 hello → 自动绑定。
    svc.begin_authorization_intent().expect("布防");
    let reply2 = client
        .connect(&svc, epoch, [87u8; 32])
        .await
        .expect("同连接重发 hello 必须有应答（N1：连接不得被清理路径关掉）");
    let ServerFrame::ServerHello(hello2) = reply2 else {
        panic!("应得 serverHello，得到 {reply2:?}")
    };
    assert_eq!(hello2.state, "authorized", "同连接自动绑定照常完成");
    let binding = hello2.binding_id.expect("authorized 必带 binding_id");

    // ③ 同一条连接上凭据交付照常完成（应答线活着）。
    assert!(client.write_available(), "authorized 应答线必须保持打开");
    let ack = client
        .send_credential(&binding, "owner-cookie-same-conn")
        .await
        .expect("交付有应答");
    assert!(matches!(ack, ServerFrame::Granted { .. }), "{ack:?}");
    assert!(svc.has_delivered_peer(&binding));

    // R02 返工回归：自动授权后**不重连**即可取得当代指纹（内核单点登记代际）。
    let probe = dsh_instance_production::BindingFingerprintProbe {
        env: Arc::new(clone_env_of(&b.env)),
        service_override: std::sync::Arc::new(std::sync::Mutex::new(Some(Arc::clone(&svc)))),
    };
    let selected = SelectedInstance {
        instance_id: b_id.clone(),
        kind: HostKind::Desktop,
        profile_dir: std::path::PathBuf::from(win_profile_b()),
        dsh_home: None,
        identity_fingerprint: String::new(),
        read_evidence_fingerprint: String::new(),
    };
    let fp = probe
        .current_fingerprint(&selected)
        .expect("探针不报错")
        .expect("自动授权即当代核验（内核统一登记）");
    assert_eq!(
        fp,
        dsh_auth::key_digest(&client.keys.spki),
        "当代指纹 = 自动绑定客户端钥的摘要"
    );

    // 断线清理由独立的真实 EOF 用例验证；MemClient 的共享发送端不会因 drop 关闭。
}

// ============================================================
// R02 基座回归：已验签的握手身份不得在登记/交付时重盖新代际。
// ============================================================

/// 只在授权意图检查时换代：客户端取时、TTL 检查仍是 gen1，验签后的意图检查
/// 返回前切到 gen2。没有 sleep，也不预置 trusted_epochs；服务端走真实验签/登记。
struct GenerationSwapClock {
    procs: Arc<SharedProcs>,
    reads_until_swap: std::sync::atomic::AtomicUsize,
}

impl DshClock for GenerationSwapClock {
    fn now_ms(&self) -> i64 {
        use std::sync::atomic::Ordering;
        let mut remaining = self.reads_until_swap.load(Ordering::SeqCst);
        while let Some(next) = remaining.checked_sub(1) {
            match self.reads_until_swap.compare_exchange_weak(
                remaining,
                next,
                Ordering::SeqCst,
                Ordering::SeqCst,
            ) {
                Ok(1) => {
                    self.procs.set_host_started_at(HOST_STARTED_AT + 1_000);
                    break;
                }
                Ok(_) => break,
                Err(actual) => remaining = actual,
            }
        }
        HOST_STARTED_AT
    }
}

fn selected_fixture_a() -> SelectedInstance {
    SelectedInstance {
        instance_id: fixture_instance_a(),
        kind: HostKind::Desktop,
        profile_dir: PathBuf::from(WIN_PROFILE_TOKEN),
        dsh_home: None,
        identity_fingerprint: String::new(),
        read_evidence_fingerprint: String::new(),
    }
}

fn live_fingerprint(env: &DshEnvironment, svc: &Arc<dsh_auth::DshAuthService>) -> Option<String> {
    dsh_instance_production::BindingFingerprintProbe {
        env: Arc::new(clone_env_of(env)),
        service_override: Arc::new(Mutex::new(Some(Arc::clone(svc)))),
    }
    .current_fingerprint(&selected_fixture_a())
    .expect("实际指纹探针可查询")
}

#[tokio::test]
async fn generation_change_after_signature_does_not_register_old_hello_as_current() {
    let b = bench();
    let clock = Arc::new(GenerationSwapClock {
        procs: b.procs.clone(),
        reads_until_swap: std::sync::atomic::AtomicUsize::new(0),
    });
    let mut env = clone_env_of(&b.env);
    env.clock = clock.clone();
    let instance_id = fixture_instance_a();
    let (svc, factory) = served_instance(&env, &instance_id, HOST_PID).await;
    let epoch1 = instance_epoch(&env, &instance_id);
    let client =
        InstanceClient::new(&factory, HOST_PID, HOST_STARTED_AT, instance_id.clone()).await;
    svc.begin_authorization_intent().expect("布防自动授权");

    // 接下来三次取时：客户端签挑战、服务端 TTL 核对、验签后的 intent 检查。
    clock
        .reads_until_swap
        .store(3, std::sync::atomic::Ordering::SeqCst);
    let reply = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        client.hello_frame(&svc, epoch1, [171u8; 32]),
    )
    .await
    .expect("换代握手必须有界返回")
    .expect("服务端须返回协议帧");
    assert_eq!(
        clock
            .reads_until_swap
            .load(std::sync::atomic::Ordering::SeqCst),
        0
    );
    assert_ne!(instance_epoch(&env, &instance_id), epoch1, "夹具确实已换代");
    assert_eq!(
        live_fingerprint(&env, &svc),
        None,
        "gen1 验签后换代，旧握手不得贡献 gen2 受信指纹；应答为 {reply:?}"
    );
    assert!(
        matches!(reply, ServerFrame::Error { ref error, .. } if error == codes::AUTH_EPOCH_STALE),
        "登记前换代必须明确拒绝，实际 {reply:?}"
    );
}

#[tokio::test]
async fn generation_change_before_delivery_does_not_create_current_peer() {
    let b = bench();
    let instance_id = fixture_instance_a();
    let (svc, factory) = served_instance(&b.env, &instance_id, HOST_PID).await;
    let epoch1 = instance_epoch(&b.env, &instance_id);
    let client =
        InstanceClient::new(&factory, HOST_PID, HOST_STARTED_AT, instance_id.clone()).await;
    svc.begin_authorization_intent().expect("布防自动授权");
    let reply = client
        .connect(&svc, epoch1, [172u8; 32])
        .await
        .expect("同代握手成功");
    let ServerFrame::ServerHello(hello) = reply else {
        panic!("同代自动授权应返回 hello，实际 {reply:?}");
    };
    assert_eq!(hello.state, "authorized");
    let binding = hello.binding_id.expect("绑定 ID");
    assert_eq!(
        live_fingerprint(&b.env, &svc),
        Some(dsh_auth::key_digest(&client.keys.spki))
    );

    b.procs.set_host_started_at(HOST_STARTED_AT + 1_000);
    let epoch2 = instance_epoch(&b.env, &instance_id);
    assert_ne!(epoch1, epoch2);
    assert_eq!(live_fingerprint(&b.env, &svc), None, "换代后旧握手不再当代");
    let reply = client
        .send_credential(&binding, "test-gen1-cookie")
        .await
        .expect("交付有协议应答");
    let peer = svc.peer_for_control().await;
    assert!(
        peer.is_none(),
        "gen1 连接交付不得生成 gen2 控制句柄；实际句柄代际 {:?}，新代际 {epoch2}，应答 {reply:?}",
        peer.as_ref().map(|p| p.epoch())
    );
    assert!(!svc.has_delivered_peer(&binding), "拒绝交付不得留下凭据");
    assert!(
        matches!(reply, ServerFrame::Error { ref error, .. } if error == codes::AUTH_EPOCH_STALE),
        "旧握手交付必须明确拒绝，实际 {reply:?}"
    );
}

// ============================================================
// R02 连接退出：发送端由客户端独占，drop 必然使服务端 recv 得到真实通道 EOF。
// ============================================================

mod connection_exit_regressions {
    use super::*;
    use multi_agents_manager_lib::services::dsh_control::local_ipc::{
        IpcClientConnection, IpcFut, IpcListener,
    };
    use tokio::sync::{mpsc, Notify};

    #[derive(Default)]
    struct ExitControl {
        eof_seen: Notify,
        release_exit: Notify,
        dropped: Notify,
    }

    struct EofConnection {
        incoming: tokio::sync::Mutex<mpsc::Receiver<String>>,
        outgoing: mpsc::Sender<String>,
        exit: Arc<ExitControl>,
    }

    impl Drop for EofConnection {
        fn drop(&mut self) {
            self.exit.dropped.notify_one();
        }
    }

    impl IpcClientConnection for EofConnection {
        fn peer_os(&self) -> PeerOsIdentity {
            PeerOsIdentity {
                pid: HOST_PID,
                uid: Some(TEST_UID),
            }
        }

        fn recv(&self) -> IpcFut<'_, Result<String, DshError>> {
            Box::pin(async move {
                if let Some(line) = self.incoming.lock().await.recv().await {
                    return Ok(line);
                }
                // 唯一客户端发送端已释放；先证明 EOF 到达，再由测试精确安排退出。
                self.exit.eof_seen.notify_one();
                self.exit.release_exit.notified().await;
                Err(DshError::new(
                    codes::IPC_STATE,
                    "test transport: client EOF",
                ))
            })
        }

        fn send(&self, frame: &str) -> IpcFut<'_, Result<(), DshError>> {
            let frame = frame.to_string();
            Box::pin(async move {
                self.outgoing
                    .send(frame)
                    .await
                    .map_err(|_| DshError::new(codes::IPC_STATE, "client closed"))
            })
        }

        fn close(self: Arc<Self>) {
            self.exit.release_exit.notify_one();
        }
        // 有意不提前报告 closed：None 必须由真实处理循环退出清理产生。
        fn is_closed(&self) -> bool {
            false
        }
    }

    struct EofListener {
        incoming: tokio::sync::Mutex<mpsc::Receiver<Box<dyn IpcClientConnection>>>,
        endpoint: Arc<Mutex<Option<String>>>,
    }

    impl IpcListener for EofListener {
        fn bind(&self) -> IpcFut<'_, Result<String, DshError>> {
            Box::pin(async {
                let address = "mem://auth-real-eof".to_string();
                *self.endpoint.lock().unwrap() = Some(address.clone());
                Ok(address)
            })
        }
        fn accept(&self) -> IpcFut<'_, Result<Box<dyn IpcClientConnection>, DshError>> {
            Box::pin(async {
                self.incoming
                    .lock()
                    .await
                    .recv()
                    .await
                    .ok_or_else(|| DshError::new(codes::IPC_STATE, "listener stopped"))
            })
        }
        fn stop(&self) {}
        fn endpoint(&self) -> Option<String> {
            self.endpoint.lock().unwrap().clone()
        }
    }

    struct EofClient {
        incoming: tokio::sync::Mutex<mpsc::Receiver<String>>,
        outgoing: mpsc::Sender<String>,
    }

    impl EofClient {
        async fn hello(
            &self,
            svc: &dsh_auth::DshAuthService,
            keys: &RustSigningKey,
            epoch: u64,
            nonce: [u8; 32],
        ) -> ServerFrame {
            let challenge = BindingChallengeMessage {
                binding_protocol: BINDING_PROTOCOL,
                key_digests: [
                    svc.key_digest().to_string(),
                    dsh_auth::key_digest(&keys.spki),
                ],
                nonce: dsh_auth::b64url_encode(&nonce),
                host_id: fixture_instance_a(),
                epoch: epoch.to_string(),
                expiry: svc.now_ms() + CHALLENGE_TTL_MS,
                home_binding: fixture_home_binding(WIN_HOME, HOST_PID, HOST_STARTED_AT),
            };
            let hello = ClientFrame::Hello(HelloFrame {
                binding_protocol: BINDING_PROTOCOL,
                public_key_spki: dsh_auth::b64url_encode(&keys.spki),
                executable: format!(r"{WIN_INSTALL_DIR}\DeepSeek Harness.exe"),
                pid: HOST_PID,
                started_at_ms: HOST_STARTED_AT,
                host_id: challenge.host_id.clone(),
                epoch: challenge.epoch.clone(),
                nonce: challenge.nonce.clone(),
                expiry: challenge.expiry,
                home_binding: challenge.home_binding.clone(),
                signature: dsh_auth::b64url_encode(&keys.sign(&challenge.to_signing_bytes())),
            });
            self.outgoing
                .send(dsh_auth::client_frame_line(&hello))
                .await
                .expect("hello 已发送");
            let line = tokio::time::timeout(std::time::Duration::from_secs(2), async {
                self.incoming.lock().await.recv().await.expect("服务端应答")
            })
            .await
            .expect("hello 应答不悬挂");
            dsh_auth::parse_server_frame(&line).expect("应答为协议帧")
        }
    }

    fn connection() -> (EofClient, Box<dyn IpcClientConnection>, Arc<ExitControl>) {
        let (client_tx, server_rx) = mpsc::channel(4);
        let (server_tx, client_rx) = mpsc::channel(4);
        let exit = Arc::new(ExitControl::default());
        (
            EofClient {
                incoming: tokio::sync::Mutex::new(client_rx),
                outgoing: client_tx,
            },
            Box::new(EofConnection {
                incoming: tokio::sync::Mutex::new(server_rx),
                outgoing: server_tx,
                exit: exit.clone(),
            }),
            exit,
        )
    }

    async fn observed(notify: &Notify, message: &str) {
        tokio::time::timeout(std::time::Duration::from_secs(2), notify.notified())
            .await
            .expect(message);
    }

    async fn service(
        env: &DshEnvironment,
    ) -> (
        Arc<dsh_auth::DshAuthService>,
        mpsc::Sender<Box<dyn IpcClientConnection>>,
    ) {
        let (tx, rx) = mpsc::channel(4);
        let endpoint = Arc::new(Mutex::new(None));
        let listener = EofListener {
            incoming: tokio::sync::Mutex::new(rx),
            endpoint: endpoint.clone(),
        };
        assert_eq!(listener.endpoint(), None, "bind 前不得冒充已启动监听");
        let svc = Arc::new(
            dsh_instance_production::init_instance_auth(env, &fixture_instance_a())
                .expect("实例服务装配")
                .with_listener(Box::new(listener)),
        );
        svc.clone().serve().await.expect("监听启动");
        assert_eq!(
            endpoint.lock().unwrap().as_deref(),
            Some("mem://auth-real-eof"),
            "serve 必须真正调用 bind，不能提前返回"
        );
        (svc, tx)
    }

    #[tokio::test]
    async fn real_client_eof_removes_current_fingerprint() {
        let b = bench();
        let (svc, accept) = service(&b.env).await;
        let (client, conn, exit) = connection();
        accept.send(conn).await.expect("连接已入队");
        let keys = RustSigningKey::generate();
        svc.begin_authorization_intent().unwrap();
        let reply = client
            .hello(&svc, &keys, current_epoch(&b.env), [173u8; 32])
            .await;
        assert!(
            matches!(reply, ServerFrame::ServerHello(ref h) if h.state == "authorized"),
            "{reply:?}"
        );
        assert_eq!(
            live_fingerprint(&b.env, &svc),
            Some(dsh_auth::key_digest(&keys.spki))
        );
        drop(client);
        observed(&exit.eof_seen, "服务端必须真正收到 EOF").await;
        exit.release_exit.notify_one();
        // Drop 只会在 handle_connection 退出清理完成且不再持有该连接后触发。
        observed(&exit.dropped, "EOF 后必须完成退出清理并释放连接").await;
        assert_eq!(
            live_fingerprint(&b.env, &svc),
            None,
            "EOF 后最终指纹必须为 None"
        );
    }

    #[tokio::test]
    async fn old_connection_delayed_exit_preserves_replacement_fingerprint() {
        let b = bench();
        let (svc, accept) = service(&b.env).await;
        let (old, conn, old_exit) = connection();
        accept.send(conn).await.expect("旧连接已入队");
        let keys = RustSigningKey::generate();
        let epoch = current_epoch(&b.env);
        svc.begin_authorization_intent().unwrap();
        let first = old.hello(&svc, &keys, epoch, [174u8; 32]).await;
        let ServerFrame::ServerHello(first) = first else {
            panic!("应得首绑应答");
        };
        assert_eq!(first.state, "authorized");
        drop(old);
        observed(&old_exit.eof_seen, "旧连接到达 EOF，退出暂被阻塞").await;

        let (new, conn, new_exit) = connection();
        accept.send(conn).await.expect("替代连接已入队");
        let second = new.hello(&svc, &keys, epoch, [175u8; 32]).await;
        let ServerFrame::ServerHello(second) = second else {
            panic!("应得重连应答");
        };
        assert_eq!(second.state, "authorized");
        assert_eq!(
            second.binding_id, first.binding_id,
            "同钥重连必须替代同一 binding"
        );
        assert_eq!(
            live_fingerprint(&b.env, &svc),
            Some(dsh_auth::key_digest(&keys.spki))
        );

        old_exit.release_exit.notify_one();
        observed(&old_exit.dropped, "旧连接必须完成迟到退出清理").await;
        assert_eq!(
            live_fingerprint(&b.env, &svc),
            Some(dsh_auth::key_digest(&keys.spki)),
            "旧连接迟退不得删除同 binding 新连接的当代指纹"
        );
        drop(new);
        observed(&new_exit.eof_seen, "新连接清理前须真正收到 EOF").await;
        new_exit.release_exit.notify_one();
        observed(&new_exit.dropped, "新连接也被回收").await;
        assert_eq!(live_fingerprint(&b.env, &svc), None);
    }
}

// ============================================================
// H05 修复轮 3 回归（I3-r3：交接单调性两窗口）
// ============================================================

/// I3-r3①：PID 复用不锁死新发布——根记录 retired 且 PID 相同，但**启动时刻/epoch
/// 不同**（新进程复用同一 PID）→ 新代际 legacy 发布必须正常落盘（retired=false），
/// 绝不因 PID 单独命中旧交接标记而被跳过。
#[tokio::test]
async fn pid_reuse_does_not_inherit_handover_retirement() {
    let b = bench(); // 单桌面
    let instance_id = fixture_instance_a();
    let root_path = b.env.tuvis_home.join("dsh").join("rendezvous.json");
    std::fs::create_dir_all(root_path.parent().unwrap()).expect("基础状态目录");
    // 上一代交接标记：同 PID、不同启动时刻/epoch（PID 复用形态）。
    let stale = serde_json::json!({
        "bindingProtocol": BINDING_PROTOCOL,
        "address": "old-endpoint",
        "hostId": instance_id,
        "epoch": "stale-epoch",
        "processIdentity": {
            "platform": "windows",
            "pid": HOST_PID,
            "startedAtMs": HOST_STARTED_AT - 5_000,
            "executable": format!(r"{WIN_INSTALL_DIR}\\DeepSeek Harness.exe"),
        },
        "publicKeySpki": "old-key",
        "retired": true,
    });
    std::fs::write(
        &root_path,
        serde_json::to_vec_pretty(&stale).expect("可序列化"),
    )
    .expect("预置 retired 根记录");

    // 同 PID 新代际 legacy 发布：serve → 根记录必须按新代际正常发布。
    let peer_os = PeerOsIdentity {
        pid: HOST_PID,
        uid: Some(TEST_UID),
    };
    let (legacy_listener, _factory) = mem_listener(peer_os);
    let legacy = Arc::new(
        dsh_auth::DshAuthService::new(clone_env_of(&b.env))
            .expect("legacy 域可装配")
            .with_listener(Box::new(legacy_listener)),
    );
    Arc::clone(&legacy).serve().await.expect("legacy serve");

    let after: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&root_path).expect("根记录在场")).expect("可解析");
    assert_eq!(
        after["retired"], false,
        "PID 复用的新代际发布不得继承旧交接标记（完整四元组判定）"
    );
    let current_epoch = instance_epoch(&b.env, &instance_id).to_string();
    assert_eq!(after["epoch"], current_epoch, "记录已随新代际刷新");
    assert_eq!(after["hostId"], instance_id);
}

/// I3-r3②：并发「读-交接-写」交错——legacy 发布（根写）与实例域交接（根写）
/// 两线程屏障齐发，排他窗口 + 窗口内写合并保证最终 retired 恒为 true
/// （预构造 stale 记录的交错撤销不可能成立）。
#[tokio::test]
async fn concurrent_publish_and_handover_keep_retired_true() {
    let b = bench();
    let instance_id = fixture_instance_a();
    let root_path = b.env.tuvis_home.join("dsh").join("rendezvous.json");
    std::fs::create_dir_all(root_path.parent().unwrap()).expect("基础状态目录");
    let current_epoch = instance_epoch(&b.env, &instance_id).to_string();
    // 预置：同代际、未退休的 legacy 记录（retire 有对象可翻；publish 同代际可合并）。
    let seed = serde_json::json!({
        "bindingProtocol": BINDING_PROTOCOL,
        "address": "seed-endpoint",
        "hostId": instance_id,
        "epoch": current_epoch,
        "processIdentity": {
            "platform": "windows",
            "pid": HOST_PID,
            "startedAtMs": HOST_STARTED_AT,
            "executable": format!(r"{WIN_INSTALL_DIR}\\DeepSeek Harness.exe"),
        },
        "publicKeySpki": "seed-key",
        "retired": false,
    });
    std::fs::write(
        &root_path,
        serde_json::to_vec_pretty(&seed).expect("可序列化"),
    )
    .expect("预置 legacy 记录");

    let peer_os = PeerOsIdentity {
        pid: HOST_PID,
        uid: Some(TEST_UID),
    };
    let env_for_legacy = clone_env_of(&b.env);
    let env_for_instance = clone_env_of(&b.env);
    let barrier = Arc::new(std::sync::Barrier::new(2));

    // 线程 1：legacy 域 serve（根写 publish，root_scope = true）。
    let barrier1 = Arc::clone(&barrier);
    let t1 = std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("线程内运行时");
        rt.block_on(async move {
            let (listener, _factory) = mem_listener(peer_os);
            let svc = Arc::new(
                dsh_auth::DshAuthService::new(env_for_legacy)
                    .expect("legacy 域可装配")
                    .with_listener(Box::new(listener)),
            );
            barrier1.wait();
            svc.serve().await.expect("legacy serve");
        });
    });
    // 线程 2：实例域 serve（根写 retire + 实例记录发布）。
    let barrier2 = Arc::clone(&barrier);
    let instance_id_for_thread = instance_id.clone();
    let t2 = std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("线程内运行时");
        rt.block_on(async move {
            let (listener, _factory) = mem_listener(PeerOsIdentity {
                pid: HOST_PID,
                uid: Some(TEST_UID),
            });
            let svc = Arc::new(
                dsh_instance_production::init_instance_auth(
                    &env_for_instance,
                    &instance_id_for_thread,
                )
                .expect("实例域可装配")
                .with_listener(Box::new(listener)),
            );
            barrier2.wait();
            svc.serve().await.expect("实例域 serve");
        });
    });
    t1.join().expect("线程 1");
    t2.join().expect("线程 2");

    // 两种交错顺序（publish 先 / retire 先）最终态都必须是 retired=true。
    let after: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&root_path).expect("根记录在场")).expect("可解析");
    assert_eq!(
        after["retired"], true,
        "并发交错下交接结果不得被撤销：{after}"
    );
    // 实例记录照常发布（交接的另一面）。
    let inst_path = instance_state_dir(&b.env, &instance_id).join("rendezvous.json");
    let inst: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&inst_path).expect("实例记录在场")).expect("可解析");
    assert_eq!(inst["hostId"], instance_id);
}

// ============================================================
// P1-1a：批准后必须把已核验指纹**锚回实例登记**
//
// 为什么单列：`register_candidate` 落的是**空指纹**记录（打破「登记要指纹、
// 指纹要授权」循环的受限首装配路径），而 `resolve_selected` / `resolve_session` /
// `readonly_root` 的指纹门一律拿登记值与当代核验值比对。授权成立时若不回写，
// 首绑之后所有写入恒 `DSH_INSTANCE_UNVERIFIED`——界面显示「已绑定」而实际
// 什么都读不了、写不了。这是生产链断点，不是「待实机」。
//
// 观测纪律：断言只取自真实批准入口的返回值与**真实登记文件**内容，不预置指纹、
// 不注入 Bound 行、不直接调 rebind。
// ============================================================

/// 内存锚点替身：把「授权域调用了锚点」变成可观测事实，且不写真实状态目录。
///
/// 它**不是**被测对象本身——被测的是「批准路径会不会调锚点」。生产锚点实现
/// （`production::RegistryFingerprintAnchor`）的写盘语义另有一条用例直接打
/// 真实 `InstanceRegistry`。
struct RecordingAnchor {
    calls: Mutex<Vec<(String, String)>>,
    fingerprints: Mutex<BTreeMap<String, String>>,
    /// `Some(err_code)` = 该锚点对任何锚定都返回这个错误码（降级传播用）。
    fail_with: Option<&'static str>,
}

impl dsh_auth::RegistryFingerprintAnchor for RecordingAnchor {
    fn anchor_fingerprint(&self, instance_id: &str, fingerprint: &str) -> Result<(), DshError> {
        self.commit_fingerprint(
            instance_id,
            fingerprint,
            &dsh_auth::FingerprintCommitMode::Normal,
        )
        .map(|_| ())
    }

    fn commit_fingerprint(
        &self,
        instance_id: &str,
        fingerprint: &str,
        mode: &dsh_auth::FingerprintCommitMode,
    ) -> Result<dsh_auth::FingerprintCommitReceipt, DshError> {
        self.calls
            .lock()
            .unwrap()
            .push((instance_id.to_string(), fingerprint.to_string()));
        if let Some(code) = self.fail_with {
            return Err(DshError {
                code: leak(code),
                detail: "anchor refused".into(),
            });
        }
        let mut fingerprints = self.fingerprints.lock().unwrap();
        let previous = fingerprints.get(instance_id).cloned().unwrap_or_default();
        if matches!(mode, dsh_auth::FingerprintCommitMode::Normal)
            && !previous.is_empty()
            && previous != fingerprint
            || matches!(mode, dsh_auth::FingerprintCommitMode::ExplicitRebind { expected_old } if expected_old != &previous)
        {
            return Err(DshError::new(codes::AUTH_SIGNATURE, "anchor conflict"));
        }
        fingerprints.insert(instance_id.to_owned(), fingerprint.to_owned());
        Ok(dsh_auth::FingerprintCommitReceipt {
            instance_id: instance_id.to_owned(),
            changed: previous != fingerprint,
            previous,
            committed: fingerprint.to_owned(),
            // 内存替身无登记只读证据面（生产回执携带提交前证据，回滚一并恢复）。
            previous_read_evidence: String::new(),
        })
    }

    fn rollback_fingerprint(
        &self,
        receipt: &dsh_auth::FingerprintCommitReceipt,
    ) -> Result<(), DshError> {
        let mut fingerprints = self.fingerprints.lock().unwrap();
        if fingerprints
            .get(&receipt.instance_id)
            .map(String::as_str)
            .unwrap_or("")
            != receipt.committed
        {
            return Err(DshError::new(codes::AUTH_SIGNATURE, "rollback conflict"));
        }
        if receipt.changed {
            fingerprints.insert(receipt.instance_id.clone(), receipt.previous.clone());
        }
        Ok(())
    }

    fn current_fingerprint(&self, instance_id: &str) -> Result<String, DshError> {
        Ok(self
            .fingerprints
            .lock()
            .unwrap()
            .get(instance_id)
            .cloned()
            .unwrap_or_default())
    }
}

/// 实例域装配 + mem 传输 + 指定锚点（生产 `init_instance_auth` 内核 + builder）。
async fn served_instance_with_anchor(
    env: &DshEnvironment,
    instance_id: &str,
    peer_pid: u32,
    anchor: Arc<dyn dsh_auth::RegistryFingerprintAnchor>,
) -> (Arc<dsh_auth::DshAuthService>, Arc<MemClientFactory>) {
    let peer_os = PeerOsIdentity {
        pid: peer_pid,
        uid: Some(TEST_UID),
    };
    let (listener, factory) = mem_listener(peer_os);
    let svc = Arc::new(
        dsh_instance_production::init_instance_auth(env, instance_id)
            .expect("实例 auth 域可装配")
            .with_listener(Box::new(listener))
            .with_registry_anchor(anchor),
    );
    Arc::clone(&svc).serve().await.expect("IPC 服务启动");
    (svc, factory)
}

/// **人工批准**路径：批准成立后必须调用锚点，且带的是本次握手的对端公钥摘要。
#[tokio::test]
async fn approval_anchors_the_verified_peer_fingerprint_into_the_registry() {
    let b = dual_bench();
    let id = fixture_instance_a();
    let anchor = Arc::new(RecordingAnchor {
        calls: Mutex::new(Vec::new()),
        fingerprints: Mutex::new(BTreeMap::new()),
        fail_with: None,
    });
    let (svc, factory) = served_instance_with_anchor(&b.env, &id, HOST_PID, anchor.clone()).await;

    let client = InstanceClient::new(&factory, HOST_PID, HOST_STARTED_AT, id.clone()).await;
    let epoch = instance_epoch(&b.env, &id);
    let reply = client
        .connect(&svc, epoch, [21u8; 32])
        .await
        .expect("握手有应答");
    let ServerFrame::ServerHello(hello) = reply else {
        panic!("首绑应得 serverHello，得到 {reply:?}")
    };
    assert_eq!(hello.state, "pending_approval");
    let binding_id = hello.binding_id.clone().expect("pending 必带 binding_id");
    assert!(
        anchor.calls.lock().unwrap().is_empty(),
        "待批阶段不得锚定（批准前零信任）"
    );

    svc.approve(&binding_id).await.expect("用户批准");

    let expected = dsh_auth::key_digest(&client.keys.spki);
    let calls = anchor.calls.lock().unwrap().clone();
    assert_eq!(
        calls,
        vec![(id.clone(), expected)],
        "人工批准必须把本次握手的对端公钥摘要锚到该实例（否则指纹门永远拿不到登记值）"
    );
}

/// **一次性授权意图**（设置卡「一键启用」）路径同样必须锚定。
#[tokio::test]
async fn one_shot_intent_authorization_anchors_the_fingerprint_too() {
    let b = dual_bench();
    let id = fixture_instance_a();
    let anchor = Arc::new(RecordingAnchor {
        calls: Mutex::new(Vec::new()),
        fingerprints: Mutex::new(BTreeMap::new()),
        fail_with: None,
    });
    let (svc, factory) = served_instance_with_anchor(&b.env, &id, HOST_PID, anchor.clone()).await;
    svc.begin_authorization_intent().expect("布防一次性意图");

    let client = InstanceClient::new(&factory, HOST_PID, HOST_STARTED_AT, id.clone()).await;
    let epoch = instance_epoch(&b.env, &id);
    let reply = client
        .connect(&svc, epoch, [22u8; 32])
        .await
        .expect("握手有应答");
    assert!(
        matches!(reply, ServerFrame::ServerHello(ref h) if h.state == "authorized"),
        "布防后首个通过全部核验的新钥应自动授权：{reply:?}"
    );

    let expected = dsh_auth::key_digest(&client.keys.spki);
    assert_eq!(
        anchor.calls.lock().unwrap().clone(),
        vec![(id.clone(), expected)],
        "自动授权也是「本代已核验」，漏锚同样让后续写入恒不可核验"
    );
}

/// 已钉钥重连：持久锚点与账本匹配时，换代同钥可授权且锚点不变。
#[tokio::test]
async fn pinned_key_reconnect_preserves_matching_fingerprint_after_generation_change() {
    let b = dual_bench();
    let id = fixture_instance_a();
    let anchor = Arc::new(RecordingAnchor {
        calls: Mutex::new(Vec::new()),
        fingerprints: Mutex::new(BTreeMap::new()),
        fail_with: None,
    });
    let (svc, factory) = served_instance_with_anchor(&b.env, &id, HOST_PID, anchor.clone()).await;

    // 首绑 + 批准（锚定第一次）。
    let first = InstanceClient::new(&factory, HOST_PID, HOST_STARTED_AT, id.clone()).await;
    let epoch1 = instance_epoch(&b.env, &id);
    let reply = first
        .connect(&svc, epoch1, [23u8; 32])
        .await
        .expect("握手有应答");
    let ServerFrame::ServerHello(hello) = reply else {
        panic!("首绑应得 serverHello，得到 {reply:?}")
    };
    svc.approve(&hello.binding_id.clone().expect("binding_id"))
        .await
        .expect("批准");
    let expected = dsh_auth::key_digest(&first.keys.spki);
    let before = anchor.fingerprints.lock().unwrap().get(&id).cloned();
    assert_eq!(
        before.as_deref(),
        Some(expected.as_str()),
        "首绑持久锚点必须匹配"
    );

    // 宿主换代（同 exe/profile，新启动时刻）→ 同钥重连走「已钉钥」分支。
    bump_instance_a_started_at(&b.procs, HOST_STARTED_AT + 7_000);
    let epoch2 = instance_epoch(&b.env, &id);
    assert_ne!(epoch1, epoch2, "换代必须换代际");
    let again = InstanceClient::with_keys(
        &factory,
        &first.keys,
        HOST_PID,
        HOST_STARTED_AT + 7_000,
        id.clone(),
    )
    .await;
    let reply = again
        .connect(&svc, epoch2, [24u8; 32])
        .await
        .expect("握手有应答");
    assert!(
        matches!(reply, ServerFrame::ServerHello(ref h) if h.state == "authorized"),
        "已钉钥的当代握手应直接 authorized：{reply:?}"
    );
    assert_eq!(
        anchor.fingerprints.lock().unwrap().get(&id).cloned(),
        before,
        "同钥重连不得改变已匹配的锚点"
    );
}

/// 锚点拒绝必须**传播**给批准调用方——不能「批准成功但登记值没锚上」静默半成品。
#[tokio::test]
async fn approval_fails_loudly_when_the_registry_anchor_refuses() {
    let b = dual_bench();
    let id = fixture_instance_a();
    let anchor = Arc::new(RecordingAnchor {
        calls: Mutex::new(Vec::new()),
        fingerprints: Mutex::new(BTreeMap::new()),
        fail_with: Some(codes::AUTH_SIGNATURE),
    });
    let (svc, factory) = served_instance_with_anchor(&b.env, &id, HOST_PID, anchor.clone()).await;

    let client = InstanceClient::new(&factory, HOST_PID, HOST_STARTED_AT, id.clone()).await;
    let epoch = instance_epoch(&b.env, &id);
    let reply = client
        .connect(&svc, epoch, [25u8; 32])
        .await
        .expect("握手有应答");
    let ServerFrame::ServerHello(hello) = reply else {
        panic!("首绑应得 serverHello，得到 {reply:?}")
    };
    let err = svc
        .approve(&hello.binding_id.clone().expect("binding_id"))
        .await
        .expect_err("锚点拒绝时批准必须报错，不得静默成功");
    assert_eq!(
        err.code,
        codes::AUTH_SIGNATURE,
        "锚点错误原样上抛（用户要能看见「登记值锚不上」）"
    );
}

/// P1：指纹冲突时，批准必须**不留下绑定账本**（半完成状态）且不改内存受信集。
///
/// 威胁：旧顺序先写 Bound 账本再锚登记值。锚定被拒时新钥已受信、账本多了
/// 一行，而用户看到的是失败——重试走「同钥已 Bound」分支，登记面仍是旧身份，
/// 后续所有写入被指纹门拒。这条用例断言顺序已改：预检先拒，账本零变化。
#[tokio::test]
async fn approval_leaves_no_bound_row_when_the_registry_identity_conflicts() {
    let b = dual_bench();
    let id = fixture_instance_a();
    // 锚点：登记面已钉住另一个身份 → 预检必然拒绝。
    struct ConflictingAnchor;
    impl dsh_auth::RegistryFingerprintAnchor for ConflictingAnchor {
        fn accepts(&self, _instance_id: &str, _fingerprint: &str) -> bool {
            false
        }
        fn anchor_fingerprint(
            &self,
            _instance_id: &str,
            _fingerprint: &str,
        ) -> Result<(), DshError> {
            Err(DshError {
                code: leak(codes::AUTH_SIGNATURE),
                detail: "conflict".into(),
            })
        }
        fn commit_fingerprint(
            &self,
            _instance_id: &str,
            _fingerprint: &str,
            _mode: &dsh_auth::FingerprintCommitMode,
        ) -> Result<dsh_auth::FingerprintCommitReceipt, DshError> {
            Err(DshError::new(codes::AUTH_SIGNATURE, "conflict"))
        }
        fn rollback_fingerprint(
            &self,
            _receipt: &dsh_auth::FingerprintCommitReceipt,
        ) -> Result<(), DshError> {
            Err(DshError::new(
                codes::AUTH_SIGNATURE,
                "no commit to roll back",
            ))
        }
        fn current_fingerprint(&self, _instance_id: &str) -> Result<String, DshError> {
            Ok("other-identity".to_owned())
        }
    }
    let (svc, factory) =
        served_instance_with_anchor(&b.env, &id, HOST_PID, Arc::new(ConflictingAnchor)).await;
    let ledger = instance_ledger_file(&b.env, &id);
    assert!(!ledger.exists(), "前置条件：从无账本开始");

    let client = InstanceClient::new(&factory, HOST_PID, HOST_STARTED_AT, id.clone()).await;
    let epoch = instance_epoch(&b.env, &id);
    let reply = client
        .connect(&svc, epoch, [31u8; 32])
        .await
        .expect("握手有应答");
    let ServerFrame::ServerHello(hello) = reply else {
        panic!("首绑应得 serverHello，得到 {reply:?}")
    };
    let err = svc
        .approve(&hello.binding_id.clone().expect("binding_id"))
        .await
        .expect_err("登记身份冲突时批准必须拒绝");
    assert_eq!(err.code, codes::AUTH_SIGNATURE, "{err}");

    // 关键断言：账本不得留下任何 Bound 行（顺序已改成「先预检、后落账」）。
    if ledger.exists() {
        let raw = std::fs::read_to_string(&ledger).unwrap_or_default();
        assert!(
            !raw.contains("\"bound\""),
            "指纹冲突时不得写下 Bound 账本（否则新钥已受信而登记面仍是旧身份，\
             形成半完成状态）：{raw}"
        );
    }
    assert_eq!(
        svc.authorization_state(),
        AuthorizationState::Unbound,
        "被拒的批准不得把服务报成已绑定"
    );
}

// ============================================================
// M03：绑定提交（CAS + 单提交边界）
// ============================================================

/// 可控锚点仅用于注入提交失败；账本、握手和凭据交付均走生产入口。
#[derive(Default)]
struct FailingCommitAnchor {
    fingerprint: Mutex<Option<String>>,
    reject_new_key: bool,
}

impl dsh_auth::RegistryFingerprintAnchor for FailingCommitAnchor {
    fn anchor_fingerprint(&self, instance_id: &str, fingerprint: &str) -> Result<(), DshError> {
        self.commit_fingerprint(
            instance_id,
            fingerprint,
            &dsh_auth::FingerprintCommitMode::Normal,
        )
        .map(|_| ())
    }

    fn commit_fingerprint(
        &self,
        instance_id: &str,
        fingerprint: &str,
        mode: &dsh_auth::FingerprintCommitMode,
    ) -> Result<dsh_auth::FingerprintCommitReceipt, DshError> {
        let mut current = self.fingerprint.lock().unwrap();
        let previous = current.clone().unwrap_or_default();
        if self.reject_new_key && !previous.is_empty() && previous != fingerprint
            || matches!(mode, dsh_auth::FingerprintCommitMode::ExplicitRebind { expected_old } if expected_old != &previous)
        {
            return Err(DshError::new(
                codes::AUTH_SIGNATURE,
                "anchor commit refused",
            ));
        }
        *current = Some(fingerprint.to_owned());
        Ok(dsh_auth::FingerprintCommitReceipt {
            instance_id: instance_id.to_owned(),
            changed: previous != fingerprint,
            previous,
            committed: fingerprint.to_owned(),
            // 内存替身无登记只读证据面（生产回执携带提交前证据，回滚一并恢复）。
            previous_read_evidence: String::new(),
        })
    }

    fn rollback_fingerprint(
        &self,
        receipt: &dsh_auth::FingerprintCommitReceipt,
    ) -> Result<(), DshError> {
        let mut current = self.fingerprint.lock().unwrap();
        if current.as_deref().unwrap_or("") != receipt.committed {
            return Err(DshError::new(codes::AUTH_SIGNATURE, "rollback conflict"));
        }
        if receipt.changed {
            *current = if receipt.previous.is_empty() {
                None
            } else {
                Some(receipt.previous.clone())
            };
        }
        Ok(())
    }

    fn current_fingerprint(&self, _instance_id: &str) -> Result<String, DshError> {
        Ok(self.fingerprint.lock().unwrap().clone().unwrap_or_default())
    }
}

/// 预检放行后锚定提交拒绝：不能先把新钥写入账本或发布为受信连接。
#[tokio::test]
async fn anchor_commit_rejection_preserves_old_binding_and_rejects_new_credentials() {
    if run_instance_credential_fixture_in_child(
        "anchor_commit_rejection_preserves_old_binding_and_rejects_new_credentials",
    ) {
        return;
    }
    let b = dual_bench();
    let id = fixture_instance_a();
    let anchor = Arc::new(FailingCommitAnchor {
        reject_new_key: true,
        ..Default::default()
    });
    let (svc, factory) = served_instance_with_anchor(&b.env, &id, HOST_PID, anchor.clone()).await;
    let epoch = instance_epoch(&b.env, &id);
    let (old, old_binding) = instance_bind_and_deliver(
        &svc,
        &factory,
        (HOST_PID, HOST_STARTED_AT, id.clone()),
        epoch,
        [201; 32],
        "old-cookie",
    )
    .await;
    let old_fingerprint = dsh_auth::key_digest(&old.keys.spki);
    assert_eq!(
        anchor.fingerprint.lock().unwrap().as_deref(),
        Some(old_fingerprint.as_str())
    );
    let ledger = instance_ledger_file(&b.env, &id);
    let before = std::fs::read(&ledger).expect("旧账本可读");

    let new = InstanceClient::new(&factory, HOST_PID, HOST_STARTED_AT, id.clone()).await;
    let reply = new.connect(&svc, epoch, [202; 32]).await.expect("新钥握手");
    let ServerFrame::ServerHello(hello) = reply else {
        panic!("应得待批 serverHello：{reply:?}");
    };
    assert_eq!(hello.state, "pending_approval");
    let binding = hello.binding_id.expect("待批绑定号");
    assert!(
        svc.approve(&binding).await.is_err(),
        "锚定提交失败必须拒绝批准"
    );

    assert_eq!(std::fs::read(&ledger).unwrap(), before, "失败不得改变账本");
    assert_eq!(
        svc.active_binding_id().as_deref(),
        Some(old_binding.as_str())
    );
    assert_eq!(
        anchor.fingerprint.lock().unwrap().as_deref(),
        Some(old_fingerprint.as_str())
    );
    assert!(svc.has_delivered_peer(&old_binding), "旧凭据保持可用");
    assert!(!svc.has_delivered_peer(&binding));
    assert!(
        new.drain()
            .await
            .iter()
            .all(|frame| !matches!(frame, ServerFrame::Granted { .. })),
        "失败不得发送放行帧"
    );
    let delivery = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        new.send_credential(&binding, "new-cookie"),
    )
    .await
    .expect("凭据交付必须及时拒绝或连接关闭");
    assert!(
        !matches!(delivery, Ok(ServerFrame::Granted { .. })),
        "失败的新钥不得交付凭据"
    );
    assert!(!svc.has_delivered_peer(&binding));
    assert_eq!(
        svc.peer_for_control()
            .await
            .expect("旧 Peer 保持可用")
            .binding_id(),
        old_binding
    );
}

/// 一次性意图不得把锚点拒绝的新钥留在账本或受信集。
#[tokio::test]
async fn automatic_approval_anchor_rejection_preserves_old_binding() {
    if run_instance_credential_fixture_in_child(
        "automatic_approval_anchor_rejection_preserves_old_binding",
    ) {
        return;
    }
    let b = dual_bench();
    let id = fixture_instance_a();
    let anchor = Arc::new(FailingCommitAnchor {
        reject_new_key: true,
        ..Default::default()
    });
    let (svc, factory) = served_instance_with_anchor(&b.env, &id, HOST_PID, anchor.clone()).await;
    let epoch = instance_epoch(&b.env, &id);
    let (old, old_binding) = instance_bind_and_deliver(
        &svc,
        &factory,
        (HOST_PID, HOST_STARTED_AT, id.clone()),
        epoch,
        [204; 32],
        "old-cookie",
    )
    .await;
    let old_fingerprint = dsh_auth::key_digest(&old.keys.spki);
    let ledger = instance_ledger_file(&b.env, &id);
    let before = std::fs::read(&ledger).expect("旧账本可读");
    svc.begin_authorization_intent().expect("布防一次性意图");
    let new = InstanceClient::new(&factory, HOST_PID, HOST_STARTED_AT, id.clone()).await;
    let result = new.connect(&svc, epoch, [205; 32]).await;
    assert!(result.is_err(), "锚定拒绝不得返回自动授权：{result:?}");
    assert_eq!(std::fs::read(&ledger).unwrap(), before, "失败不得改变账本");
    assert_eq!(
        svc.active_binding_id().as_deref(),
        Some(old_binding.as_str())
    );
    assert_eq!(
        anchor.fingerprint.lock().unwrap().as_deref(),
        Some(old_fingerprint.as_str())
    );
    assert!(svc.has_delivered_peer(&old_binding));
    assert_eq!(
        svc.peer_for_control()
            .await
            .expect("旧 Peer 保持可用")
            .binding_id(),
        old_binding
    );

    let retry = InstanceClient::with_keys(&factory, &new.keys, HOST_PID, HOST_STARTED_AT, id).await;
    let reply = retry
        .connect(&svc, epoch, [206; 32])
        .await
        .expect("新钥重新握手");
    let ServerFrame::ServerHello(hello) = reply else {
        panic!("重新握手应得 serverHello：{reply:?}");
    };
    assert_eq!(hello.state, "pending_approval", "失败的新钥不得变成已绑定");
    let binding = hello.binding_id.expect("待批绑定号");
    let delivery = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        retry.send_credential(&binding, "new-cookie"),
    )
    .await
    .expect("凭据交付必须及时拒绝或连接关闭");
    assert!(!matches!(delivery, Ok(ServerFrame::Granted { .. })));
    assert!(!svc.has_delivered_peer(&binding));
}

/// 账本原子写失败时，不能把首次绑定发布成受信 Peer 或留下锚点。
#[tokio::test]
async fn ledger_write_failure_does_not_publish_first_binding_or_anchor() {
    let b = dual_bench();
    let id = fixture_instance_a();
    let anchor = Arc::new(FailingCommitAnchor::default());
    let (svc, factory) = served_instance_with_anchor(&b.env, &id, HOST_PID, anchor.clone()).await;
    let ledger = instance_ledger_file(&b.env, &id);
    assert!(!ledger.exists());
    let client = InstanceClient::new(&factory, HOST_PID, HOST_STARTED_AT, id.clone()).await;
    let reply = client
        .connect(&svc, instance_epoch(&b.env, &id), [203; 32])
        .await
        .expect("握手");
    let ServerFrame::ServerHello(hello) = reply else {
        panic!("应得待批 serverHello：{reply:?}");
    };
    assert_eq!(hello.state, "pending_approval");
    let binding = hello.binding_id.expect("待批绑定号");
    std::fs::create_dir(ledger.with_extension("tmp")).expect("阻塞账本临时文件");
    assert!(
        svc.approve(&binding).await.is_err(),
        "账本写失败必须拒绝批准"
    );
    assert!(!ledger.exists(), "失败不得产生账本");
    assert_eq!(svc.authorization_state(), AuthorizationState::Unbound);
    assert_eq!(svc.verified_peer_fingerprint().expect("宿主可核验"), None);
    assert_eq!(
        *anchor.fingerprint.lock().unwrap(),
        None,
        "失败后锚点必须恢复空值"
    );
    assert!(!svc.has_delivered_peer(&binding));
    assert!(client
        .drain()
        .await
        .iter()
        .all(|frame| !matches!(frame, ServerFrame::Granted { .. })));
    let delivery = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        client.send_credential(&binding, "new-cookie"),
    )
    .await
    .expect("凭据交付必须及时拒绝或连接关闭");
    assert!(!matches!(delivery, Ok(ServerFrame::Granted { .. })));
    assert!(!svc.has_delivered_peer(&binding));
    assert!(svc.peer_for_control().await.is_none());
}

/// 真实登记和宿主身份；每条测试均使用独立临时目录。
struct RealRegistryAuthBench {
    _root: tempfile::TempDir,
    env: DshEnvironment,
    id: String,
    home: PathBuf,
    registry: Arc<InstanceRegistry>,
    clock: Arc<VarClock>,
}

fn real_registry_auth_bench() -> RealRegistryAuthBench {
    let root = tempfile::tempdir().expect("临时目录");
    let home = root.path().join("home");
    let profile = home.join("profiles/desktop");
    std::fs::create_dir_all(&profile).unwrap();
    std::fs::write(profile.join("package.json"), b"{}").unwrap();
    let procs = Arc::new(SharedProcs {
        processes: Mutex::new(vec![real_desktop_process(
            HOST_PID,
            &profile,
            HOST_STARTED_AT,
        )]),
        listeners: Mutex::new(vec![DshListener {
            owner_pid: HOST_PID,
            authority: "127.0.0.1:53002".into(),
        }]),
    });
    let clock = Arc::new(VarClock(Mutex::new(HOST_STARTED_AT)));
    let env = DshEnvironment {
        home: root.path().join("env-home"),
        tuvis_home: root.path().join("tuvis-home"),
        dsh_state_dir: None,
        processes: procs,
        files: Arc::new(RealFsFiles),
        runner: Arc::new(NullRunner),
        clock: clock.clone(),
        environments: Arc::new(
            multi_agents_manager_lib::services::dsh_control::model::TestUnobservableEnvironments,
        ),
    };
    let registry = Arc::new(InstanceRegistry::open(root.path().join("instances.json")).unwrap());
    let id = registry
        .register_candidate(HostKind::Desktop, &profile)
        .unwrap()
        .instance_id;
    registry.select(&id).expect("选中待批准实例");
    RealRegistryAuthBench {
        _root: root,
        env,
        id,
        home,
        registry,
        clock,
    }
}

async fn real_registry_client(
    factory: &Arc<MemClientFactory>,
    b: &RealRegistryAuthBench,
) -> InstanceClient {
    let mut client = InstanceClient::new(factory, HOST_PID, HOST_STARTED_AT, b.id.clone()).await;
    client.executable = "/Applications/DeepSeek Harness.app/Contents/MacOS/DeepSeek Harness".into();
    client.home_root = b.home.to_string_lossy().to_string();
    client
}

#[derive(Clone, Copy)]
enum ApprovalExpiryPause {
    AfterPreflight,
    AfterRealCas,
}

/// 包装真实锚点，使用通知和条件变量确定性暂停批准线程。
struct PausingApprovalAnchor {
    real: dsh_instance_production::RegistryFingerprintAnchor,
    pause: ApprovalExpiryPause,
    entered: tokio::sync::Notify,
    released: Mutex<bool>,
    release: std::sync::Condvar,
}

impl PausingApprovalAnchor {
    fn pause_here(&self) {
        self.entered.notify_one();
        let released = self.released.lock().unwrap();
        let (released, timeout) = self
            .release
            .wait_timeout_while(released, std::time::Duration::from_secs(10), |released| {
                !*released
            })
            .unwrap();
        assert!(
            *released && !timeout.timed_out(),
            "测试必须明确放行暂停的批准"
        );
    }

    fn resume(&self) {
        *self.released.lock().unwrap() = true;
        self.release.notify_one();
    }
}

impl dsh_auth::RegistryFingerprintAnchor for PausingApprovalAnchor {
    fn accepts(&self, instance_id: &str, fingerprint: &str) -> bool {
        let accepted =
            dsh_auth::RegistryFingerprintAnchor::accepts(&self.real, instance_id, fingerprint);
        if matches!(self.pause, ApprovalExpiryPause::AfterPreflight) {
            assert!(accepted, "必须先通过真实预检");
            self.pause_here();
        }
        accepted
    }
    fn anchor_fingerprint(&self, instance_id: &str, fingerprint: &str) -> Result<(), DshError> {
        dsh_auth::RegistryFingerprintAnchor::anchor_fingerprint(
            &self.real,
            instance_id,
            fingerprint,
        )
    }
    fn commit_fingerprint(
        &self,
        instance_id: &str,
        fingerprint: &str,
        mode: &dsh_auth::FingerprintCommitMode,
    ) -> Result<dsh_auth::FingerprintCommitReceipt, DshError> {
        let receipt = dsh_auth::RegistryFingerprintAnchor::commit_fingerprint(
            &self.real,
            instance_id,
            fingerprint,
            mode,
        )?;
        if matches!(self.pause, ApprovalExpiryPause::AfterRealCas) {
            assert!(receipt.changed, "首绑必须真实提交新的锚点");
            self.pause_here();
        }
        Ok(receipt)
    }
    fn rollback_fingerprint(
        &self,
        receipt: &dsh_auth::FingerprintCommitReceipt,
    ) -> Result<(), DshError> {
        dsh_auth::RegistryFingerprintAnchor::rollback_fingerprint(&self.real, receipt)
    }
    fn current_fingerprint(&self, instance_id: &str) -> Result<String, DshError> {
        dsh_auth::RegistryFingerprintAnchor::current_fingerprint(&self.real, instance_id)
    }
}

async fn assert_approval_expired_at_commit_boundary(pause: ApprovalExpiryPause) {
    let b = real_registry_auth_bench();
    let anchor = Arc::new(PausingApprovalAnchor {
        real: dsh_instance_production::RegistryFingerprintAnchor::with_registry(b.registry.clone()),
        pause,
        entered: tokio::sync::Notify::new(),
        released: Mutex::new(false),
        release: std::sync::Condvar::new(),
    });
    let (svc, factory) = served_instance_with_anchor(&b.env, &b.id, HOST_PID, anchor.clone()).await;
    let client = real_registry_client(&factory, &b).await;
    let reply = client
        .connect(&svc, instance_epoch(&b.env, &b.id), [219; 32])
        .await
        .unwrap();
    let ServerFrame::ServerHello(hello) = reply else {
        panic!("首绑应待批：{reply:?}")
    };
    assert_eq!(hello.state, "pending_approval");
    let deadline = hello.pending_expires_at.expect("待批应带实际截止时间");
    let binding = hello.binding_id.expect("待批绑定号");
    assert!(svc.now_ms() <= deadline, "批准启动前挑战必须有效");
    assert_eq!(
        b.registry.registered(&b.id).unwrap().identity_fingerprint,
        ""
    );
    let task_svc = Arc::clone(&svc);
    let task_binding = binding.clone();
    let task = tokio::spawn(async move { task_svc.approve(&task_binding).await });
    tokio::time::timeout(std::time::Duration::from_secs(5), anchor.entered.notified())
        .await
        .expect("批准必须到达指定屏障");
    let fingerprint_at_pause = b.registry.registered(&b.id).unwrap().identity_fingerprint;
    match pause {
        ApprovalExpiryPause::AfterPreflight => {
            assert_eq!(fingerprint_at_pause, "", "预检后尚未提交")
        }
        ApprovalExpiryPause::AfterRealCas => assert_eq!(
            fingerprint_at_pause,
            dsh_auth::key_digest(&client.keys.spki),
            "暂停前真实 CAS 必须成功"
        ),
    }
    b.clock.set(deadline.checked_add(1).unwrap());
    anchor.resume();
    let result = tokio::time::timeout(std::time::Duration::from_secs(5), task)
        .await
        .expect("批准不得悬挂")
        .expect("批准任务不得 panic");
    assert!(result.is_err(), "挑战过期后不得批准成功");
    assert_eq!(
        b.registry.registered(&b.id).unwrap().identity_fingerprint,
        "",
        "失败必须保留或恢复空锚点"
    );
    let ledger = instance_ledger_file(&b.env, &b.id);
    if ledger.exists() {
        let value: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&ledger).unwrap()).unwrap();
        assert!(
            value["bindings"]
                .as_array()
                .unwrap()
                .iter()
                .all(|row| row["state"] != "bound"),
            "过期不得留下 Bound：{value}"
        );
    }
    assert_eq!(svc.authorization_state(), AuthorizationState::Unbound);
    assert_eq!(svc.verified_peer_fingerprint().unwrap(), None);
    assert!(!svc.has_delivered_peer(&binding));
    assert!(svc.peer_for_control().await.is_none());
    assert!(client
        .drain()
        .await
        .iter()
        .all(|frame| !matches!(frame, ServerFrame::Granted { .. })));
    let delivery = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        client.send_credential(&binding, "expired-cookie"),
    )
    .await
    .expect("凭据交付应拒绝或关闭");
    assert!(!matches!(delivery, Ok(ServerFrame::Granted { .. })));
    assert!(!svc.has_delivered_peer(&binding));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn approval_rejects_when_challenge_expires_after_preflight() {
    assert_approval_expired_at_commit_boundary(ApprovalExpiryPause::AfterPreflight).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn approval_rolls_back_when_challenge_expires_during_real_anchor_commit() {
    assert_approval_expired_at_commit_boundary(ApprovalExpiryPause::AfterRealCas).await;
}

/// 构造持久 Bound 与登记锚点不一致，再用新服务实例验证恢复门。
async fn assert_reconnect_rejects_inconsistent_real_anchor(missing: bool) {
    let b = real_registry_auth_bench();
    let anchor = Arc::new(
        dsh_instance_production::RegistryFingerprintAnchor::with_registry(b.registry.clone()),
    );
    let (original, factory) = served_instance_with_anchor(&b.env, &b.id, HOST_PID, anchor).await;
    let first = real_registry_client(&factory, &b).await;
    let epoch = instance_epoch(&b.env, &b.id);
    let reply = first.connect(&original, epoch, [217; 32]).await.unwrap();
    let ServerFrame::ServerHello(hello) = reply else {
        panic!("首绑应待批：{reply:?}")
    };
    assert_eq!(hello.state, "pending_approval");
    let binding = hello.binding_id.unwrap();
    original.approve(&binding).await.expect("真实批准首绑");
    let original_fp = dsh_auth::key_digest(&first.keys.spki);
    assert_eq!(
        b.registry.registered(&b.id).unwrap().identity_fingerprint,
        original_fp
    );
    let ledger_path = instance_ledger_file(&b.env, &b.id);
    let ledger_before = std::fs::read(&ledger_path).expect("真实 Bound 账本已落盘");
    assert_eq!(
        dsh_auth::pinned_fingerprint_for(&b.env, &b.id).as_deref(),
        Some(original_fp.as_str())
    );

    // 只修改临时登记文件，模拟恢复时的缺失/不匹配；账本仍为真实批准产物。
    let registry_path = b._root.path().join("instances.json");
    let mut disk: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&registry_path).unwrap()).unwrap();
    let row = disk["instances"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|row| row["instanceId"].as_str() == Some(b.id.as_str()))
        .unwrap();
    let inconsistent = if missing {
        String::new()
    } else {
        dsh_auth::key_digest(&RustSigningKey::generate().spki)
    };
    assert_ne!(inconsistent, original_fp);
    row["identityFingerprint"] = serde_json::Value::String(inconsistent.clone());
    std::fs::write(&registry_path, serde_json::to_vec_pretty(&disk).unwrap()).unwrap();
    let registry_before = std::fs::read(&registry_path).unwrap();
    assert_eq!(
        b.registry.registered(&b.id).unwrap().identity_fingerprint,
        inconsistent
    );

    let anchor = Arc::new(
        dsh_instance_production::RegistryFingerprintAnchor::with_registry(b.registry.clone()),
    );
    let (restarted, factory) = served_instance_with_anchor(&b.env, &b.id, HOST_PID, anchor).await;
    let mut retry = InstanceClient::with_keys(
        &factory,
        &first.keys,
        HOST_PID,
        HOST_STARTED_AT,
        b.id.clone(),
    )
    .await;
    retry.executable = "/Applications/DeepSeek Harness.app/Contents/MacOS/DeepSeek Harness".into();
    retry.home_root = b.home.to_string_lossy().to_string();
    let reply = retry
        .hello_frame(&restarted, epoch, [218; 32])
        .await
        .expect("重连必须返回拒绝帧");
    assert!(
        matches!(&reply, ServerFrame::Error { error, .. } if error == codes::AUTH_SIGNATURE),
        "不一致不得授权：{reply:?}"
    );
    assert_eq!(
        std::fs::read(&registry_path).unwrap(),
        registry_before,
        "拒绝时不得自动补写或替换锚点"
    );
    assert_eq!(
        std::fs::read(&ledger_path).unwrap(),
        ledger_before,
        "拒绝不得改变 Bound 账本"
    );
    assert_eq!(restarted.verified_peer_fingerprint().unwrap(), None);
    assert!(!restarted.has_delivered_peer(&binding));
    assert!(restarted.peer_for_control().await.is_none());
    let delivery = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        retry.send_credential(&binding, "untrusted-cookie"),
    )
    .await
    .expect("凭据交付应拒绝或连接关闭");
    assert!(!matches!(delivery, Ok(ServerFrame::Granted { .. })));
    assert!(!restarted.has_delivered_peer(&binding));
    assert!(restarted.peer_for_control().await.is_none());
}

#[tokio::test]
async fn pinned_reconnect_rejects_missing_real_registry_fingerprint() {
    assert_reconnect_rejects_inconsistent_real_anchor(true).await;
}

#[tokio::test]
async fn pinned_reconnect_rejects_mismatched_real_registry_fingerprint() {
    assert_reconnect_rejects_inconsistent_real_anchor(false).await;
}

type CapturedConnectionSlot = Arc<
    Mutex<
        Option<
            Arc<
                dyn multi_agents_manager_lib::services::dsh_control::local_ipc::IpcClientConnection,
            >,
        >,
    >,
>;

/// 捕获真实内存传输的服务端连接，以便在真实登记提交边界关闭它。
struct CapturingListener {
    inner: multi_agents_manager_lib::services::dsh_control::local_ipc::mem::MemIpcListener,
    connection: CapturedConnectionSlot,
}

struct CapturedConnection(
    Arc<dyn multi_agents_manager_lib::services::dsh_control::local_ipc::IpcClientConnection>,
);

impl multi_agents_manager_lib::services::dsh_control::local_ipc::IpcClientConnection
    for CapturedConnection
{
    fn peer_os(&self) -> PeerOsIdentity {
        self.0.peer_os()
    }
    fn recv(
        &self,
    ) -> multi_agents_manager_lib::services::dsh_control::local_ipc::IpcFut<
        '_,
        Result<String, DshError>,
    > {
        self.0.recv()
    }
    fn send(
        &self,
        frame: &str,
    ) -> multi_agents_manager_lib::services::dsh_control::local_ipc::IpcFut<'_, Result<(), DshError>>
    {
        self.0.send(frame)
    }
    fn close(self: Arc<Self>) {
        Arc::clone(&self.0).close();
    }
    fn is_closed(&self) -> bool {
        self.0.is_closed()
    }
}

impl multi_agents_manager_lib::services::dsh_control::local_ipc::IpcListener for CapturingListener {
    fn bind(
        &self,
    ) -> multi_agents_manager_lib::services::dsh_control::local_ipc::IpcFut<
        '_,
        Result<String, DshError>,
    > {
        self.inner.bind()
    }
    fn accept(
        &self,
    ) -> multi_agents_manager_lib::services::dsh_control::local_ipc::IpcFut<
        '_,
        Result<
            Box<
                dyn multi_agents_manager_lib::services::dsh_control::local_ipc::IpcClientConnection,
            >,
            DshError,
        >,
    > {
        Box::pin(async move {
            let conn: Arc<
                dyn multi_agents_manager_lib::services::dsh_control::local_ipc::IpcClientConnection,
            > = Arc::from(self.inner.accept().await?);
            *self.connection.lock().unwrap() = Some(Arc::clone(&conn));
            Ok(Box::new(CapturedConnection(conn)) as Box<dyn multi_agents_manager_lib::services::dsh_control::local_ipc::IpcClientConnection>)
        })
    }
    fn stop(&self) {
        self.inner.stop();
    }
    fn endpoint(&self) -> Option<String> {
        self.inner.endpoint()
    }
}

/// 在真实提交前切换选择，或在真实提交后关闭连接；不替代 Registry。
struct CommitBoundaryAnchor {
    real: dsh_instance_production::RegistryFingerprintAnchor,
    registry: Arc<InstanceRegistry>,
    switch_to: Option<String>,
    close: Option<CapturedConnectionSlot>,
    reached: std::sync::atomic::AtomicBool,
}

impl dsh_auth::RegistryFingerprintAnchor for CommitBoundaryAnchor {
    fn accepts(&self, instance_id: &str, fingerprint: &str) -> bool {
        dsh_auth::RegistryFingerprintAnchor::accepts(&self.real, instance_id, fingerprint)
    }
    fn anchor_fingerprint(&self, instance_id: &str, fingerprint: &str) -> Result<(), DshError> {
        dsh_auth::RegistryFingerprintAnchor::anchor_fingerprint(
            &self.real,
            instance_id,
            fingerprint,
        )
    }
    fn current_fingerprint(&self, instance_id: &str) -> Result<String, DshError> {
        dsh_auth::RegistryFingerprintAnchor::current_fingerprint(&self.real, instance_id)
    }
    fn rollback_fingerprint(
        &self,
        receipt: &dsh_auth::FingerprintCommitReceipt,
    ) -> Result<(), DshError> {
        dsh_auth::RegistryFingerprintAnchor::rollback_fingerprint(&self.real, receipt)
    }
    fn commit_fingerprint(
        &self,
        instance_id: &str,
        fingerprint: &str,
        mode: &dsh_auth::FingerprintCommitMode,
    ) -> Result<dsh_auth::FingerprintCommitReceipt, DshError> {
        if let Some(id) = &self.switch_to {
            assert_eq!(
                self.registry.selected()?.unwrap().instance_id,
                instance_id,
                "提交前原实例必须已选中"
            );
            self.registry.select(id)?;
            self.reached
                .store(true, std::sync::atomic::Ordering::SeqCst);
        }
        let receipt = dsh_auth::RegistryFingerprintAnchor::commit_fingerprint(
            &self.real,
            instance_id,
            fingerprint,
            mode,
        )?;
        if let Some(slot) = &self.close {
            assert_eq!(
                self.registry.registered(instance_id)?.identity_fingerprint,
                fingerprint,
                "先确认真实 CAS 已提交"
            );
            let connection = slot
                .lock()
                .unwrap()
                .as_ref()
                .cloned()
                .expect("服务端连接已捕获");
            connection.close();
            self.reached
                .store(true, std::sync::atomic::Ordering::SeqCst);
        }
        Ok(receipt)
    }
}

/// 包装真实锚点：确认登记已写入后才阻塞账本写入，不替代登记或伪造 CAS。
struct BlockLedgerAfterRealAnchor {
    real: dsh_instance_production::RegistryFingerprintAnchor,
    registry: Arc<InstanceRegistry>,
    ledger_tmp: PathBuf,
    observed: Mutex<Vec<String>>,
}

impl dsh_auth::RegistryFingerprintAnchor for BlockLedgerAfterRealAnchor {
    fn accepts(&self, instance_id: &str, fingerprint: &str) -> bool {
        dsh_auth::RegistryFingerprintAnchor::accepts(&self.real, instance_id, fingerprint)
    }

    fn anchor_fingerprint(&self, instance_id: &str, fingerprint: &str) -> Result<(), DshError> {
        dsh_auth::RegistryFingerprintAnchor::anchor_fingerprint(
            &self.real,
            instance_id,
            fingerprint,
        )
    }

    fn commit_fingerprint(
        &self,
        instance_id: &str,
        fingerprint: &str,
        mode: &dsh_auth::FingerprintCommitMode,
    ) -> Result<dsh_auth::FingerprintCommitReceipt, DshError> {
        let receipt = dsh_auth::RegistryFingerprintAnchor::commit_fingerprint(
            &self.real,
            instance_id,
            fingerprint,
            mode,
        )?;
        let persisted = self.registry.registered(instance_id)?.identity_fingerprint;
        assert_eq!(persisted, fingerprint, "真实登记必须先完成锚定");
        self.observed.lock().unwrap().push(persisted);
        std::fs::create_dir(&self.ledger_tmp).expect("锚定成功后阻塞账本临时文件");
        Ok(receipt)
    }

    fn rollback_fingerprint(
        &self,
        receipt: &dsh_auth::FingerprintCommitReceipt,
    ) -> Result<(), DshError> {
        dsh_auth::RegistryFingerprintAnchor::rollback_fingerprint(&self.real, receipt)
    }

    fn current_fingerprint(&self, instance_id: &str) -> Result<String, DshError> {
        dsh_auth::RegistryFingerprintAnchor::current_fingerprint(&self.real, instance_id)
    }
}

/// 预检后选中实例改变，普通批准不得提交旧实例。
#[tokio::test]
async fn approval_rejects_when_selected_instance_changes_before_real_cas() {
    let b = real_registry_auth_bench();
    let other_profile = b._root.path().join("other/profiles/desktop");
    std::fs::create_dir_all(&other_profile).unwrap();
    std::fs::write(other_profile.join("package.json"), b"{}").unwrap();
    let other = b
        .registry
        .register_candidate(HostKind::Desktop, &other_profile)
        .unwrap()
        .instance_id;
    assert_ne!(b.id, other);
    let anchor = Arc::new(CommitBoundaryAnchor {
        real: dsh_instance_production::RegistryFingerprintAnchor::with_registry(b.registry.clone()),
        registry: b.registry.clone(),
        switch_to: Some(other.clone()),
        close: None,
        reached: std::sync::atomic::AtomicBool::new(false),
    });
    let (svc, factory) = served_instance_with_anchor(&b.env, &b.id, HOST_PID, anchor.clone()).await;
    let client = real_registry_client(&factory, &b).await;
    let reply = client
        .connect(&svc, instance_epoch(&b.env, &b.id), [215; 32])
        .await
        .unwrap();
    let ServerFrame::ServerHello(hello) = reply else {
        panic!("应待批：{reply:?}")
    };
    assert_eq!(hello.state, "pending_approval");
    let binding = hello.binding_id.unwrap();
    let result = svc.approve(&binding).await;
    assert!(
        anchor.reached.load(std::sync::atomic::Ordering::SeqCst),
        "必须在真实提交前切换选择"
    );
    assert_eq!(b.registry.selected().unwrap().unwrap().instance_id, other);
    assert!(result.is_err(), "失去选择的实例不得批准成功");
    assert_eq!(
        b.registry.registered(&b.id).unwrap().identity_fingerprint,
        ""
    );
    assert_eq!(
        b.registry.registered(&other).unwrap().identity_fingerprint,
        ""
    );
    assert!(
        !instance_ledger_file(&b.env, &b.id).exists(),
        "拒绝不得产生绑定账本"
    );
    assert_eq!(svc.authorization_state(), AuthorizationState::Unbound);
    assert_eq!(svc.verified_peer_fingerprint().unwrap(), None);
    assert!(!svc.has_delivered_peer(&binding));
    assert!(svc.peer_for_control().await.is_none());
    assert!(client
        .drain()
        .await
        .iter()
        .all(|frame| !matches!(frame, ServerFrame::Granted { .. })));
    let delivery = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        client.send_credential(&binding, "new-cookie"),
    )
    .await
    .expect("交付应拒绝或关闭");
    assert!(!matches!(delivery, Ok(ServerFrame::Granted { .. })));
    assert!(!svc.has_delivered_peer(&binding));
}

/// 真实 CAS 成功后连接关闭，返回失败不能留下可重连的 Bound 账本。
#[tokio::test]
async fn approval_connection_closed_after_real_cas_does_not_leave_bound() {
    let b = real_registry_auth_bench();
    let (listener, factory) = mem_listener(PeerOsIdentity {
        pid: HOST_PID,
        uid: Some(TEST_UID),
    });
    let slot = Arc::new(Mutex::new(None));
    let anchor = Arc::new(CommitBoundaryAnchor {
        real: dsh_instance_production::RegistryFingerprintAnchor::with_registry(b.registry.clone()),
        registry: b.registry.clone(),
        switch_to: None,
        close: Some(slot.clone()),
        reached: std::sync::atomic::AtomicBool::new(false),
    });
    let svc = Arc::new(
        dsh_instance_production::init_instance_auth(&b.env, &b.id)
            .unwrap()
            .with_listener(Box::new(CapturingListener {
                inner: listener,
                connection: slot,
            }))
            .with_registry_anchor(anchor.clone()),
    );
    Arc::clone(&svc).serve().await.unwrap();
    let client = real_registry_client(&factory, &b).await;
    let reply = client
        .connect(&svc, instance_epoch(&b.env, &b.id), [216; 32])
        .await
        .unwrap();
    let ServerFrame::ServerHello(hello) = reply else {
        panic!("应待批：{reply:?}")
    };
    assert_eq!(hello.state, "pending_approval");
    let binding = hello.binding_id.unwrap();
    let result = svc.approve(&binding).await;
    assert!(
        anchor.reached.load(std::sync::atomic::Ordering::SeqCst),
        "必须在真实 CAS 成功后关闭连接"
    );
    assert!(!client.write_available(), "连接确实关闭");
    assert!(result.is_err(), "关闭后不得报告批准成功");
    assert_eq!(
        b.registry.registered(&b.id).unwrap().identity_fingerprint,
        "",
        "失败必须回滚新锚点"
    );
    let ledger = instance_ledger_file(&b.env, &b.id);
    if ledger.exists() {
        let value: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&ledger).unwrap()).unwrap();
        assert!(
            value["bindings"]
                .as_array()
                .unwrap()
                .iter()
                .all(|row| row["state"] != "bound"),
            "失败不得留下 Bound：{value}"
        );
    }
    assert_eq!(svc.authorization_state(), AuthorizationState::Unbound);
    assert_eq!(svc.verified_peer_fingerprint().unwrap(), None);
    assert!(!svc.has_delivered_peer(&binding));
    assert!(svc.peer_for_control().await.is_none());
    assert!(client
        .drain()
        .await
        .iter()
        .all(|frame| !matches!(frame, ServerFrame::Granted { .. })));
    assert!(client
        .send_credential(&binding, "new-cookie")
        .await
        .is_err());
}

/// 真实空登记先 CAS 成功，再让账本写失败；必须回滚空指纹且不发布。
#[tokio::test]
async fn real_registry_first_anchor_rolls_back_when_ledger_write_fails() {
    let b = real_registry_auth_bench();
    let ledger = instance_ledger_file(&b.env, &b.id);
    let anchor = Arc::new(BlockLedgerAfterRealAnchor {
        real: dsh_instance_production::RegistryFingerprintAnchor::with_registry(b.registry.clone()),
        registry: b.registry.clone(),
        ledger_tmp: ledger.with_extension("tmp"),
        observed: Mutex::new(Vec::new()),
    });
    let (svc, factory) = served_instance_with_anchor(&b.env, &b.id, HOST_PID, anchor.clone()).await;
    let client = real_registry_client(&factory, &b).await;
    let fingerprint = dsh_auth::key_digest(&client.keys.spki);
    assert_eq!(
        b.registry.registered(&b.id).unwrap().identity_fingerprint,
        ""
    );
    let reply = client
        .connect(&svc, instance_epoch(&b.env, &b.id), [211; 32])
        .await
        .unwrap();
    let ServerFrame::ServerHello(hello) = reply else {
        panic!("预期待批：{reply:?}")
    };
    assert_eq!(hello.state, "pending_approval");
    let binding = hello.binding_id.unwrap();
    let result = svc.approve(&binding).await;
    assert_eq!(
        *anchor.observed.lock().unwrap(),
        vec![fingerprint],
        "必须先真实锚定，再注入写失败"
    );
    assert!(result.is_err(), "账本写失败必须拒绝批准");
    assert_eq!(
        b.registry.registered(&b.id).unwrap().identity_fingerprint,
        "",
        "必须回滚真实登记文件"
    );
    assert!(!ledger.exists());
    assert_eq!(svc.authorization_state(), AuthorizationState::Unbound);
    assert_eq!(svc.verified_peer_fingerprint().unwrap(), None);
    assert!(!svc.has_delivered_peer(&binding));
    assert!(svc.peer_for_control().await.is_none());
    assert!(client
        .drain()
        .await
        .iter()
        .all(|f| !matches!(f, ServerFrame::Granted { .. })));
    let delivery = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        client.send_credential(&binding, "new-cookie"),
    )
    .await
    .expect("交付应拒绝或关闭");
    assert!(!matches!(delivery, Ok(ServerFrame::Granted { .. })));
    assert!(!svc.has_delivered_peer(&binding));
}

/// 同钥重试已有非空登记时写账本失败，不能把原指纹错误地清空。
#[tokio::test]
async fn real_registry_existing_fingerprint_survives_ledger_write_failure() {
    let b = real_registry_auth_bench();
    let ledger = instance_ledger_file(&b.env, &b.id);
    let anchor = Arc::new(BlockLedgerAfterRealAnchor {
        real: dsh_instance_production::RegistryFingerprintAnchor::with_registry(b.registry.clone()),
        registry: b.registry.clone(),
        ledger_tmp: ledger.with_extension("tmp"),
        observed: Mutex::new(Vec::new()),
    });
    let (svc, factory) = served_instance_with_anchor(&b.env, &b.id, HOST_PID, anchor.clone()).await;
    let client = real_registry_client(&factory, &b).await;
    let fingerprint = dsh_auth::key_digest(&client.keys.spki);
    b.registry
        .compare_and_set_fingerprint(&b.id, "", &fingerprint)
        .unwrap();
    let reply = client
        .connect(&svc, instance_epoch(&b.env, &b.id), [212; 32])
        .await
        .unwrap();
    let ServerFrame::ServerHello(hello) = reply else {
        panic!("预期待批：{reply:?}")
    };
    assert_eq!(hello.state, "pending_approval");
    let binding = hello.binding_id.unwrap();
    let result = svc.approve(&binding).await;
    assert_eq!(
        *anchor.observed.lock().unwrap(),
        vec![fingerprint.clone()],
        "必须先通过真实锚定"
    );
    assert!(result.is_err(), "账本写失败必须拒绝批准");
    assert_eq!(
        b.registry.registered(&b.id).unwrap().identity_fingerprint,
        fingerprint
    );
    assert!(!ledger.exists());
    assert_eq!(svc.authorization_state(), AuthorizationState::Unbound);
    assert_eq!(svc.verified_peer_fingerprint().unwrap(), None);
    assert!(!svc.has_delivered_peer(&binding));
    assert!(svc.peer_for_control().await.is_none());
    assert!(client
        .drain()
        .await
        .iter()
        .all(|f| !matches!(f, ServerFrame::Granted { .. })));
    let delivery = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        client.send_credential(&binding, "new-cookie"),
    )
    .await
    .expect("交付应拒绝或关闭");
    assert!(!matches!(delivery, Ok(ServerFrame::Granted { .. })));
    assert!(!svc.has_delivered_peer(&binding));
}

/// 真实预检完成后等待另一请求；所有登记操作仍由生产锚点执行。
struct FirstApprovalPreflightBarrier {
    real: dsh_instance_production::RegistryFingerprintAnchor,
    observed: Mutex<Vec<(String, bool)>>,
    ready: std::sync::Condvar,
}

impl dsh_auth::RegistryFingerprintAnchor for FirstApprovalPreflightBarrier {
    fn accepts(&self, instance_id: &str, fingerprint: &str) -> bool {
        let accepted =
            dsh_auth::RegistryFingerprintAnchor::accepts(&self.real, instance_id, fingerprint);
        let mut observed = self.observed.lock().unwrap();
        observed.push((fingerprint.to_owned(), accepted));
        self.ready.notify_all();
        let (observed, timeout) = self
            .ready
            .wait_timeout_while(observed, std::time::Duration::from_secs(10), |entries| {
                entries.len() < 2
            })
            .unwrap();
        assert!(
            !timeout.timed_out() || observed.len() >= 2,
            "两个批准请求未到达预检屏障"
        );
        accepted
    }

    fn anchor_fingerprint(&self, instance_id: &str, fingerprint: &str) -> Result<(), DshError> {
        dsh_auth::RegistryFingerprintAnchor::anchor_fingerprint(
            &self.real,
            instance_id,
            fingerprint,
        )
    }

    fn commit_fingerprint(
        &self,
        instance_id: &str,
        fingerprint: &str,
        mode: &dsh_auth::FingerprintCommitMode,
    ) -> Result<dsh_auth::FingerprintCommitReceipt, DshError> {
        dsh_auth::RegistryFingerprintAnchor::commit_fingerprint(
            &self.real,
            instance_id,
            fingerprint,
            mode,
        )
    }

    fn rollback_fingerprint(
        &self,
        receipt: &dsh_auth::FingerprintCommitReceipt,
    ) -> Result<(), DshError> {
        dsh_auth::RegistryFingerprintAnchor::rollback_fingerprint(&self.real, receipt)
    }

    fn current_fingerprint(&self, instance_id: &str) -> Result<String, DshError> {
        dsh_auth::RegistryFingerprintAnchor::current_fingerprint(&self.real, instance_id)
    }
}

/// 两个不同钥的真实预检均通过后再提交，登记、账本和凭据只能属于同一个胜者。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_first_approvals_commit_only_one_real_registry_binding() {
    if run_instance_credential_fixture_in_child(
        "concurrent_first_approvals_commit_only_one_real_registry_binding",
    ) {
        return;
    }
    let b = real_registry_auth_bench();
    let anchor = Arc::new(FirstApprovalPreflightBarrier {
        real: dsh_instance_production::RegistryFingerprintAnchor::with_registry(b.registry.clone()),
        observed: Mutex::new(Vec::new()),
        ready: std::sync::Condvar::new(),
    });
    let (svc, factory) = served_instance_with_anchor(&b.env, &b.id, HOST_PID, anchor.clone()).await;
    let epoch = instance_epoch(&b.env, &b.id);
    let a = real_registry_client(&factory, &b).await;
    let c = real_registry_client(&factory, &b).await;
    let ServerFrame::ServerHello(hello_a) = a.connect(&svc, epoch, [213; 32]).await.unwrap() else {
        panic!("A 应待批")
    };
    let ServerFrame::ServerHello(hello_c) = c.connect(&svc, epoch, [214; 32]).await.unwrap() else {
        panic!("B 应待批")
    };
    assert_eq!(hello_a.state, "pending_approval");
    assert_eq!(hello_c.state, "pending_approval");
    let binding_a = hello_a.binding_id.unwrap();
    let binding_c = hello_c.binding_id.unwrap();
    let fingerprint_a = dsh_auth::key_digest(&a.keys.spki);
    let fingerprint_c = dsh_auth::key_digest(&c.keys.spki);
    assert_ne!(fingerprint_a, fingerprint_c, "必须使用两把不同的钥");
    let svc_a = Arc::clone(&svc);
    let svc_c = Arc::clone(&svc);
    let id_a = binding_a.clone();
    let id_c = binding_c.clone();
    let task_a = tokio::spawn(async move { svc_a.approve(&id_a).await });
    let task_c = tokio::spawn(async move { svc_c.approve(&id_c).await });
    let (result_a, result_c) = tokio::join!(task_a, task_c);
    let result_a = result_a.expect("A 批准任务不得 panic");
    let result_c = result_c.expect("B 批准任务不得 panic");
    {
        let observed = anchor.observed.lock().unwrap();
        assert_eq!(observed.len(), 2, "必须恰有两次预检");
        assert!(
            observed.contains(&(fingerprint_a, true)),
            "A 的真实预检必须通过"
        );
        assert!(
            observed.contains(&(fingerprint_c, true)),
            "B 的真实预检必须通过"
        );
    }
    assert_ne!(result_a.is_ok(), result_c.is_ok(), "两次批准只能成功一次");
    let (winner, winner_id, loser, loser_id) = if result_a.is_ok() {
        (&a, &binding_a, &c, &binding_c)
    } else {
        (&c, &binding_c, &a, &binding_a)
    };
    assert_eq!(
        b.registry.registered(&b.id).unwrap().identity_fingerprint,
        dsh_auth::key_digest(&winner.keys.spki)
    );
    let ledger: serde_json::Value =
        serde_json::from_slice(&std::fs::read(instance_ledger_file(&b.env, &b.id)).unwrap())
            .unwrap();
    let rows = ledger["bindings"].as_array().expect("账本行");
    assert_eq!(rows.len(), 1, "只允许一条绑定账本行");
    assert_eq!(rows[0]["bindingId"].as_str(), Some(winner_id.as_str()));
    assert_eq!(rows[0]["state"].as_str(), Some("bound"));
    let _ = winner.drain().await;
    let ack = winner
        .send_credential(winner_id, "winner-cookie")
        .await
        .unwrap();
    assert!(matches!(ack, ServerFrame::Granted { .. }));
    assert!(svc.has_delivered_peer(winner_id));
    let _ = loser.drain().await;
    let delivery = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        loser.send_credential(loser_id, "loser-cookie"),
    )
    .await
    .expect("败者交付应拒绝或关闭");
    assert!(!matches!(delivery, Ok(ServerFrame::Granted { .. })));
    assert!(!svc.has_delivered_peer(loser_id));
    assert_eq!(
        svc.peer_for_control().await.unwrap().binding_id(),
        winner_id.as_str()
    );
}

/// 并发首绑：两把不同的钥同时提交，只能有一个成功（CAS 保证）。
///
/// 为什么必须在**真实 Registry** 上测：这是文件锁 + 读改写的行为，替身证明了
/// 不锁的问题。用同一实例 ID、期望值都为空（首绑），两个不同新钥并发提交。
#[test]
fn concurrent_first_anchors_admit_exactly_one_key() {
    use multi_agents_manager_lib::services::dsh_control::instances::{
        instance_id_for, HostKind, InstanceRegistry,
    };
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home-a");
    let profile = home.join("profiles/desktop");
    std::fs::create_dir_all(&profile).unwrap();
    std::fs::write(profile.join("package.json"), b"{}").unwrap();
    let profile = std::fs::canonicalize(profile).unwrap();
    let registry_path = root.path().join("instances.json");
    let instance_id = instance_id_for(HostKind::Desktop, &profile);
    {
        let registry = InstanceRegistry::open(&registry_path).unwrap();
        registry
            .register_candidate(HostKind::Desktop, &profile)
            .expect("候选登记");
    }

    // 两把不同的钥（空期望值 = 首绑）并发提交。
    let mut handles = Vec::new();
    for digest in ["key-alpha", "key-beta"] {
        let path = registry_path.clone();
        let id = instance_id.clone();
        handles.push(std::thread::spawn(move || {
            let registry = InstanceRegistry::open(&path).unwrap();
            registry
                .compare_and_set_fingerprint(&id, "", digest)
                .is_ok()
        }));
    }
    let results: Vec<bool> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    assert_eq!(
        results.iter().filter(|ok| **ok).count(),
        1,
        "并发首绑只能有一个成功，实际 {results:?}"
    );

    // 落盘值必须是那个成功者的钥（不是两次写混在一起）。
    let registry = InstanceRegistry::open(&registry_path).unwrap();
    let committed = registry
        .registered(&instance_id)
        .unwrap()
        .identity_fingerprint;
    assert!(
        committed == "key-alpha" || committed == "key-beta",
        "落盘值必须是单一合法钥：{committed}"
    );
}

/// CAS 的三种结果：空值写入 / 同值幂等 / 期望不符拒绝（且不改盘）。
#[test]
fn fingerprint_cas_refuses_a_stale_expected_value_without_writing() {
    use multi_agents_manager_lib::services::dsh_control::instances::{
        instance_id_for, HostKind, InstanceRegistry,
    };
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home-a");
    let profile = home.join("profiles/desktop");
    std::fs::create_dir_all(&profile).unwrap();
    std::fs::write(profile.join("package.json"), b"{}").unwrap();
    let profile = std::fs::canonicalize(profile).unwrap();
    let path = root.path().join("instances.json");
    let id = instance_id_for(HostKind::Desktop, &profile);
    let registry = InstanceRegistry::open(&path).unwrap();
    registry
        .register_candidate(HostKind::Desktop, &profile)
        .expect("候选登记");

    // ① 空值 → 写入
    registry
        .compare_and_set_fingerprint(&id, "", "key-1")
        .expect("空登记值可提交");
    // ② 同值 → 幂等
    registry
        .compare_and_set_fingerprint(&id, "key-1", "key-1")
        .expect("同值幂等");
    // ③ 期望不符 → 拒绝，且**不改盘**
    let err = registry
        .compare_and_set_fingerprint(&id, "", "key-2")
        .expect_err("期望值过期必须拒绝（否则就是静默覆盖别人的提交）");
    assert_eq!(err.code, codes::AUTH_SIGNATURE, "{err}");
    assert_eq!(
        registry.registered(&id).unwrap().identity_fingerprint,
        "key-1",
        "拒绝时不得改动已提交值"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn linked_profile_handshake_accepts_proven_home_and_rejects_physical_target_home() {
    use std::os::unix::fs::symlink;
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("proven-home");
    let foreign = root.path().join("foreign-home");
    let physical = foreign.join("profiles/desktop");
    std::fs::create_dir_all(&physical).unwrap();
    std::fs::write(physical.join("package.json"), b"{}").unwrap();
    std::fs::create_dir_all(home.join("profiles")).unwrap();
    let profile = home.join("profiles/desktop");
    symlink(&physical, &profile).unwrap();
    let pid = 47091;
    let started = 1_760_020_000_000;
    let env = DshEnvironment {
        home: home.clone(),
        tuvis_home: root.path().join("tuvis"),
        dsh_state_dir: None,
        processes: Arc::new(SharedProcs {
            processes: Mutex::new(vec![real_desktop_process(pid, &profile, started)]),
            listeners: Mutex::new(vec![DshListener {
                owner_pid: pid,
                authority: "127.0.0.1:53091".into(),
            }]),
        }),
        files: Arc::new(RealFsFiles),
        runner: Arc::new(NullRunner),
        clock: Arc::new(VarClock(Mutex::new(started))),
        environments: Arc::new(
            multi_agents_manager_lib::services::dsh_control::model::TestUnobservableEnvironments,
        ),
    };
    let registry = InstanceRegistry::open(root.path().join("instances.json")).unwrap();
    let instance = registry
        .register_candidate_with_home(HostKind::Desktop, &profile, Some(&home))
        .unwrap();
    let (svc, factory) = served_instance(&env, &instance.instance_id, pid).await;
    let epoch = instance_epoch(&env, &instance.instance_id);
    let mut wrong = InstanceClient::new(&factory, pid, started, instance.instance_id.clone()).await;
    wrong.executable = "/Applications/DeepSeek Harness.app/Contents/MacOS/DeepSeek Harness".into();
    wrong.home_root = foreign.to_string_lossy().into_owned();
    let error = wrong
        .connect(&svc, epoch, [91; 32])
        .await
        .expect_err("物理外链目标 home 不可冒充已证明 home");
    assert_eq!(error.code, codes::INSTANCE_UNVERIFIED);
    let mut correct =
        InstanceClient::new(&factory, pid, started, instance.instance_id.clone()).await;
    correct.executable = wrong.executable.clone();
    correct.home_root = home.to_string_lossy().into_owned();
    let reply = correct
        .connect(&svc, epoch, [92; 32])
        .await
        .expect("已证明 home 的握手应通过 home 对位");
    assert!(matches!(reply, ServerFrame::ServerHello(hello) if hello.state == "pending_approval"));
}

// ============================================================
// P1-3（Round-7 Task 3）：接受循环 spawn 与发布解耦
//
// 根因回归：serve 首次调用在 bind 之后、spawn 接受循环之前发布失败（非
// HOST_NOT_RUNNING，如实上抛）→ 监听端留槽但无人 accept；重试 serve 永远走
// 幂等分支（只补活体跟踪、不补接受循环）——端口晚于进程就绪（DSH 进程已在、
// 回环监听未就绪）时，兔维斯重启后重控通道永久失联，直到重启兔维斯。
//
// 断言面：宿主进程在场 + 回环监听缺席 → 首次 serve 发布失败上抛；监听就绪后
// 重试 serve → 幂等成功，且 (a) 连接有应答（接受循环在场）、(b) 记录已发布 +
// 完整握手（不重启兔维斯）。
// ============================================================

/// (a) 首次 serve 发布失败 → 重试 serve → 接受循环在场：测试客户端连接并完成
/// 一次完整握手（当前代码留下「已 bind 无人 accept」，连接永远无应答）。
#[tokio::test]
async fn retry_after_failed_first_publish_still_accepts_connections() {
    let b = bench();
    // 阶段一：宿主进程在场但回环监听缺席（端口晚于进程）→ 发布失败上抛。
    b.procs.listeners.lock().unwrap().clear();
    let (listener, factory) = mem_listener(PeerOsIdentity {
        pid: HOST_PID,
        uid: Some(TEST_UID),
    });
    let svc = Arc::new(
        dsh_auth::DshAuthService::new(clone_env(&b))
            .expect("auth 服务可建")
            .without_liveness_watcher() // 隔离接受循环面（活体跟踪不参与本用例）
            .with_listener(Box::new(listener)),
    );
    let first = Arc::clone(&svc)
        .serve()
        .await
        .expect_err("发布失败必须上抛");
    assert_ne!(
        first.code,
        codes::HOST_NOT_RUNNING,
        "必须是发布失败（非宿主缺席的推迟面）：{first}"
    );

    // 阶段二：回环监听就绪，重试 serve → 幂等分支必须补齐接受循环。
    b.procs.listeners.lock().unwrap().push(DshListener {
        owner_pid: HOST_PID,
        authority: "127.0.0.1:5214".into(),
    });
    Arc::clone(&svc)
        .serve()
        .await
        .expect("重试 serve 必须幂等成功并补齐接受循环");

    // 接受循环在场：连接 + hello 得到真实应答（10s 兜底超时，避免死挂）。
    let client = TestClient {
        keys: RustSigningKey::generate(),
        conn: factory.connect().await,
        received: Mutex::new(Vec::new()),
    };
    let epoch = current_epoch(&b.env);
    let reply = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        client.connect_frame(&svc, epoch, [11u8; 32]).await
    })
    .await
    .expect("接受循环必须在场（连接必须有应答——当前代码此处死挂）")
    .expect("握手必须完成");
    let ServerFrame::ServerHello(hello) = reply else {
        panic!("新钥 hello 应得 serverHello，得到 {reply:?}")
    };
    assert_eq!(hello.state, "pending_approval", "逐连接核验照常（待批面）");
}

/// (b) 端口晚于进程就绪：首次 serve 发布失败（INSTANCE_UNVERIFIED 形态）→
/// 宿主变为可核验 → 重试 serve（幂等分支 + 活体跟踪补发布）→ 记录已发布 且
/// 连接可达（完整握手，全程不重启兔维斯）。
#[cfg(unix)]
#[tokio::test]
async fn port_later_than_process_recovers_publish_and_accept_without_restart() {
    let b = bench();
    // 阶段一：宿主在场 + 回环监听缺席 → desktop 核验失败（监听缺失，非
    // HOST_NOT_RUNNING）→ 发布失败上抛。
    b.procs.listeners.lock().unwrap().clear();
    let (listener, factory) = mem_listener(PeerOsIdentity {
        pid: HOST_PID,
        uid: Some(TEST_UID),
    });
    let svc = Arc::new(
        dsh_auth::DshAuthService::new(clone_env(&b))
            .expect("auth 服务可建")
            .with_listener(Box::new(listener)),
    );
    let first = Arc::clone(&svc)
        .serve()
        .await
        .expect_err("发布失败必须上抛");
    assert_ne!(first.code, codes::HOST_NOT_RUNNING, "{first}");

    // 阶段二：回环监听就绪（宿主变为可核验）→ 重试 serve 幂等成功。
    b.procs.listeners.lock().unwrap().push(DshListener {
        owner_pid: HOST_PID,
        authority: "127.0.0.1:5214".into(),
    });
    Arc::clone(&svc).serve().await.expect("重试 serve 幂等成功");

    // 记录必须已发布（活体跟踪拍补发布；不重启兔维斯）。
    let rendezvous = b.env.tuvis_home.join("dsh").join("rendezvous.json");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    let record = loop {
        if let Ok(bytes) = std::fs::read(&rendezvous) {
            if let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes) {
                if value["retired"] == false
                    && value["address"].as_str().is_some_and(|a| !a.is_empty())
                {
                    break value;
                }
            }
        }
        assert!(
            std::time::Instant::now() < deadline,
            "宿主可核验后记录必须由活体跟踪补发布"
        );
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    };
    assert_eq!(record["hostId"], fixture_host_id(), "记录必须指向当代宿主");

    // 连接可达：完整握手（逐连接核验照常）。
    let client = TestClient {
        keys: RustSigningKey::generate(),
        conn: factory.connect().await,
        received: Mutex::new(Vec::new()),
    };
    let epoch = current_epoch(&b.env);
    let reply = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        client.connect_frame(&svc, epoch, [12u8; 32]).await
    })
    .await
    .expect("接受循环必须在场（连接必须有应答）")
    .expect("握手必须完成");
    let ServerFrame::ServerHello(hello) = reply else {
        panic!("新钥 hello 应得 serverHello，得到 {reply:?}")
    };
    assert_eq!(hello.state, "pending_approval");
}
