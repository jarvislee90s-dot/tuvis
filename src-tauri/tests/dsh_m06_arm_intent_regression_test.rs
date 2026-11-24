//! M06 一键布防：生产命令核心与真实 v2 授权握手的回归。
//! 用户禁止运行 Cargo；本文件尚未编译或运行，预期缺插件用例暴露当前命令缺口。
//! 客户端独立编码签名；进程、文件、时钟和 IPC 为隔离边界，授权服务为真实实现。
//! 预期命令核心接收 env、registry、显式请求和 InstanceServices；本文件先于实现。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use ed25519_dalek::{Signer, SigningKey};
use serde::Serialize;
use serde_json::{json, Value};

use multi_agents_manager_lib::commands::dsh::{dsh_binding_arm_intent_core, DshInstanceRequest};
use multi_agents_manager_lib::services::dsh_control::auth::{self, DshAuthService};
use multi_agents_manager_lib::services::dsh_control::error::DshError;
use multi_agents_manager_lib::services::dsh_control::identity;
use multi_agents_manager_lib::services::dsh_control::install::{
    embedded_manifest, BindingRevoker, DshPluginManager, EMBEDDED_PACKAGE_BYTES, PACKAGE_NAME,
};
use multi_agents_manager_lib::services::dsh_control::instances::{
    instance_id_for, HostKind, InstanceRegistry,
};
use multi_agents_manager_lib::services::dsh_control::local_ipc::mem::{
    mem_listener, MemClient, MemClientFactory,
};
use multi_agents_manager_lib::services::dsh_control::local_ipc::PeerOsIdentity;
use multi_agents_manager_lib::services::dsh_control::model::{
    ActivationState, CommandRequest, DshClock, DshCommandRunner, DshEnvironment, DshFileSource,
    DshListener, DshProcess, DshProcessSource, InstallationState, LocalHost, RunOutcome,
};
use multi_agents_manager_lib::services::dsh_control::production;

const NOW: i64 = 1_760_000_000_000;
const PID: u32 = 4201;
const STARTED: i64 = NOW - 10_000;
const WAIT: Duration = Duration::from_secs(3);
// 钩子是测试二进制内的 OnceLock；所有测试串行，防止其他握手误占屏障。
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

struct Clock(AtomicI64);
impl DshClock for Clock {
    fn now_ms(&self) -> i64 {
        self.0.load(Ordering::SeqCst)
    }
}

struct NoRevoke;
impl BindingRevoker for NoRevoke {
    fn revoke_profile(&self, _profile_id: &str) -> Result<(), DshError> {
        panic!("查询安装状态不应撤权")
    }
}

// 使用仓库内置发行包的真实内容，而不是伪造 installed 状态或空包目录。
fn install_embedded_plugin(profile: &Path, root: &Path) {
    let archive = root.join("remote-access.tgz");
    std::fs::write(&archive, EMBEDDED_PACKAGE_BYTES).unwrap();
    let package_dir = profile.join("node_modules/@tuvis/dsh-remote-access");
    std::fs::create_dir_all(&package_dir).unwrap();
    let output = std::process::Command::new("tar")
        .args(["-xzf"])
        .arg(&archive)
        .arg("--strip-components=1")
        .arg("-C")
        .arg(&package_dir)
        .output()
        .expect("测试需要 tar 来解开项目内置插件包");
    assert!(
        output.status.success(),
        "解包失败: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let package: Value =
        serde_json::from_slice(&std::fs::read(package_dir.join("package.json")).unwrap()).unwrap();
    assert_eq!(package["name"], PACKAGE_NAME);
    assert_eq!(
        package["version"],
        embedded_manifest().unwrap().plugin_version
    );
    assert!(package_dir.join("lib/index.js").is_file());
    std::fs::write(
        profile.join("package.json"),
        serde_json::to_vec(&json!({
            "name": "dsh-profile-desktop",
            "dependencies": { (PACKAGE_NAME): format!("file:{}", archive.display()) },
            "dsh": { "profile": { "bundles": [PACKAGE_NAME] } }
        }))
        .unwrap(),
    )
    .unwrap();
}

struct Processes {
    process: DshProcess,
    running: AtomicBool,
}
impl DshProcessSource for Processes {
    fn list_processes(&self) -> Vec<DshProcess> {
        if self.running.load(Ordering::SeqCst) {
            vec![self.process.clone()]
        } else {
            vec![]
        }
    }
    fn listening_sockets(&self) -> Vec<DshListener> {
        if !self.running.load(Ordering::SeqCst) {
            return vec![];
        }
        vec![DshListener {
            owner_pid: PID,
            authority: "127.0.0.1:24001".into(),
        }]
    }
}

// 只拦截一次；系统线程负责释放，确保单线程 runtime 被阻塞时测试也能结束。
struct SlowGate {
    entered: Mutex<Option<mpsc::Sender<()>>>,
    release: Mutex<mpsc::Receiver<()>>,
}

impl SlowGate {
    fn wait_once(&self) {
        let entered = self.entered.lock().unwrap().take();
        if let Some(entered) = entered {
            let _ = entered.send(());
            let _ = self.release.lock().unwrap().recv_timeout(WAIT * 2);
        }
    }
}

#[derive(Clone, Copy)]
enum SlowPoint {
    Processes,
    Listeners,
    ProfileRead,
}

struct SlowProcesses {
    inner: Arc<Processes>,
    gate: Arc<SlowGate>,
    point: SlowPoint,
}

impl DshProcessSource for SlowProcesses {
    fn list_processes(&self) -> Vec<DshProcess> {
        if matches!(self.point, SlowPoint::Processes) {
            self.gate.wait_once();
        }
        self.inner.list_processes()
    }

    fn listening_sockets(&self) -> Vec<DshListener> {
        if matches!(self.point, SlowPoint::Listeners) {
            self.gate.wait_once();
        }
        self.inner.listening_sockets()
    }
}

struct SlowFiles {
    inner: Arc<dyn DshFileSource>,
    gate: Arc<SlowGate>,
    profile_manifest: PathBuf,
    point: SlowPoint,
}

impl DshFileSource for SlowFiles {
    fn exists(&self, path: &Path) -> bool {
        self.inner.exists(path)
    }
    fn is_dir(&self, path: &Path) -> bool {
        self.inner.is_dir(path)
    }
    fn list_dir(&self, path: &Path) -> Vec<PathBuf> {
        self.inner.list_dir(path)
    }
    fn read_small(&self, path: &Path, max_bytes: u64) -> Result<Vec<u8>, DshError> {
        if matches!(self.point, SlowPoint::ProfileRead) && path == self.profile_manifest {
            self.gate.wait_once();
        }
        self.inner.read_small(path, max_bytes)
    }
    fn read_link_target(&self, path: &Path) -> Option<PathBuf> {
        self.inner.read_link_target(path)
    }
    fn is_readable_regular_file(&self, path: &Path) -> bool {
        self.inner.is_readable_regular_file(path)
    }
    fn resolve_actual_path(&self, path: &Path) -> Option<PathBuf> {
        self.inner.resolve_actual_path(path)
    }
}

struct NoCommands;
impl DshCommandRunner for NoCommands {
    fn run(&self, _request: &CommandRequest) -> Result<RunOutcome, DshError> {
        panic!("授权行为测试不得执行宿主命令")
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct WireHome {
    home_root: String,
    host_pid: u32,
    host_started_at_ms: i64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct WireChallenge {
    binding_protocol: u32,
    key_digests: [String; 2],
    nonce: String,
    host_id: String,
    epoch: String,
    expiry: i64,
    home_binding: WireHome,
}

struct ClientKey(SigningKey);
impl ClientKey {
    fn new(seed: u8) -> Self {
        Self(SigningKey::from_bytes(&[seed; 32]))
    }
    fn spki(&self) -> Vec<u8> {
        // 标准 Ed25519 SPKI，独立于生产包皮函数。
        let mut bytes = vec![
            0x30, 0x2a, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x03, 0x21, 0x00,
        ];
        bytes.extend_from_slice(&self.0.verifying_key().to_bytes());
        bytes
    }
}

#[path = "support/dsh_digest.rs"]
mod dsh_digest;
use dsh_digest::digest;

struct Bench {
    env: DshEnvironment,
    host: LocalHost,
    svc: Arc<DshAuthService>,
    manager: Arc<DshPluginManager>,
    registry: Arc<InstanceRegistry>,
    processes: Arc<Processes>,
    factory: Arc<MemClientFactory>,
    #[cfg(feature = "dsh-test-probes")]
    clock: Arc<Clock>,
    // 最后释放临时目录；每例独立 Tokio runtime 退出时回收服务后台任务。
    _tmp: tempfile::TempDir,
}

impl Bench {
    async fn start() -> Self {
        Self::start_with_plugin(false).await
    }

    async fn start_with_plugin(installed: bool) -> Self {
        Self::start_with_anchor(installed, |registry| {
            Arc::new(production::RegistryFingerprintAnchor::with_registry(
                registry,
            ))
        })
        .await
    }

    async fn start_with_anchor(
        installed: bool,
        anchor: impl FnOnce(Arc<InstanceRegistry>) -> Arc<dyn auth::RegistryFingerprintAnchor>,
    ) -> Self {
        let tmp = tempfile::tempdir().expect("独立临时根");
        let home = tmp.path().join("fixture-home/.dsh");
        let profile = home.join("profiles/desktop");
        std::fs::create_dir_all(&profile).unwrap();
        std::fs::write(profile.join("package.json"), b"{}").unwrap();
        if installed {
            install_embedded_plugin(&profile, tmp.path());
        }
        let exe = tmp
            .path()
            .join("DeepSeek Harness.app/Contents/MacOS/DeepSeek Harness");
        let process = DshProcess {
            pid: PID,
            exe: exe.clone(),
            started_at_ms: STARTED,
            cmdline: vec![
                exe.to_string_lossy().into_owned(),
                tmp.path()
                    .join("node_modules/@deepseek-ai/dsh-desktop-host/lib/index.js")
                    .to_string_lossy()
                    .into_owned(),
                profile.to_string_lossy().into_owned(),
            ],
        };
        let clock = Arc::new(Clock(AtomicI64::new(NOW)));
        let processes = Arc::new(Processes {
            process,
            running: AtomicBool::new(true),
        });
        let env = DshEnvironment {
            home,
            tuvis_home: tmp.path().join("tuvis"),
            dsh_state_dir: Some(tmp.path().join("state")),
            processes: processes.clone(),
            files: Arc::new(production::FsFileSource),
            runner: Arc::new(NoCommands),
            clock: clock.clone(),
            // 测试默认「不可观测」：显式拒绝 Web home 归属（fail-closed），
            // 绝不让假进程源落到真实系统 PID 上。需要 Web 归属的用例自行注入假观测器。
            environments: std::sync::Arc::new(multi_agents_manager_lib::services::dsh_control::model::TestUnobservableEnvironments),
        };
        let id = instance_id_for(HostKind::Desktop, &profile);
        let host = identity::discover_instance(&env, &id).expect("argv/profile/监听证据可发现宿主");
        let registry = Arc::new(InstanceRegistry::open(tmp.path().join("instances.json")).unwrap());
        let instance = registry
            .register_candidate(HostKind::Desktop, &profile)
            .unwrap();
        assert_eq!(instance.instance_id, id);
        registry.select(&id).unwrap();
        let manager = Arc::new(production::init_instance_manager(
            &env,
            &instance,
            Arc::new(NoRevoke),
        ));
        if installed {
            let status = manager.status().expect("真实插件状态可读");
            assert_eq!(status.installation, InstallationState::Installed);
            assert_eq!(status.activation, ActivationState::Running);
            assert_eq!(
                status.plugin_version.as_deref(),
                Some(embedded_manifest().unwrap().plugin_version.as_str())
            );
        }
        let (listener, factory) = mem_listener(PeerOsIdentity {
            pid: PID,
            uid: None,
        });
        let svc = Arc::new(
            production::init_instance_auth(&env, &id)
                .unwrap()
                .with_registry_anchor(anchor(registry.clone()))
                .with_listener(Box::new(listener)),
        );
        svc.serve().await.expect("真实授权服务循环启动");
        Self {
            env,
            host,
            svc,
            manager,
            registry,
            processes,
            factory,
            #[cfg(feature = "dsh-test-probes")]
            clock,
            _tmp: tmp,
        }
    }

    // 另一个实际 registry 对象指向同一文件，覆盖跨对象的文件锁与持久 revision。
    fn reopen_registry(&self) -> InstanceRegistry {
        InstanceRegistry::open(self._tmp.path().join("instances.json")).unwrap()
    }

    fn register_other(&self, other: &Bench) {
        self.registry
            .register_candidate(HostKind::Desktop, &other.host.profile_dir)
            .unwrap();
    }

    fn assert_unbound(&self) {
        assert!(self.svc.active_binding_id().is_none(), "不能产生受信绑定");
        assert!(
            self.registry
                .registered(&self.host.host_id)
                .unwrap()
                .identity_fingerprint
                .is_empty(),
            "不能提交实例指纹"
        );
        if let Ok(bytes) = std::fs::read(self.state_dir().join("bindings.json")) {
            let ledger: Value = serde_json::from_slice(&bytes).unwrap();
            assert!(!ledger["bindings"]
                .as_array()
                .unwrap()
                .iter()
                .any(|row| row["state"] == "bound"));
        }
    }

    fn request(&self) -> DshInstanceRequest {
        DshInstanceRequest {
            instance_id: self.host.host_id.clone(),
        }
    }

    async fn arm(&self) -> Result<(), String> {
        let instance_id = self.host.host_id.clone();
        let manager = self.manager.clone();
        let auth = self.svc.clone();
        let services = move |id: &str| -> Result<_, DshError> {
            assert_eq!(id, instance_id);
            Ok((manager.clone(), auth.clone()))
        };
        dsh_binding_arm_intent_core(&self.env, &self.registry, self.request(), &services).await
    }

    async fn assert_next_requires_approval(&self, seed: u8) {
        let conn = self.connect().await;
        let reply = exchange(&conn, &self.hello(&ClientKey::new(seed), 1)).await;
        assert_state(&reply, "pending_approval");
    }

    async fn assert_next_not_authorized(&self, seed: u8) {
        let conn = self.connect().await;
        let reply = exchange(&conn, &self.hello(&ClientKey::new(seed), 1)).await;
        assert!(
            reply["type"] == "error"
                || (reply["type"] == "server_hello" && reply["state"] == "pending_approval"),
            "失效意图不得授权: {reply}"
        );
        self.assert_unbound();
    }

    fn state_dir(&self) -> PathBuf {
        auth::instance_state_dir(&self.env, &self.host.host_id).unwrap()
    }

    #[cfg(feature = "dsh-test-probes")]
    fn ledger(&self) -> Vec<u8> {
        std::fs::read(self.state_dir().join("bindings.json")).unwrap()
    }

    fn hello(&self, keys: &ClientKey, counter: u64) -> Value {
        let mut nonce = [0u8; 32];
        nonce[..8].copy_from_slice(&counter.to_be_bytes());
        let msg = WireChallenge {
            binding_protocol: 2,
            key_digests: [digest(self.svc.public_key_spki()), digest(&keys.spki())],
            nonce: URL_SAFE_NO_PAD.encode(nonce),
            host_id: self.host.host_id.clone(),
            epoch: self.host.epoch.to_string(),
            // 帧的有效期固定，使用注入时钟而不依赖墙钟。
            expiry: NOW + 60_000,
            home_binding: WireHome {
                home_root: self.env.home.to_string_lossy().into_owned(),
                host_pid: PID,
                host_started_at_ms: STARTED,
            },
        };
        json!({
            "type": "hello", "bindingProtocol": 2,
            "publicKeySpki": URL_SAFE_NO_PAD.encode(keys.spki()),
            "executable": self.host.executable.to_string_lossy(),
            "pid": PID, "startedAtMs": STARTED,
            "hostId": msg.host_id, "epoch": msg.epoch,
            "nonce": msg.nonce, "expiry": msg.expiry,
            "homeBinding": msg.home_binding,
            "signature": URL_SAFE_NO_PAD.encode(keys.0.sign(&serde_json::to_vec(&msg).unwrap()).to_bytes()),
        })
    }

    async fn connect(&self) -> MemClient {
        tokio::time::timeout(WAIT, self.factory.connect())
            .await
            .expect("连接入队有界")
    }
}

async fn receive(conn: &MemClient) -> Value {
    let line = tokio::time::timeout(WAIT, conn.recv())
        .await
        .expect("服务端应答不得悬挂")
        .expect("服务端连接保持可读");
    serde_json::from_str(&line).expect("真实服务端返回 JSON 帧")
}

async fn exchange(conn: &MemClient, wire: &Value) -> Value {
    tokio::time::timeout(WAIT, conn.send(&wire.to_string()))
        .await
        .expect("发送有界")
        .expect("真实 IPC 传输接受帧");
    receive(conn).await
}

fn assert_state(reply: &Value, state: &str) -> String {
    assert_eq!(reply["type"], "server_hello", "有效握手可达：{reply}");
    assert_eq!(reply["state"], state, "授权决策：{reply}");
    reply["bindingId"].as_str().expect("决策附带绑定号").into()
}

#[tokio::test]
async fn arm_command_rejects_missing_plugin_without_granting_the_next_handshake() {
    let _serial = SERIAL.lock().await;
    let b = Bench::start().await;
    // 空 profile 不包含插件或安装产物。
    let error = b.arm().await.expect_err("插件未安装时不能布防");
    assert!(!error.is_empty());
    let conn = b.connect().await;
    let reply = exchange(&conn, &b.hello(&ClientKey::new(31), 1)).await;
    assert_state(&reply, "pending_approval");
}

#[derive(Clone, Copy)]
enum DamagedPlugin {
    EmptyDirectory,
    MissingEntrypoint,
    WrongVersion,
}

async fn assert_damaged_plugin_rejected(damage: DamagedPlugin) {
    let _serial = SERIAL.lock().await;
    let b = Bench::start_with_plugin(true).await;
    let profile = &b.host.profile_dir;
    let package_dir = profile.join("node_modules/@tuvis/dsh-remote-access");
    match damage {
        DamagedPlugin::EmptyDirectory => {
            std::fs::remove_dir_all(&package_dir).unwrap();
            std::fs::create_dir_all(&package_dir).unwrap();
        }
        DamagedPlugin::MissingEntrypoint => {
            std::fs::remove_file(package_dir.join("lib/index.js")).unwrap();
        }
        DamagedPlugin::WrongVersion => {
            let path = package_dir.join("package.json");
            let mut package: Value =
                serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
            package["version"] = json!(format!(
                "{}-forged",
                embedded_manifest().unwrap().plugin_version
            ));
            std::fs::write(path, serde_json::to_vec(&package).unwrap()).unwrap();
        }
    }
    let manifest: Value =
        serde_json::from_slice(&std::fs::read(profile.join("package.json")).unwrap()).unwrap();
    assert!(manifest["dependencies"].get(PACKAGE_NAME).is_some());
    assert!(manifest["dsh"]["profile"]["bundles"]
        .as_array()
        .unwrap()
        .iter()
        .any(|v| v.as_str() == Some(PACKAGE_NAME)));

    let result = b.arm().await;
    let conn = b.connect().await;
    let reply = exchange(&conn, &b.hello(&ClientKey::new(61), 1)).await;
    assert!(
        result.is_err(),
        "残缺插件不得布防: {result:?}; 握手: {reply}"
    );
    assert_state(&reply, "pending_approval");
    b.assert_unbound();
}

#[tokio::test]
async fn arm_rejects_empty_plugin_directory_and_keeps_handshake_pending() {
    assert_damaged_plugin_rejected(DamagedPlugin::EmptyDirectory).await;
}

#[tokio::test]
async fn arm_rejects_missing_plugin_entrypoint_and_keeps_handshake_pending() {
    assert_damaged_plugin_rejected(DamagedPlugin::MissingEntrypoint).await;
}

#[tokio::test]
async fn arm_rejects_wrong_plugin_version_and_keeps_handshake_pending() {
    assert_damaged_plugin_rejected(DamagedPlugin::WrongVersion).await;
}

async fn assert_arm_keeps_runtime_responsive(point: SlowPoint) {
    let _serial = SERIAL.lock().await;
    let b = Bench::start_with_plugin(true).await;
    let (entered_tx, entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let gate = Arc::new(SlowGate {
        entered: Mutex::new(Some(entered_tx)),
        release: Mutex::new(release_rx),
    });
    let instance = b.registry.registered(&b.host.host_id).unwrap();
    let env = DshEnvironment {
        home: b.env.home.clone(),
        tuvis_home: b.env.tuvis_home.clone(),
        dsh_state_dir: b.env.dsh_state_dir.clone(),
        processes: Arc::new(SlowProcesses {
            inner: b.processes.clone(),
            gate: gate.clone(),
            point,
        }),
        files: Arc::new(SlowFiles {
            inner: b.env.files.clone(),
            gate,
            // 管理器读取登记时规范化的 profile，而不是发现阶段的原始路径。
            profile_manifest: instance.profile_dir.join("package.json"),
            point,
        }),
        runner: b.env.runner.clone(),
        clock: b.env.clock.clone(),
        environments: b.env.environments.clone(),
    };
    let manager = Arc::new(production::init_instance_manager(
        &env,
        &instance,
        Arc::new(NoRevoke),
    ));
    let auth = b.svc.clone();
    let instance_id = b.host.host_id.clone();
    let services = move |id: &str| -> Result<_, DshError> {
        assert_eq!(id, instance_id);
        Ok((manager.clone(), auth.clone()))
    };
    let beats = Arc::new(AtomicUsize::new(0));
    let stop = Arc::new(AtomicBool::new(false));
    let heartbeat = {
        let beats = beats.clone();
        let stop = stop.clone();
        tokio::spawn(async move {
            while !stop.load(Ordering::SeqCst) {
                beats.fetch_add(1, Ordering::SeqCst);
                tokio::task::yield_now().await;
            }
        })
    };
    tokio::task::yield_now().await;
    let observer = std::thread::spawn(move || {
        let entered = entered_rx.recv_timeout(WAIT).is_ok();
        let before = beats.load(Ordering::SeqCst);
        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        while entered
            && beats.load(Ordering::SeqCst) == before
            && std::time::Instant::now() < deadline
        {
            std::thread::sleep(Duration::from_millis(5));
        }
        let advanced = beats.load(Ordering::SeqCst) > before;
        let _ = release_tx.send(());
        (entered, advanced)
    });
    let result = dsh_binding_arm_intent_core(&env, &b.registry, b.request(), &services).await;
    stop.store(true, Ordering::SeqCst);
    heartbeat.await.unwrap();
    let (entered, advanced) = observer.join().unwrap();
    assert!(entered, "命令必须到达指定慢路径");
    assert!(advanced, "慢路径等待期间单线程 runtime 的心跳必须推进");
    result.expect("运行中且安装完整的实例必须成功布防");
    let conn = b.connect().await;
    let reply = exchange(&conn, &b.hello(&ClientKey::new(62), 1)).await;
    assert_state(&reply, "authorized");
}

#[tokio::test(flavor = "current_thread")]
async fn arm_keeps_heartbeat_advancing_during_slow_process_scan() {
    assert_arm_keeps_runtime_responsive(SlowPoint::Processes).await;
}

#[tokio::test(flavor = "current_thread")]
async fn arm_keeps_heartbeat_advancing_during_slow_listener_scan() {
    assert_arm_keeps_runtime_responsive(SlowPoint::Listeners).await;
}

#[tokio::test(flavor = "current_thread")]
async fn arm_keeps_heartbeat_advancing_during_slow_profile_read() {
    assert_arm_keeps_runtime_responsive(SlowPoint::ProfileRead).await;
}

#[tokio::test]
async fn rejected_unverified_hello_does_not_consume_command_armed_intent() {
    let _serial = SERIAL.lock().await;
    let b = Bench::start_with_plugin(true).await;
    b.arm().await.expect("已选且运行中的已安装实例可布防");
    let keys = ClientKey::new(31);
    let invalid = b.connect().await;
    let mut forged = b.hello(&keys, 1);
    forged["signature"] = json!(URL_SAFE_NO_PAD.encode([0u8; 64]));
    let rejected = exchange(&invalid, &forged).await;
    assert_eq!(rejected["type"], "error", "未通过签名验证的握手必须被拒绝");
    let valid = b.connect().await;
    let reply = exchange(&valid, &b.hello(&keys, 2)).await;
    assert_state(&reply, "authorized");
    assert_eq!(
        b.registry
            .registered(&b.host.host_id)
            .unwrap()
            .identity_fingerprint,
        digest(&keys.spki()),
        "真实锚点必须写入登记指纹"
    );
}

#[tokio::test]
async fn arm_command_requires_an_explicitly_selected_instance() {
    let _serial = SERIAL.lock().await;
    let b = Bench::start_with_plugin(true).await;
    let registry = Arc::new(InstanceRegistry::open(b._tmp.path().join("unselected.json")).unwrap());
    registry
        .register_candidate(HostKind::Desktop, &b.host.profile_dir)
        .unwrap();
    let manager = b.manager.clone();
    let auth = b.svc.clone();
    let services = move |_: &str| -> Result<_, DshError> { Ok((manager.clone(), auth.clone())) };
    dsh_binding_arm_intent_core(&b.env, &registry, b.request(), &services)
        .await
        .expect_err("唯一已登记实例也不能代替显式选择");
    b.assert_next_requires_approval(32).await;
}

#[tokio::test]
async fn arm_command_rejects_a_request_for_another_selected_instance() {
    let _serial = SERIAL.lock().await;
    let a = Bench::start_with_plugin(true).await;
    let b = Bench::start_with_plugin(true).await;
    a.registry
        .register_candidate(HostKind::Desktop, &b.host.profile_dir)
        .unwrap();
    let a_id = a.host.host_id.clone();
    let a_manager = a.manager.clone();
    let a_auth = a.svc.clone();
    let b_manager = b.manager.clone();
    let b_auth = b.svc.clone();
    let services = move |id: &str| -> Result<_, DshError> {
        if id == a_id {
            Ok((a_manager.clone(), a_auth.clone()))
        } else {
            Ok((b_manager.clone(), b_auth.clone()))
        }
    };
    dsh_binding_arm_intent_core(&a.env, &a.registry, b.request(), &services)
        .await
        .expect_err("请求 B 而选中 A 必须拒绝");
    a.assert_next_requires_approval(33).await;
    b.assert_next_requires_approval(34).await;
}

#[tokio::test]
async fn arm_command_rejects_a_stopped_host_without_granting_a_later_handshake() {
    let _serial = SERIAL.lock().await;
    let b = Bench::start_with_plugin(true).await;
    b.processes.running.store(false, Ordering::SeqCst);
    assert_ne!(
        b.manager.status().unwrap().activation,
        ActivationState::Running
    );
    b.arm().await.expect_err("宿主不运行时不能布防");
    b.processes.running.store(true, Ordering::SeqCst);
    b.assert_next_requires_approval(35).await;
}

#[tokio::test]
async fn switching_selection_during_a_validation_cannot_arm_a_or_b() {
    let _serial = SERIAL.lock().await;
    let a = Bench::start_with_plugin(true).await;
    let b = Bench::start_with_plugin(true).await;
    a.registry
        .register_candidate(HostKind::Desktop, &b.host.profile_dir)
        .unwrap();
    let switched = Arc::new(AtomicBool::new(false));
    let second = Arc::new(a.reopen_registry());
    let switched_in_services = switched.clone();
    let a_id = a.host.host_id.clone();
    let b_id = b.host.host_id.clone();
    let manager = a.manager.clone();
    let auth = a.svc.clone();
    // 服务解析发生在 A 的初始选择检查之后；此处切换真实登记，避免睡眠和调度竞态。
    // 返回的是 A 的真实管理器和授权服务，并非模拟命令判定。
    let services = move |id: &str| -> Result<_, DshError> {
        assert_eq!(id, a_id);
        second.select(&b_id).unwrap();
        switched_in_services.store(true, Ordering::SeqCst);
        Ok((manager.clone(), auth.clone()))
    };
    dsh_binding_arm_intent_core(&a.env, &a.registry, a.request(), &services)
        .await
        .expect_err("核验 A 期间切到 B 必须拒绝布防");
    assert!(switched.load(Ordering::SeqCst), "必须确实经过切换点");
    assert_eq!(
        a.registry.selected().unwrap().unwrap().instance_id,
        b.host.host_id
    );
    a.assert_next_requires_approval(36).await;
    b.assert_next_requires_approval(37).await;
}

#[tokio::test]
async fn arm_command_rejects_web_host_with_unverified_home() {
    let _serial = SERIAL.lock().await;
    let b = Bench::start_with_plugin(true).await;
    let profile = b.env.home.join("profiles/web");
    std::fs::create_dir_all(&profile).unwrap();
    install_embedded_plugin(&profile, b._tmp.path());
    let web_process = DshProcess {
        pid: PID,
        exe: PathBuf::from("/usr/local/bin/node"),
        started_at_ms: STARTED,
        cmdline: vec![
            "/usr/local/bin/node".into(),
            "/opt/dsh/bin/dsh".into(),
            "web".into(),
        ],
    };
    let env = DshEnvironment {
        home: b.env.home.clone(),
        tuvis_home: b.env.tuvis_home.clone(),
        dsh_state_dir: b.env.dsh_state_dir.clone(),
        processes: Arc::new(Processes {
            process: web_process,
            running: AtomicBool::new(true),
        }),
        files: b.env.files.clone(),
        runner: b.env.runner.clone(),
        clock: b.env.clock.clone(),
        environments: b.env.environments.clone(),
    };
    let web = b
        .registry
        .register_candidate(HostKind::Web, &profile)
        .unwrap();
    b.registry.select(&web.instance_id).unwrap();
    let manager = Arc::new(production::init_instance_manager(
        &env,
        &web,
        Arc::new(NoRevoke),
    ));
    let status = manager.status().unwrap();
    assert_eq!(status.installation, InstallationState::Installed);
    assert_eq!(
        status.activation,
        ActivationState::Running,
        "原始 Web 进程在场，但归属仍须核验"
    );
    let auth = Arc::new(
        production::init_instance_auth(&env, &web.instance_id)
            .unwrap()
            .with_registry_anchor(Arc::new(
                production::RegistryFingerprintAnchor::with_registry(b.registry.clone()),
            )),
    );
    let instance_id = web.instance_id.clone();
    let manager_for_services = manager.clone();
    let auth_for_services = auth.clone();
    let services = move |id: &str| -> Result<_, DshError> {
        assert_eq!(id, instance_id);
        Ok((manager_for_services.clone(), auth_for_services.clone()))
    };
    let request = DshInstanceRequest {
        instance_id: web.instance_id.clone(),
    };
    dsh_binding_arm_intent_core(&env, &b.registry, request, &services)
        .await
        .expect_err("Web 宿主 home 不可核验时必须拒绝布防");
    assert!(auth.active_binding_id().is_none());
    assert!(b
        .registry
        .registered(&web.instance_id)
        .unwrap()
        .identity_fingerprint
        .is_empty());
}

#[tokio::test]
async fn selection_aba_during_validation_cannot_arm_the_original_instance() {
    let _serial = SERIAL.lock().await;
    let a = Bench::start_with_plugin(true).await;
    let b = Bench::start_with_plugin(true).await;
    a.register_other(&b);
    let second = Arc::new(a.reopen_registry());
    let switched = Arc::new(AtomicBool::new(false));
    let switched_in_services = switched.clone();
    let a_id = a.host.host_id.clone();
    let b_id = b.host.host_id.clone();
    let manager = a.manager.clone();
    let auth = a.svc.clone();
    let services = move |id: &str| -> Result<_, DshError> {
        assert_eq!(id, a_id);
        second.select(&b_id).unwrap();
        second.select(&a_id).unwrap();
        switched_in_services.store(true, Ordering::SeqCst);
        Ok((manager.clone(), auth.clone()))
    };
    dsh_binding_arm_intent_core(&a.env, &a.registry, a.request(), &services)
        .await
        .expect_err("核验期间 A→B→A 必须拒绝");
    assert!(switched.load(Ordering::SeqCst));
    assert_eq!(
        a.registry.selected().unwrap().unwrap().instance_id,
        a.host.host_id
    );
    a.assert_next_not_authorized(38).await;
    b.assert_next_not_authorized(39).await;
}

#[tokio::test]
async fn switching_away_after_arm_invalidates_the_intent() {
    let _serial = SERIAL.lock().await;
    let a = Bench::start_with_plugin(true).await;
    let b = Bench::start_with_plugin(true).await;
    a.register_other(&b);
    a.arm().await.unwrap();
    a.reopen_registry().select(&b.host.host_id).unwrap();
    a.assert_next_not_authorized(40).await;
    b.assert_next_not_authorized(41).await;
}

#[tokio::test]
async fn selection_aba_after_arm_does_not_revive_the_intent() {
    let _serial = SERIAL.lock().await;
    let a = Bench::start_with_plugin(true).await;
    let b = Bench::start_with_plugin(true).await;
    a.register_other(&b);
    a.arm().await.unwrap();
    let second = a.reopen_registry();
    second.select(&b.host.host_id).unwrap();
    second.select(&a.host.host_id).unwrap();
    a.assert_next_not_authorized(42).await;
    b.assert_next_not_authorized(43).await;
}

#[cfg(feature = "dsh-test-probes")]
struct IntentPause {
    entered: tokio::sync::oneshot::Sender<()>,
    release: tokio::sync::oneshot::Receiver<()>,
}

#[cfg(feature = "dsh-test-probes")]
fn intent_pause_slot() -> &'static Arc<std::sync::Mutex<Option<IntentPause>>> {
    static SLOT: std::sync::OnceLock<Arc<std::sync::Mutex<Option<IntentPause>>>> =
        std::sync::OnceLock::new();
    SLOT.get_or_init(|| {
        let slot = Arc::new(std::sync::Mutex::new(None::<IntentPause>));
        let hook_slot = slot.clone();
        assert!(
            auth::init_intent_consumed_hook_for_test(Arc::new(move || {
                let pause = hook_slot.lock().unwrap().take();
                Box::pin(async move {
                    if let Some(pause) = pause {
                        let _ = pause.entered.send(());
                        let _ = tokio::time::timeout(WAIT * 3, pause.release).await;
                    }
                })
            }))
            .is_ok(),
            "测试二进制必须独占 intent 钩子"
        );
        slot
    })
}

#[cfg(feature = "dsh-test-probes")]
async fn assert_switch_after_intent_consumed_rejects_commit(aba: bool) {
    let _serial = SERIAL.lock().await;
    let a = Bench::start_with_plugin(true).await;
    let b = Bench::start_with_plugin(true).await;
    a.register_other(&b);
    let second = a.reopen_registry();
    a.arm().await.unwrap();
    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
    assert!(intent_pause_slot()
        .lock()
        .unwrap()
        .replace(IntentPause {
            entered: entered_tx,
            release: release_rx,
        })
        .is_none());
    let conn = a.connect().await;
    conn.send(&a.hello(&ClientKey::new(44), 1).to_string())
        .await
        .unwrap();
    tokio::time::timeout(WAIT, entered_rx)
        .await
        .expect("意图消费钩子未到达")
        .expect("钩子未通知");
    let a_id = a.host.host_id.clone();
    let b_id = b.host.host_id.clone();
    let mut switching = tokio::task::spawn_blocking(move || {
        second.select(&b_id).unwrap();
        if aba {
            second.select(&a_id).unwrap();
        }
    });
    let switched = tokio::time::timeout(WAIT, &mut switching).await;
    // 即使切换被意外锁住，也先释放 hook，避免测试把运行时永久卡住。
    release_tx.send(()).expect("提交屏障必须仍在等待");
    switched
        .expect("意图消费暂停时选择不得被锁住")
        .expect("切换任务失败");
    let reply = receive(&conn).await;
    assert_eq!(reply["type"], "error", "已消费意图的提交必须拒绝: {reply}");
    a.assert_unbound();
    b.assert_unbound();
}

#[cfg(feature = "dsh-test-probes")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn switch_after_intent_consumption_rejects_fingerprint_commit() {
    assert_switch_after_intent_consumed_rejects_commit(false).await;
}

#[cfg(feature = "dsh-test-probes")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn selection_aba_after_intent_consumption_rejects_fingerprint_commit() {
    assert_switch_after_intent_consumed_rejects_commit(true).await;
}

// 待生产侧提供的仅测试探针：同步回调必须在 Intent 事务保护内执行，阶段名见下方。
// 接口：init_intent_commit_stage_hook_for_test(Arc<dyn Fn(&'static str) + Send + Sync>)。
#[cfg(feature = "dsh-test-probes")]
struct CommitPause {
    stage: &'static str,
    entered: tokio::sync::oneshot::Sender<()>,
    release: std::sync::mpsc::Receiver<()>,
}

#[cfg(feature = "dsh-test-probes")]
fn commit_pause_slot() -> &'static Arc<std::sync::Mutex<Option<CommitPause>>> {
    static SLOT: std::sync::OnceLock<Arc<std::sync::Mutex<Option<CommitPause>>>> =
        std::sync::OnceLock::new();
    SLOT.get_or_init(|| {
        let slot = Arc::new(std::sync::Mutex::new(None::<CommitPause>));
        let hook_slot = slot.clone();
        assert!(
            auth::init_intent_commit_stage_hook_for_test(Arc::new(move |stage| {
                let pause = {
                    let mut slot = hook_slot.lock().unwrap();
                    if slot.as_ref().is_some_and(|p| p.stage == stage) {
                        slot.take()
                    } else {
                        None
                    }
                };
                if let Some(pause) = pause {
                    let _ = pause.entered.send(());
                    let _ = pause.release.recv_timeout(WAIT * 5);
                }
            }))
            .is_ok()
        );
        slot
    })
}

#[cfg(feature = "dsh-test-probes")]
fn pause_commit(
    stage: &'static str,
) -> (
    tokio::sync::oneshot::Receiver<()>,
    std::sync::mpsc::Sender<()>,
) {
    let (entered, seen) = tokio::sync::oneshot::channel();
    let (release, wait) = std::sync::mpsc::channel();
    assert!(commit_pause_slot()
        .lock()
        .unwrap()
        .replace(CommitPause {
            stage,
            entered,
            release: wait
        })
        .is_none());
    (seen, release)
}

#[cfg(feature = "dsh-test-probes")]
struct RollbackPause {
    real: production::RegistryFingerprintAnchor,
    entered: std::sync::Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
    release: std::sync::Mutex<std::sync::mpsc::Receiver<()>>,
}

#[cfg(feature = "dsh-test-probes")]
impl auth::RegistryFingerprintAnchor for RollbackPause {
    fn accepts(&self, id: &str, fingerprint: &str) -> bool {
        auth::RegistryFingerprintAnchor::accepts(&self.real, id, fingerprint)
    }
    fn anchor_fingerprint(&self, id: &str, fingerprint: &str) -> Result<(), DshError> {
        auth::RegistryFingerprintAnchor::anchor_fingerprint(&self.real, id, fingerprint)
    }
    fn current_fingerprint(&self, id: &str) -> Result<String, DshError> {
        auth::RegistryFingerprintAnchor::current_fingerprint(&self.real, id)
    }
    fn commit_fingerprint(
        &self,
        id: &str,
        fingerprint: &str,
        mode: &auth::FingerprintCommitMode,
    ) -> Result<auth::FingerprintCommitReceipt, DshError> {
        auth::RegistryFingerprintAnchor::commit_fingerprint(&self.real, id, fingerprint, mode)
    }
    fn rollback_fingerprint(
        &self,
        receipt: &auth::FingerprintCommitReceipt,
    ) -> Result<(), DshError> {
        if let Some(entered) = self.entered.lock().unwrap().take() {
            let _ = entered.send(());
        }
        let _ = self.release.lock().unwrap().recv_timeout(WAIT * 5);
        auth::RegistryFingerprintAnchor::rollback_fingerprint(&self.real, receipt)
    }
}

#[cfg(feature = "dsh-test-probes")]
async fn switch_while_commit_paused(stage: &'static str, compensate: bool) {
    let _serial = SERIAL.lock().await;
    let (rollback_entered, rollback_seen) = tokio::sync::oneshot::channel();
    let (rollback_release, rollback_wait) = std::sync::mpsc::channel();
    let a = Bench::start_with_anchor(true, move |registry| {
        Arc::new(RollbackPause {
            real: production::RegistryFingerprintAnchor::with_registry(registry),
            entered: std::sync::Mutex::new(Some(rollback_entered)),
            release: std::sync::Mutex::new(rollback_wait),
        })
    })
    .await;
    let b = Bench::start_with_plugin(true).await;
    a.register_other(&b);
    let second = a.reopen_registry();
    a.arm().await.unwrap();
    let (entered, release) = pause_commit(stage);
    let conn = a.connect().await;
    let key = ClientKey::new(50);
    conn.send(&a.hello(&key, 1).to_string()).await.unwrap();
    tokio::time::timeout(WAIT, entered)
        .await
        .expect("提交阶段未到达")
        .expect("阶段钩子已丢失");
    // 直接读快照文件，避免在被测 registry 锁上重入。
    let registry: Value =
        serde_json::from_slice(&std::fs::read(a._tmp.path().join("instances.json")).unwrap())
            .unwrap();
    let row = registry["instances"]
        .as_array()
        .unwrap()
        .iter()
        .find(|v| v["instanceId"] == a.host.host_id)
        .unwrap();
    assert_eq!(
        row["identityFingerprint"],
        digest(&key.spki()),
        "阶段必须在真实 CAS 成功之后"
    );
    if stage == "before_trusted_publish" {
        let ledger: Value = serde_json::from_slice(&a.ledger()).unwrap();
        assert_eq!(ledger["bindings"][0]["state"], "bound", "账本必须已提交");
    } else if let Ok(bytes) = std::fs::read(a.state_dir().join("bindings.json")) {
        let ledger: Value = serde_json::from_slice(&bytes).unwrap();
        assert!(
            !ledger["bindings"]
                .as_array()
                .unwrap()
                .iter()
                .any(|row| row["state"] == "bound"),
            "账本尚未提交"
        );
    }
    let (started, observed) = tokio::sync::oneshot::channel();
    let b_id = b.host.host_id.clone();
    let mut selecting = tokio::task::spawn_blocking(move || {
        let _ = started.send(());
        second.select(&b_id)
    });
    tokio::time::timeout(WAIT, observed).await.unwrap().unwrap();
    let prematurely_completed = tokio::time::timeout(Duration::from_millis(200), &mut selecting)
        .await
        .is_ok();
    if compensate {
        a.clock.0.store(NOW + 61_000, Ordering::SeqCst);
    }
    release.send(()).unwrap();
    assert!(!prematurely_completed, "提交中 select(B) 不得完成");
    if compensate {
        tokio::time::timeout(WAIT, rollback_seen)
            .await
            .expect("未进入指纹补偿")
            .unwrap();
        let completed_during_rollback =
            tokio::time::timeout(Duration::from_millis(200), &mut selecting)
                .await
                .is_ok();
        rollback_release.send(()).unwrap();
        assert!(
            !completed_during_rollback,
            "指纹补偿结束前 select(B) 不得完成"
        );
    }
    let reply = receive(&conn).await;
    tokio::time::timeout(WAIT, selecting)
        .await
        .expect("提交结束后选择仍阻塞")
        .unwrap()
        .unwrap();
    assert_eq!(
        a.registry.selected().unwrap().unwrap().instance_id,
        b.host.host_id
    );
    if compensate {
        assert_eq!(reply["type"], "error", "过期提交必须失败");
        a.assert_unbound();
    } else {
        assert_state(&reply, "authorized");
        assert_eq!(
            a.registry
                .registered(&a.host.host_id)
                .unwrap()
                .identity_fingerprint,
            digest(&key.spki())
        );
        let ledger: Value = serde_json::from_slice(&a.ledger()).unwrap();
        assert_eq!(ledger["bindings"][0]["state"], "bound");
    }
}

#[cfg(feature = "dsh-test-probes")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn select_waits_after_intent_anchor_cas() {
    switch_while_commit_paused("after_anchor_cas", false).await;
}

#[cfg(feature = "dsh-test-probes")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn select_waits_before_intent_ledger_write() {
    switch_while_commit_paused("before_ledger_write", false).await;
}

#[cfg(feature = "dsh-test-probes")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn select_waits_before_intent_trusted_publish() {
    switch_while_commit_paused("before_trusted_publish", false).await;
}

#[cfg(feature = "dsh-test-probes")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn select_waits_until_failed_intent_is_compensated() {
    switch_while_commit_paused("after_anchor_cas", true).await;
}
