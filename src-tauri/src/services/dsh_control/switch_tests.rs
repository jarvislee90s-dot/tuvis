//! 「远程控制 DSH」单开关回归（brief 验收逐条；unix——夹具走真实临时目录 +
//! mem IPC，隔离 TUVIS_HOME/DSH 状态目录，绝不触真实 `~/.dsh`/`~/.tuvis`）。
//!
//! 覆盖面（brief 验收映射）：
//! * `enable_auto_skips_ready_steps` —— 事实就绪的步骤自动跳过（安装/授权不重做）；
//! * `enable_completes_via_armed_intent` —— 干净实例：布防短时意图 → 真实握手
//!   自动绑定 → 就绪（无第二次绑定/核对码/手工安装）；
//! * `enable_installs_then_waits_reopen_then_ready` —— 线性推进：等待退出 →
//!   安装 → 提示重开 → 核验加载 → 授权 → 就绪；
//! * `install_failure_keeps_step_with_repair_resume` —— 安装失败留在当前步骤，
//!   修复完成后自动接续原流程；
//! * `disable_invalidates_immediately_then_uninstalls` —— 点击关闭后立即验证
//!   授权失效；宿主退出后自动完成卸载；
//! * `disable_while_installing_cancels_and_uninstalls` —— 等待/安装中关闭：
//!   取消上一轮作业 → 自动卸载（等待退出不强杀）；
//! * `uninstall_failure_keeps_off_with_retry` —— 卸载失败保持远控关闭 +
//!   失败步骤 + 重试；
//! * `late_receipt_cannot_reenable_closed_instance` —— 迟到回执/旧代次 CAS 拒绝；
//! * `instance_switch_pauses_old_intent` —— 等待中切换实例：旧意图暂停不失败，
//!   不作用于另一实例；
//! * `upgrade_restores_switch_from_existing_binding` —— 已有合法绑定按现有事实
//!   恢复开关；
//! * `same_identity_dir_cycle_reenable_auto_authorizes` —— **同一份插件身份目录**
//!   走完「开启→关闭→自动卸载→再开启」：卸载退役旧身份（目录删除 + 登记锚清空），
//!   再开启装出全新钥经本次开启的授权意图自动绑定——全程无手工步骤、无核对码；
//! * `upgrade_compat_revoked_tombstone_and_stale_identity_retired_on_reenable` ——
//!   升级兼容：已撤权墓碑 + 旧身份目录仍在场（旧构建终态）→ 再开启的安装路径先
//!   退役旧身份再装；
//! * `upgrade_compat_installed_dead_identity_reinstalls_before_reopen` —— 升级
//!   兼容：插件仍装着但带死钥（撤权墓碑 + 旧身份在场）→ 再开启路由进安装段
//!   （等待宿主正常退出后退役旧身份再装），不进授权死等；
//! * `uninstall_identity_removal_failure_reports_retryable_then_retry_succeeds` ——
//!   卸载删身份失败 → 如实报失败可重试（旧钥在场绝不谎报成功），重试后完成；
//! * `disable_during_install_wait_cancels_job_and_late_receipt_stays_off` ——
//!   等待退出期间可关闭（安装作业被取消）+ 迟到的安装成功回执不得激活已关闭实例；
//! * `rebind_required_on_identity_drift_then_ready` —— 身份漂移（登记指纹 ≠ 新钥）
//!   进可信重绑面板，确认后到就绪（身份变化不得无提示信任）；
//! * 门槛单测：`gate_authorization` / `gate_lifecycle`。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use super::*;

#[path = "switch_select_tests.rs"]
mod switch_select_tests;
use crate::services::dsh_control::error::{codes, DshError};
use crate::services::dsh_control::instances::{self, HostKind};
use crate::services::dsh_control::local_ipc::{mem, PeerOsIdentity};
use crate::services::dsh_control::model::{
    AuthorizationState, CommandRequest, DshClock, DshCommandRunner, DshEnvironment, DshFileSource,
    DshListener, DshProcess, DshProcessSource, ManagementKind, ManagementPhase, RunOutcome,
    TestUnobservableEnvironments,
};
use crate::services::dsh_control::{auth, identity, install};

// ============================================================
// 夹具
// ============================================================

const HOST_PID: u32 = 424242;
const HOST_STARTED_AT: i64 = 1_760_000_000_000;
const HOST_AUTHORITY: &str = "127.0.0.1:5217";
const HOST_BINDING_KEY: &[u8] = b"switch-fixture-binding-key";

/// 可开关的宿主进程源（active = 宿主在场）。
struct SwitchHost {
    active: AtomicBool,
    listener_active: AtomicBool,
    extra: Mutex<Vec<DshProcess>>,
    exe: PathBuf,
    profile_token: String,
}

impl DshProcessSource for SwitchHost {
    fn list_processes(&self) -> Vec<DshProcess> {
        let extra = self.extra.lock().unwrap();
        let mut processes = extra.clone();
        if self.active.load(Ordering::SeqCst) {
            processes.push(DshProcess {
                pid: HOST_PID,
                exe: self.exe.clone(),
                cmdline: vec![
                    self.exe.to_string_lossy().into_owned(),
                    "dsh-desktop-host/lib/index.js".into(),
                    self.profile_token.clone(),
                ],
                started_at_ms: HOST_STARTED_AT,
            });
        }
        processes
    }
    fn listening_sockets(&self) -> Vec<DshListener> {
        if self.active.load(Ordering::SeqCst) && self.listener_active.load(Ordering::SeqCst) {
            vec![DshListener {
                owner_pid: HOST_PID,
                authority: HOST_AUTHORITY.into(),
            }]
        } else {
            vec![]
        }
    }
}

/// 固定时钟（本套回归不推进时间；意图 TTL 以「刚布防必未过期」断言）。
struct FixedClock;
impl DshClock for FixedClock {
    fn now_ms(&self) -> i64 {
        HOST_STARTED_AT
    }
}

/// 脚本化官方 CLI：add/remove/list 按真实契约作答（add 成功时真实落盘插件
/// 标记、remove 真实清理），失败注入开关控制。`adds` 单独计数——死钥判据的
/// 「无重装循环」断言面（复审 Critical）。
struct ScriptedRunner {
    calls: AtomicUsize,
    adds: AtomicUsize,
    removes: AtomicUsize,
    remove_profiles: Mutex<Vec<PathBuf>>,
    fail_add: AtomicBool,
    fail_remove: AtomicBool,
    installed: AtomicBool,
    profile: PathBuf,
    version: String,
}

impl DshCommandRunner for ScriptedRunner {
    fn run(&self, request: &CommandRequest) -> Result<RunOutcome, DshError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let args: Vec<String> = request
            .args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        let op = args.get(3).map(String::as_str).unwrap_or_default();
        match op {
            "add" => {
                self.adds.fetch_add(1, Ordering::SeqCst);
                if self.fail_add.load(Ordering::SeqCst) {
                    return Ok(RunOutcome {
                        exit_code: 1,
                        stdout: String::new(),
                        stderr: "Error: fixture add failure".into(),
                    });
                }
                self.write_plugin_files();
                self.installed.store(true, Ordering::SeqCst);
                Ok(RunOutcome {
                    exit_code: 0,
                    stdout: String::new(),
                    stderr: String::new(),
                })
            }
            "remove" => {
                self.removes.fetch_add(1, Ordering::SeqCst);
                if let Some((_, home)) = request.env.iter().find(|(key, _)| key == "DSH_HOME") {
                    self.remove_profiles
                        .lock()
                        .unwrap()
                        .push(PathBuf::from(home).join("profiles").join(&args[2]));
                }
                if self.fail_remove.load(Ordering::SeqCst) {
                    return Ok(RunOutcome {
                        exit_code: 1,
                        stdout: String::new(),
                        stderr: "Error: fixture remove failure".into(),
                    });
                }
                let _ = std::fs::remove_dir_all(self.profile.join("node_modules"));
                std::fs::write(self.profile.join("package.json"), b"{}").unwrap();
                self.installed.store(false, Ordering::SeqCst);
                Ok(RunOutcome {
                    exit_code: 0,
                    stdout: String::new(),
                    stderr: String::new(),
                })
            }
            "list" => {
                let stdout = if self.installed.load(Ordering::SeqCst) {
                    format!(
                        "dsh-profile-desktop {} (PRIVATE)\n└── {}@{}\n1 package\n",
                        self.profile.display(),
                        install::PACKAGE_NAME,
                        self.version
                    )
                } else {
                    String::new()
                };
                Ok(RunOutcome {
                    exit_code: 0,
                    stdout,
                    stderr: String::new(),
                })
            }
            other => panic!("fixture runner saw unexpected cli op: {other}"),
        }
    }
}

impl ScriptedRunner {
    /// 真实落盘插件标记（与官方 add 后的文件态同形）。
    fn write_plugin_files(&self) {
        let pkg_dir = self.profile.join("node_modules/@tuvis/dsh-remote-access");
        std::fs::create_dir_all(&pkg_dir).unwrap();
        std::fs::write(
            self.profile.join("package.json"),
            format!(
                r#"{{"dependencies":{{"{name}":"file:cached.tgz"}},"dsh":{{"profile":{{"bundles":["{name}"]}}}}}}"#,
                name = install::PACKAGE_NAME
            ),
        )
        .unwrap();
        std::fs::write(
            pkg_dir.join("package.json"),
            format!(
                r#"{{"name":"{}","version":"{}"}}"#,
                install::PACKAGE_NAME,
                self.version
            ),
        )
        .unwrap();
    }
}

/// 限根真实文件源（根外一概不可见；物理 profile 锁启用）。
struct FixtureFiles(PathBuf);

impl DshFileSource for FixtureFiles {
    fn requires_physical_profile_lock(&self) -> bool {
        true
    }
    fn exists(&self, path: &Path) -> bool {
        path.exists()
    }
    fn is_dir(&self, path: &Path) -> bool {
        path.is_dir()
    }
    fn list_dir(&self, path: &Path) -> Vec<PathBuf> {
        if !path.starts_with(&self.0) {
            return vec![];
        }
        std::fs::read_dir(path)
            .map(|entries| {
                entries
                    .filter_map(Result::ok)
                    .map(|entry| entry.path())
                    .collect()
            })
            .unwrap_or_default()
    }
    fn read_small(&self, path: &Path, max: u64) -> Result<Vec<u8>, DshError> {
        crate::services::dsh_control::production::FsFileSource.read_small(path, max)
    }
}

/// 一套完整开关夹具（单实例；多实例用例在 registry 上另登记第二实例）。
struct Fixture {
    _temp: tempfile::TempDir,
    root: PathBuf,
    home: PathBuf,
    profile: PathBuf,
    env: Arc<DshEnvironment>,
    registry: Arc<InstanceRegistry>,
    instance_id: String,
    host: Arc<SwitchHost>,
    runner: Arc<ScriptedRunner>,
    auth: Arc<DshAuthService>,
    manager: Arc<DshPluginManager>,
    service: Arc<RemoteSwitchService>,
    /// 服务来源闭包（manager+auth 的注入装配；重启模拟用例给新服务实例复用
    /// 同一 Arc——「新进程同状态目录」下服务对象重建而事实面不变）。
    services: Arc<SwitchServices>,
    /// 升级证据只读源（生产 `upgrade_evidence_source` 同判据的夹具投影；重启
    /// 模拟用例给新服务实例复用同一 Arc）。
    evidence_source: Arc<SwitchEvidenceSource>,
    /// mem 握手客户端工厂（with_listener 注入；不绑真实套接字）。
    factory: Arc<mem::MemClientFactory>,
}

impl Fixture {
    fn new() -> Self {
        Self::build(false, false)
    }

    /// Round-8 P1-3：自定义 DSH_HOME 变体——宿主真实 home（argv profile token
    /// 指向处 = 登记核验 home）与兔维斯侧推导 env.home（`production_env` 口径）
    /// **刻意分开**：升级实例在自定义 home 下运行时，推导 home 是错的目录。
    /// 装配口径与生产一致：安装管理器经 `init_instance_manager`（env.home =
    /// 登记核验 home），开关服务吃推导 env（`switch_for_instance` 用
    /// `production_env`）——两处口径差异即根因现场。
    fn new_with_custom_host_home() -> Self {
        Self::build(true, false)
    }

    /// 第九轮 P1-2：旧登记形态（升级携带）——候选登记**不带 dsh_home**（旧构建
    /// 的登记行形状）。关闭链路必须先回填登记 home 再撤权，否则卸载受理门在
    /// 撤权后永远 Unproven（关闭+卸载死路）。
    fn new_legacy() -> Self {
        Self::build(false, true)
    }

    fn build(custom_host_home: bool, legacy_no_dsh_home: bool) -> Self {
        let _ = env_logger::builder().is_test(true).try_init();
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let physical = root.join("physical/profile");
        std::fs::create_dir_all(&physical).unwrap();
        std::fs::write(physical.join("package.json"), b"{}").unwrap();
        let home = if custom_host_home {
            root.join("custom-home")
        } else {
            root.join("home-a")
        };
        std::fs::create_dir_all(home.join("profiles")).unwrap();
        std::os::unix::fs::symlink(&physical, home.join("profiles/desktop")).unwrap();
        // 兔维斯侧推导 env.home：custom 模式下是独立空目录（无 profile 链接——
        // 推导值对自定义 home 宿主完全错位；非 custom 模式与真实 home 同目录）。
        let env_home = if custom_host_home {
            root.join("env-home")
        } else {
            home.clone()
        };
        std::fs::create_dir_all(env_home.join("profiles")).unwrap();
        let exe = root.join("Applications/DSH.app/Contents/MacOS/DeepSeek Harness");
        std::fs::create_dir_all(exe.parent().unwrap()).unwrap();
        std::fs::write(&exe, b"fixture").unwrap();
        let host = Arc::new(SwitchHost {
            active: AtomicBool::new(true),
            listener_active: AtomicBool::new(true),
            extra: Mutex::new(Vec::new()),
            exe: exe.clone(),
            // argv 携带宿主**真实** home 的 profile 入口（发现/登记的权威来源）。
            profile_token: home.join("profiles/desktop").to_string_lossy().into_owned(),
        });
        let version = install::embedded_manifest().unwrap().plugin_version;
        let runner = Arc::new(ScriptedRunner {
            calls: AtomicUsize::new(0),
            adds: AtomicUsize::new(0),
            removes: AtomicUsize::new(0),
            remove_profiles: Mutex::new(Vec::new()),
            fail_add: AtomicBool::new(false),
            fail_remove: AtomicBool::new(false),
            installed: AtomicBool::new(false),
            profile: physical.clone(),
            version,
        });
        let mut env = (*crate::services::dsh_control::production::production_env()).clone();
        env.home = env_home;
        env.tuvis_home = root.join("tuvis");
        env.dsh_state_dir = Some(root.join("state"));
        env.processes = host.clone();
        env.files = Arc::new(FixtureFiles(root.clone()));
        env.runner = runner.clone();
        env.clock = Arc::new(FixedClock);
        env.environments = Arc::new(TestUnobservableEnvironments);
        let registry =
            Arc::new(instances::InstanceRegistry::open(root.join("instances.json")).unwrap());
        // 干净实例：候选登记（指纹为空 = 尚未锚定；首绑由意图/批准锚定）。
        // legacy 变体：不带 dsh_home（旧构建登记行形状；第九轮 P1-2 关闭链回归）。
        let instance = if legacy_no_dsh_home {
            registry
                .register_candidate(HostKind::Desktop, &physical)
                .unwrap()
        } else {
            registry
                .register_candidate_with_home(HostKind::Desktop, &physical, Some(&home))
                .unwrap()
        };
        registry.select(&instance.instance_id).unwrap();
        let env = Arc::new(env);

        // mem IPC（serve 用注入监听端；回归不起真实 UDS）。
        let (listener, factory) = mem::mem_listener(PeerOsIdentity {
            pid: HOST_PID,
            uid: Some(501),
        });
        let auth = Arc::new(
            DshAuthService::new_for_instance((*env).clone(), &instance.instance_id)
                .unwrap()
                .with_registry_anchor(
                    Arc::new(
                        crate::services::dsh_control::production::RegistryFingerprintAnchor::with_registry(
                            Arc::clone(&registry),
                        ),
                    ) as Arc<dyn auth::RegistryFingerprintAnchor>,
                )
                .without_liveness_watcher()
                .with_listener(Box::new(listener)),
        );
        let manager = Arc::new(Self::assemble_manager(
            &env,
            &registry,
            &instance,
            &physical,
            &auth,
            custom_host_home,
        ));
        let services: Arc<SwitchServices> = {
            let manager = Arc::clone(&manager);
            let auth = Arc::clone(&auth);
            Arc::new(move || Ok((Arc::clone(&manager), Arc::clone(&auth))))
        };
        // 升级证据只读源（生产 `upgrade_evidence_source` 同判据的夹具投影）：
        // 证据读取零装配副作用（见 fixture_upgrade_evidence_facts）。
        let evidence_source: Arc<SwitchEvidenceSource> = {
            let env = Arc::clone(&env);
            let manager = Arc::clone(&manager);
            let id = instance.instance_id.clone();
            Arc::new(move || fixture_upgrade_evidence_facts(&env, &manager, &id))
        };
        let services_for_fixture = Arc::clone(&services);
        let evidence_for_fixture = Arc::clone(&evidence_source);
        let service = Arc::new(
            RemoteSwitchService::new(
                Arc::clone(&env),
                Arc::clone(&registry),
                instance.instance_id.clone(),
                services,
                evidence_for_fixture,
            )
            .unwrap()
            .with_poll(Duration::from_millis(20)),
        );
        // P1-1：授权提交点执行门（生产装配同款——switch 域闭包接线到 auth 域；
        // `production::auth_for_instance` 的登记形状）。
        auth.set_switch_gate(switch_authorization_gate(Arc::clone(&service)));
        Self {
            _temp: temp,
            root,
            home,
            profile: physical,
            env,
            registry,
            instance_id: instance.instance_id,
            host,
            runner,
            auth,
            manager,
            service,
            services: services_for_fixture,
            evidence_source,
            factory,
        }
    }

    /// 预置插件已安装（事实注入；不等 CLI）。
    fn seed_plugin_installed(&self) {
        self.runner.write_plugin_files();
        self.runner.installed.store(true, Ordering::SeqCst);
    }

    /// 预置 Bound 绑定（真实账本行；profile 指向夹具物理 profile）。
    fn seed_bound_binding(&self) {
        self.auth.seed_bound_for_test(
            "fixture-binding",
            &auth::b64url_encode(HOST_BINDING_KEY),
            &self.profile.to_string_lossy(),
        );
    }

    /// 预置 Bound 绑定的**落盘形态**（升级迁移世界：账本行由旧构建直接写盘，
    /// 从未经当代握手/开关面触碰）。与 [`Self::seed_bound_binding`] 的差别：seed
    /// 注入只在内存账本，而「重启后新服务实例按盘面装载」的升级形态必须真落盘
    /// ——`read_raw`/新 auth 域读的都是盘面。幂等追加，不覆盖既有行。
    fn write_bound_row_to_disk(&self, binding_id: &str, spki_b64: &str) {
        let row = serde_json::json!({
            "bindingId": binding_id,
            "publicKeySpki": spki_b64,
            "revokedKeySpki": "",
            "executable": "/fixture/dsh",
            "pid": 1,
            "startedAtMs": 1,
            "profileDir": self.profile.to_string_lossy(),
            "createdAtMs": 2,
            "state": "bound",
        });
        let path = self.bindings_ledger_file();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut ledger: serde_json::Value = std::fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_else(|| serde_json::json!({ "bindings": [] }));
        ledger["bindings"]
            .as_array_mut()
            .expect("账本必须含 bindings 数组")
            .push(row);
        std::fs::write(&path, serde_json::to_vec(&ledger).unwrap()).unwrap();
    }

    /// 当前当代宿主代际（hello 用）。
    fn host_epoch(&self) -> String {
        identity::discover_instance_candidate(&self.env, &self.instance_id)
            .unwrap()
            .host
            .epoch
            .to_string()
    }

    /// 插件身份目录（实例域；与插件 `instanceFileStorage` 的落点同形——
    /// `packages/dsh-remote-access` index.ts：`<dshHome>/tuvis-remote/<hostId>/`）。
    fn plugin_identity_dir(&self) -> PathBuf {
        self.home.join("tuvis-remote").join(&self.instance_id)
    }

    fn plugin_identity_file(&self) -> PathBuf {
        self.plugin_identity_dir().join("identity.json")
    }

    /// 模拟插件**启动会话前**的身份落盘（复审 Critical 保真面：真实插件
    /// `generateAndStore()` 先于 transport.connect / handshake——auth.ts:478/648；
    /// 不存在「首绑成功后才写身份」的生命周期）。内容形态与插件 identity.json
    /// 一致（`publicKeySpki` = SPKI DER base64url）；死钥判据读此字段比对墓碑。
    fn write_plugin_identity(&self, spki: &[u8]) {
        let dir = self.plugin_identity_dir();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            self.plugin_identity_file(),
            format!(
                r#"{{"version":1,"privateKeyPem":"fixture","publicKeySpki":"{}","trustAnchorSpki":null,"revoked":false,"revokedBindingId":null}}"#,
                auth::b64url_encode(spki)
            ),
        )
        .unwrap();
    }

    /// 模拟旧构建终态的撤权：对当前 Bound 行直接撤权（墓碑落账；不经开关——
    /// 升级兼容用例要在「新卸载退役段」缺席的前提下构造旧状态）。
    fn revoke_bound_directly(&self) {
        let binding_id = self.auth.active_binding_id().expect("前置：Bound 绑定在场");
        self.auth.revoke_local(&binding_id).unwrap();
    }

    /// 模拟旧构建的包移除（文件态清空；身份目录不动）。
    fn remove_plugin_files(&self) {
        self.runner.installed.store(false, Ordering::SeqCst);
        let _ = std::fs::remove_dir_all(self.profile.join("node_modules"));
        std::fs::write(self.profile.join("package.json"), b"{}").unwrap();
    }

    /// env 接线的登记句柄（生产 `production_registry` 同形——open_with_env）：
    /// 只读历史解析的生产入口需要 env（离线安全的历史钉住源）。夹具主句柄
    /// 保持无 env 形态（既有用例口径），读面用例各自开句柄读同一 instances.json。
    fn readonly_registry(&self) -> instances::InstanceRegistry {
        instances::InstanceRegistry::open_with_env(
            self.root.join("instances.json"),
            Some(Arc::clone(&self.env)),
        )
        .unwrap()
    }

    /// 管理器装配内核（`build` 与重启模拟共用；接线与生产装饰器同形）：
    /// 撤权 resolver 指向本夹具授权域、身份退役面清登记锚、**作业受理门**跑
    /// 生产内核 `verify_job_admission`（第九轮 P1-2：没有它，「登记 home 证据
    /// 失效 → start 被拒」的关闭死路在夹具里不可复现）。
    fn assemble_manager(
        env: &Arc<DshEnvironment>,
        registry: &Arc<InstanceRegistry>,
        instance: &instances::SelectedInstance,
        physical: &Path,
        auth: &Arc<DshAuthService>,
        custom_host_home: bool,
    ) -> DshPluginManager {
        let revoker =
            crate::services::dsh_control::production::InstanceBindingRevoker::with_resolver(
                Arc::clone(env),
                instance.instance_id.clone(),
                {
                    let auth = Arc::clone(auth);
                    Arc::new(move || Ok(Arc::clone(&auth)))
                },
            );
        let mut manager = if custom_host_home {
            // 生产装配同款：env.home = 登记核验 home（自定义 home 升级实例的
            // 管理器 home 来自登记证据，而非兔维斯推导值）。
            crate::services::dsh_control::production::init_instance_manager(
                env,
                instance,
                Arc::new(revoker),
            )
        } else {
            DshPluginManager::for_instance(
                (**env).clone(),
                Arc::new(revoker),
                install::InstanceTarget {
                    instance_id: instance.instance_id.clone(),
                    kind: HostKind::Desktop,
                    profile_dir: physical.to_path_buf(),
                },
            )
        };
        // P1-2 生产同款接线：插件身份退役面（旧钥退役时清空登记指纹锚——
        // 再开启的新钥走首绑 TOFU；旧钥由撤权墓碑拦截，不因锚清空复活）。
        manager.attach_identity_retirer({
            let registry = Arc::clone(registry);
            Arc::new(move |instance_id: &str| registry.retire_identity_fingerprint(instance_id))
        });
        let gated_registry = Arc::clone(registry);
        let gated_env = Arc::clone(env);
        let gated_id = instance.instance_id.clone();
        let assembled_home = instances::normalize_profile(manager.assembled_home());
        manager.attach_job_admission_gate(Arc::new(move |scope: install::AdmissionScope| {
            crate::services::dsh_control::production::verify_job_admission(
                &gated_env,
                &gated_registry,
                &gated_id,
                &assembled_home,
                scope,
            )
        }));
        manager
    }

    /// 重启模拟（第九轮 P1-2）：同装配内核的**新管理器**——从盘上账本装载
    /// （崩溃遗留的非终态回执进内存查询面，本进程内无在跑任务 = 幽灵遗留）。
    fn fresh_manager(&self) -> Arc<DshPluginManager> {
        Arc::new(Self::assemble_manager(
            &self.env,
            &self.registry,
            &self
                .registry
                .registered(&self.instance_id)
                .expect("重启模拟前置：实例已登记"),
            &self.profile,
            &self.auth,
            false,
        ))
    }

    /// 种一个只读归属核验可扫到的历史会话（生产只读面同形：
    /// `<home>/sessions/<project>/<session_id>/` 首行带 id 的代际日志）。
    fn seed_history_session(&self, session_id: &str) {
        let dir = self.home.join("sessions").join("project").join(session_id);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("session.jsonl"),
            format!("{{\"type\":\"session\",\"id\":\"{session_id}\"}}\n"),
        )
        .unwrap();
    }

    /// 该实例绑定账本落点（auth 域实例作用域 state；撤权墓碑/回填证据面）。
    fn bindings_ledger_file(&self) -> PathBuf {
        auth::instance_state_dir(&self.env, &self.instance_id)
            .expect("夹具实例 ID 恒合法")
            .join("bindings.json")
    }

    /// 该实例管理账本落点（install 域实例作用域 state；崩溃遗留回执面）。
    fn management_ledger_file(&self) -> PathBuf {
        self.env
            .tuvis_home
            .join("dsh")
            .join("instances")
            .join(&self.instance_id)
            .join("management.json")
    }

    fn canonical_home(&self) -> PathBuf {
        std::fs::canonicalize(&self.home).unwrap()
    }
}

/// 夹具只读证据投影（生产 `production::upgrade_evidence_source` 同判据）：盘读
/// 最新 Bound 行（含待落盘撤权标记指名面）+ 真实管理器只读 `status` 安装档
/// ——证据读取零装配副作用（夹具 manager 预装配、无 auth 域触达）。
fn fixture_upgrade_evidence_facts(
    env: &DshEnvironment,
    manager: &Arc<DshPluginManager>,
    instance_id: &str,
) -> UpgradeEvidenceFacts {
    let bound = auth::latest_bound_evidence_for(env, instance_id);
    let (bound_digest, bound_revocation_pending) = match bound {
        Some((digest, pending)) => (Some(digest), pending),
        None => (None, false),
    };
    let installed = !bound_revocation_pending
        && matches!(
            manager.status().map(|status| status.installation),
            Ok(InstallationState::Installed | InstallationState::Conflict)
        );
    UpgradeEvidenceFacts {
        bound_digest,
        bound_revocation_pending,
        installed,
    }
}

/// 失败注入探针的**本文件串行门**（Round-8；第九轮 P1-2 Minor-3 扩面）：
/// `ledger_write_probe` 与 `install::identity_removal_probe` 的全局槽 set 语义都
/// 是替换（非叠加），本文件多个用例跨 await 持有注入——并发跑会互相顶掉判据。
/// 用例开头取锁、全程持有（单线程 tokio 测试运行时不重入该锁，无死锁面）。
async fn probe_serial() -> tokio::sync::MutexGuard<'static, ()> {
    static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    SERIAL.lock().await
}

/// 轮询等待外表断言成立（驱动器 20ms 一拍；20s 上限兜底）。
async fn wait_view(
    service: &Arc<RemoteSwitchService>,
    what: &str,
    pred: impl Fn(&SwitchView) -> bool,
) -> SwitchView {
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    loop {
        let view = service.view().unwrap();
        if pred(&view) {
            return view;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "等待 {what} 超时: {view:?}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// 轮询等待短时握手意图布防到位（进入授权相位与布防分两拍）。
async fn wait_intent_armed(fixture: &Fixture) {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        if fixture.auth.authorization_intent_armed_for_test() {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "等待短时握手意图布防超时（可能授权步已失败）"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// 真实握手客户端（mem 传输；与 dsh_auth_test 同款 hello 形态）。
struct HandshakeClient {
    keys: auth::RustSigningKey,
    conn: mem::MemClient,
}

/// hello 输入快照（P1-1 停点用例把 hello 放进独立任务——快照与夹具生命周期解耦）。
struct HandshakeInputs {
    now_ms: i64,
    server_digest: String,
    epoch: String,
    home: PathBuf,
    instance_id: String,
    host_exe: String,
}

impl HandshakeInputs {
    fn of(fixture: &Fixture) -> Self {
        Self {
            now_ms: fixture.auth.now_ms(),
            server_digest: fixture.auth.key_digest().to_string(),
            epoch: fixture.host_epoch(),
            home: fixture.home.clone(),
            instance_id: fixture.instance_id.clone(),
            host_exe: fixture_host_exe(fixture),
        }
    }
}

impl HandshakeClient {
    async fn connect(factory: &mem::MemClientFactory) -> Self {
        Self {
            keys: auth::RustSigningKey::generate(),
            conn: factory.connect().await,
        }
    }

    /// 固定种子客户端（同钥重连 = 新连接；撤权后旧钥重连探测用）。
    async fn connect_with_seed(factory: &mem::MemClientFactory, seed: &[u8; 32]) -> Self {
        Self {
            keys: auth::RustSigningKey::from_seed(seed),
            conn: factory.connect().await,
        }
    }

    /// 一次真实 hello 往返；返回服务端应答帧。
    async fn hello(
        &self,
        fixture: &Fixture,
        nonce: &[u8; 32],
    ) -> Result<auth::ServerFrame, DshError> {
        let inputs = HandshakeInputs::of(fixture);
        self.hello_with(&inputs, nonce).await
    }

    /// 快照输入版 hello（独立任务内运行——夹具被测试线程继续使用时）。
    async fn hello_with(
        &self,
        inputs: &HandshakeInputs,
        nonce: &[u8; 32],
    ) -> Result<auth::ServerFrame, DshError> {
        let home_binding = auth::HomeBinding {
            home_root: inputs.home.to_string_lossy().into_owned(),
            host_pid: HOST_PID,
            host_started_at_ms: HOST_STARTED_AT,
        };
        let msg = auth::BindingChallengeMessage {
            binding_protocol: auth::BINDING_PROTOCOL,
            key_digests: [
                inputs.server_digest.clone(),
                auth::key_digest(&self.keys.spki),
            ],
            nonce: auth::b64url_encode(nonce),
            host_id: inputs.instance_id.clone(),
            epoch: inputs.epoch.clone(),
            expiry: inputs.now_ms + auth::CHALLENGE_TTL_MS,
            home_binding: home_binding.clone(),
        };
        let signature = self.keys.sign(&msg.to_signing_bytes());
        let hello = auth::ClientFrame::Hello(auth::HelloFrame {
            binding_protocol: auth::BINDING_PROTOCOL,
            public_key_spki: auth::b64url_encode(&self.keys.spki),
            executable: inputs.host_exe.clone(),
            pid: HOST_PID,
            started_at_ms: HOST_STARTED_AT,
            host_id: inputs.instance_id.clone(),
            epoch: inputs.epoch.clone(),
            nonce: auth::b64url_encode(nonce),
            expiry: inputs.now_ms + auth::CHALLENGE_TTL_MS,
            home_binding,
            signature: auth::b64url_encode(&signature),
        });
        self.conn
            .send(&auth::client_frame_line(&hello))
            .await
            .unwrap();
        let line = self.conn.recv().await?;
        auth::parse_server_frame(&line)
    }
}

fn fixture_host_exe(fixture: &Fixture) -> String {
    fixture
        .root
        .join("Applications/DSH.app/Contents/MacOS/DeepSeek Harness")
        .to_string_lossy()
        .into_owned()
}

fn server_state(reply: &auth::ServerFrame) -> String {
    match reply {
        auth::ServerFrame::ServerHello(hello) => hello.state.clone(),
        auth::ServerFrame::Error { error, .. } => format!("error:{error}"),
        _ => "other".to_owned(),
    }
}

// ============================================================
// 用例
// ============================================================

#[tokio::test]
async fn gate_functions_reflect_switch_states() {
    let off = SwitchRecord::default_with(SwitchPhase::Off);
    let failed = SwitchRecord {
        enabled: true,
        phase: SwitchPhase::Failed,
        ..Default::default()
    };
    let waiting = SwitchRecord::default_with(SwitchPhase::WaitingUninstall);
    let ready = SwitchRecord {
        enabled: true,
        phase: SwitchPhase::Ready,
        ..Default::default()
    };
    let enabling = SwitchRecord {
        enabled: true,
        phase: SwitchPhase::Authorizing,
        ..Default::default()
    };
    // 授权激活：只在开关开启的活跃窗口可用。
    assert!(gate_authorization(None).is_err());
    assert!(gate_authorization(Some(&off)).is_err());
    assert!(gate_authorization(Some(&failed)).is_err());
    assert!(gate_authorization(Some(&waiting)).is_err());
    assert!(gate_authorization(Some(&ready)).is_ok());
    assert!(gate_authorization(Some(&enabling)).is_ok());
    // 生命周期命令：开关持有期间拒绝直调；真 Off/记录缺席放行（清理口）；
    // 关闭未收敛（WaitingUninstall/关闭侧 Failed，第九轮 P1-1）一并拒绝。
    assert!(gate_lifecycle(None).is_ok());
    assert!(gate_lifecycle(Some(&off)).is_ok());
    assert!(gate_lifecycle(Some(&failed)).is_ok());
    assert!(gate_lifecycle(Some(&ready)).is_err());
    assert!(gate_lifecycle(Some(&enabling)).is_err());
    // 第九轮 P1-1：等待卸载 = 关闭未收敛（close_pending）→ 直调一律拒绝——不得用
    // 生命周期命令改写关闭流程（直调修复曾把 Failed(Revoke) 洗成可推进态）。
    assert_eq!(
        gate_lifecycle(Some(&waiting)).unwrap_err().code,
        codes::SWITCH_ACTIVE
    );
    // 关闭侧 Failed（enabled=false + failed_step=Revoke/Uninstall）同样拒绝直调。
    let failed_revoke = SwitchRecord {
        enabled: false,
        phase: SwitchPhase::Failed,
        failed_step: Some(SwitchStep::Revoke),
        ..Default::default()
    };
    let failed_uninstall = SwitchRecord {
        enabled: false,
        phase: SwitchPhase::Failed,
        failed_step: Some(SwitchStep::Uninstall),
        ..Default::default()
    };
    assert_eq!(
        gate_lifecycle(Some(&failed_revoke)).unwrap_err().code,
        codes::SWITCH_ACTIVE
    );
    assert_eq!(
        gate_lifecycle(Some(&failed_uninstall)).unwrap_err().code,
        codes::SWITCH_ACTIVE
    );
    // 错误码形态。
    assert_eq!(
        gate_authorization(None).unwrap_err().code,
        codes::SWITCH_OFF
    );
    assert_eq!(
        gate_lifecycle(Some(&ready)).unwrap_err().code,
        codes::SWITCH_ACTIVE
    );
}

/// 预提交门组合语义夹具；不证明生产装配已接入该组合。
fn check_pre_submit_fixture(
    registry: &instances::InstanceRegistry,
    hub: &crate::services::dsh_control::DshControlHub,
    id: &str,
) -> Result<(), DshError> {
    hub.check_switch_gate(id)?;
    if registry
        .selected()?
        .as_ref()
        .map(|instance| instance.instance_id.as_str())
        != Some(id)
    {
        return Err(DshError::new(
            codes::INSTANCE_SWITCHED,
            "instance selection changed",
        ));
    }
    Ok(())
}

/// 选择从 A 切到 B 后，组合门拒绝 A 并允许已开启的 B。
#[test]
fn production_pre_submit_refuses_after_selection_switch() {
    let f = Fixture::new();
    let physical_b = f.root.join("physical/profile-b");
    std::fs::create_dir_all(&physical_b).unwrap();
    std::fs::write(physical_b.join("package.json"), b"{}").unwrap();
    let b = f
        .registry
        .register(HostKind::Desktop, &physical_b, &auth::key_digest(b"gate-b"))
        .unwrap();
    let hub = crate::services::dsh_control::DshControlHub::disabled_arc();
    hub.set_switch_gate(Some(Arc::new(|_| {
        gate_authorization(Some(&SwitchRecord {
            enabled: true,
            phase: SwitchPhase::Ready,
            ..Default::default()
        }))
    })));
    assert!(check_pre_submit_fixture(&f.registry, &hub, &f.instance_id).is_ok());
    f.registry.select(&b.instance_id).unwrap();
    assert_eq!(
        check_pre_submit_fixture(&f.registry, &hub, &f.instance_id)
            .unwrap_err()
            .code,
        codes::INSTANCE_SWITCHED
    );
    assert!(check_pre_submit_fixture(&f.registry, &hub, &b.instance_id).is_ok());
}

/// 选择仍是 A 但开关关闭时，组合门明确拒绝。
#[test]
fn production_pre_submit_refuses_when_switch_closed() {
    let f = Fixture::new();
    let hub = crate::services::dsh_control::DshControlHub::disabled_arc();
    hub.set_switch_gate(Some(Arc::new(|_| {
        gate_authorization(Some(&SwitchRecord::default_with(SwitchPhase::Off)))
    })));
    assert_eq!(
        check_pre_submit_fixture(&f.registry, &hub, &f.instance_id)
            .unwrap_err()
            .code,
        codes::SWITCH_OFF
    );
}

/// P1-2 选择复核核心（生产真判据直测）：仍是当前选择放行；选择切走拒绝；
/// 登记在场但无选择拒绝；真无登记（旧单例域）放行空态。
#[test]
fn pre_submit_selection_check_semantics() {
    let f = Fixture::new();
    assert!(
        crate::services::dsh_control::production::pre_submit_selection_check(
            &f.registry,
            &f.instance_id
        )
        .is_ok()
    );
    assert_eq!(
        crate::services::dsh_control::production::pre_submit_selection_check(
            &f.registry,
            "host-other"
        )
        .unwrap_err()
        .code,
        codes::INSTANCE_SWITCHED
    );
    // 登记在场但无选择（注册不自动选择）→ 需显式选择。
    let temp = tempfile::tempdir().unwrap();
    let physical = temp.path().join("physical/profile-unselected");
    std::fs::create_dir_all(&physical).unwrap();
    std::fs::write(physical.join("package.json"), b"{}").unwrap();
    let reg = instances::InstanceRegistry::open(temp.path().join("instances.json")).unwrap();
    reg.register_candidate(HostKind::Desktop, &physical)
        .unwrap();
    assert_eq!(
        crate::services::dsh_control::production::pre_submit_selection_check(&reg, "host-any")
            .unwrap_err()
            .code,
        codes::TARGET_AMBIGUOUS
    );
    // 真无登记：旧桌面单例兼容域，选择复核空态放行。
    let temp2 = tempfile::tempdir().unwrap();
    let reg_empty = instances::InstanceRegistry::open(temp2.path().join("instances.json")).unwrap();
    assert!(
        crate::services::dsh_control::production::pre_submit_selection_check(
            &reg_empty,
            "host-legacy"
        )
        .is_ok()
    );
}

/// P1-2 装配断言：生产构造默认装预提交门；注入/撤除经 install_pre_submit 往返。
#[test]
fn pre_submit_gate_installed_on_production_channels() {
    assert!(
        crate::services::dsh_control::actions::PeerChannel::for_instance(
            "assembly-probe-pre-submit".to_string()
        )
        .has_pre_submit()
    );
    let channel = crate::services::dsh_control::actions::PeerChannel::production();
    assert!(channel.has_pre_submit());
    channel.install_pre_submit(None);
    assert!(!channel.has_pre_submit());
}

/// 事实就绪的步骤自动跳过：插件在场 + 绑定 Bound → 直接就绪，零 CLI 调用。
#[tokio::test]
async fn enable_auto_skips_ready_steps() {
    let f = Fixture::new();
    f.seed_plugin_installed();
    f.seed_bound_binding();
    let view = f.service.enable().unwrap();
    assert!(view.enabled);
    let view = wait_view(&f.service, "就绪", |v| v.phase == SwitchPhase::Ready).await;
    assert!(view.enabled);
    assert_eq!(
        f.runner.calls.load(Ordering::SeqCst),
        0,
        "不得重复安装/重做授权"
    );
    assert!(!view.rebind_required);
    // 绑定在场但凭据未交付（无真实握手交付）→ 发送面呈「恢复中」。
    assert_eq!(view.send, Some(ChannelReadiness::Recovering));
}

/// 干净实例开启：布防短时意图 → 真实握手自动绑定 → 就绪（无第二次绑定/核对码）。
#[tokio::test]
async fn enable_completes_via_armed_intent() {
    let f = Fixture::new();
    f.seed_plugin_installed();
    f.service.enable().unwrap();
    wait_view(&f.service, "授权中", |v| {
        v.phase == SwitchPhase::Authorizing
    })
    .await;
    wait_intent_armed(&f).await;
    // 插件（新钥）真实握手：意图自动完成绑定。
    let client = HandshakeClient::connect(&f.factory).await;
    let reply = client
        .hello(&f, b"switch-intent-nonce-000000000000")
        .await
        .unwrap();
    assert_eq!(
        server_state(&reply),
        "authorized",
        "意图布防下的新钥握手应自动授权: {reply:?}"
    );
    assert_eq!(f.auth.authorization_state(), AuthorizationState::Bound);
    let view = wait_view(&f.service, "就绪", |v| v.phase == SwitchPhase::Ready).await;
    assert!(view.enabled);
    // 已同意范围：开启意图落在选定实例的本实例级授权上。
    let record = f.service.read().unwrap();
    assert_eq!(record.scope, SWITCH_SCOPE);
}

/// 线性推进（插件缺席）：等待退出 → 安装 → 提示重开 → 核验加载 → 授权 → 就绪。
#[tokio::test]
async fn enable_installs_then_waits_reopen_then_ready() {
    let f = Fixture::new();
    f.service.enable().unwrap();
    // 宿主在场：安装作业停在等待退出（不强杀）。
    wait_view(&f.service, "安装中", |v| {
        v.phase == SwitchPhase::Installing
    })
    .await;
    let view = wait_view(&f.service, "等待退出", |v| {
        v.job
            .as_ref()
            .is_some_and(|j| j.phase == ManagementPhase::WaitingForDesktopExit)
    })
    .await;
    assert!(view.enabled, "等待退出期间开关意图保持开启");
    // 用户退出 DSH → 作业自动继续并完成 → 提示重开。
    f.host.active.store(false, Ordering::SeqCst);
    wait_view(&f.service, "等待重开", |v| {
        v.phase == SwitchPhase::AwaitingReopen
    })
    .await;
    assert!(
        f.runner.calls.load(Ordering::SeqCst) > 0,
        "安装必须真实发生（官方 CLI 被调用）"
    );
    // 用户重开 DSH → 核验插件加载 → 授权。
    f.host.active.store(true, Ordering::SeqCst);
    wait_view(&f.service, "核验插件", |v| {
        v.phase == SwitchPhase::VerifyingPlugin || v.phase == SwitchPhase::Authorizing
    })
    .await;
    wait_view(&f.service, "授权中", |v| {
        v.phase == SwitchPhase::Authorizing
    })
    .await;
    wait_intent_armed(&f).await;
    // 绑定落地 → 就绪。
    f.seed_bound_binding();
    wait_view(&f.service, "就绪", |v| v.phase == SwitchPhase::Ready).await;
}

/// 安装失败留在当前步骤；修复完成后自动接续原流程（重新推进到等待重开）。
#[tokio::test]
async fn install_failure_keeps_step_with_repair_resume() {
    let f = Fixture::new();
    f.runner.fail_add.store(true, Ordering::SeqCst);
    f.service.enable().unwrap();
    f.host.active.store(false, Ordering::SeqCst);
    let view = wait_view(&f.service, "安装失败", |v| {
        v.phase == SwitchPhase::Failed
    })
    .await;
    assert_eq!(view.failed_step, Some(SwitchStep::Install));
    assert_eq!(
        view.reason_code.as_deref(),
        Some(codes::CLI_FAILED),
        "失败步骤必须如实给出原因: {view:?}"
    );
    assert!(view.enabled, "失败时开关意图不变（重试入口接管）");
    // 修复：修复作业也失败 → Failed(repair)；修好后再修复 → 成功接续。
    f.service.repair().unwrap();
    let view = wait_view(&f.service, "修复失败", |v| {
        v.phase == SwitchPhase::Failed
    })
    .await;
    assert_eq!(view.failed_step, Some(SwitchStep::Repair));
    f.runner.fail_add.store(false, Ordering::SeqCst);
    f.service.repair().unwrap();
    let view = wait_view(&f.service, "修复后接续", |v| {
        v.phase == SwitchPhase::AwaitingReopen || v.phase == SwitchPhase::VerifyingPlugin
    })
    .await;
    assert!(
        matches!(
            view.phase,
            SwitchPhase::AwaitingReopen | SwitchPhase::VerifyingPlugin
        ),
        "修复成功必须自动接续原流程: {view:?}"
    );
}

/// 点击关闭后立即失效（撤权/断控制台/清意图）；宿主退出后自动完成卸载。
///
/// P1-4（2026-11-14 用户裁决，推翻第 6 轮「全局吊销接受为已知限制」）：
/// `revoke_local` 的控制台吊销已**按实例收敛**——关闭本实例不再株连其他实例的
/// 网页凭据/活控制台；跨实例不株连的回归见
/// `tests/dsh_console_per_instance_revoke_test.rs`（本夹具单实例测不到该面）。
#[tokio::test]
async fn disable_invalidates_immediately_then_uninstalls() {
    let f = Fixture::new();
    f.seed_plugin_installed();
    f.seed_bound_binding();
    f.service.enable().unwrap();
    wait_view(&f.service, "就绪", |v| v.phase == SwitchPhase::Ready).await;
    let view = f.service.disable().unwrap();
    assert!(!view.enabled);
    assert_eq!(view.phase, SwitchPhase::WaitingUninstall);
    // 立即验证：绑定已撤、意图已清、发送通道未授权。
    assert_eq!(
        f.auth.authorization_state(),
        AuthorizationState::Revoked,
        "同步段必须完成撤权"
    );
    assert!(!f.auth.authorization_intent_armed_for_test());
    assert_eq!(
        view.send,
        Some(ChannelReadiness::NotAuthorized),
        "关闭后发送通道必须立即呈未授权"
    );
    let record = f.service.read().unwrap();
    assert_eq!(record.scope, SWITCH_SCOPE, "关闭记录仍绑定原范围");
    // 宿主仍在：卸载作业等待正常退出（不强杀）。
    wait_view(&f.service, "等待卸载", |v| {
        v.job
            .as_ref()
            .is_some_and(|j| j.kind == ManagementKind::Uninstall)
    })
    .await;
    f.host.active.store(false, Ordering::SeqCst);
    let view = wait_view(&f.service, "关闭完成", |v| v.phase == SwitchPhase::Off).await;
    assert!(!view.enabled);
    assert!(
        f.runner.calls.load(Ordering::SeqCst) > 0,
        "卸载必须真实发生（官方 remove 被调用）"
    );
}

/// 关闭入口必须先拒绝新动作，即使已有提交许可尚未排空。
#[tokio::test(flavor = "multi_thread")]
async fn disable_refuses_new_actions_before_inflight_permit_drains() {
    // 销项复审 Important-7：本用例操纵全局提交边界（持许可、进关闭入口），
    // 与 production 的提交边界状态机用例共用 TEST_BOUNDARY_SERIAL 串行——
    // 不用 probe_serial（那把锁只覆盖本文件的探针槽，盖不住边界全局量）。
    let _serial = crate::services::dsh_control::production::tests::TEST_BOUNDARY_SERIAL
        .lock()
        .await;
    let f = Fixture::new();
    f.seed_plugin_installed();
    f.seed_bound_binding();
    f.service.enable().unwrap();
    wait_view(&f.service, "就绪", |v| v.phase == SwitchPhase::Ready).await;
    f.service.execution_gate().expect("关闭前允许执行");

    let permit = crate::services::dsh_control::production::commit_permit_enter().await;
    let (entered_tx, entered_rx) = std::sync::mpsc::channel();
    *TEST_DISABLE_ENTER.lock().unwrap() = Some((f.instance_id.clone(), entered_tx));
    let service = Arc::clone(&f.service);
    let close = tokio::task::spawn_blocking(move || service.disable());
    let entered = tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if entered_rx.try_recv().is_ok() {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await;
    *TEST_DISABLE_ENTER.lock().unwrap() = None;
    let refusal = tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if let Err(error) = f.service.execution_gate() {
                break error.code;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await;
    // 先释放许可并收尾，再断言，避免失败时遗留全局 flip。
    drop(permit);
    let closed = tokio::time::timeout(Duration::from_secs(5), close).await;
    assert!(entered.is_ok(), "关闭必须进入真实入口");
    assert_eq!(
        refusal.expect("许可排空前执行门须在 1 秒内拒绝"),
        codes::SWITCH_OFF
    );
    assert!(closed.expect("关闭最终完成").unwrap().is_ok());
}

/// 销项复审 P1-2：enable 提交 enabled 后只清除**入口观察到的**关闭纪元——
/// 「观察之后、提交之前」到达的新关闭（更高纪元）不被拆除，关闭拒绝面保持。
/// 旧实现入口无条件清旗（P2-3 撤销的语义），新关闭会被并发开启拆掉。
#[tokio::test(flavor = "multi_thread")]
async fn enable_does_not_clear_newer_closing_intent() {
    let f = Fixture::new();
    // 关闭意图 #1 先落地（disable 首步即 mark_closing；返回本次纪元）。
    let first_epoch = crate::services::dsh_control::production::mark_closing(&f.instance_id);
    // enable 入口「已入口 → 未提交」窗口内确定性插队。
    let (entered_tx, entered_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let release_rx = std::sync::Mutex::new(release_rx);
    arm_test_hooks::set_before_enable(
        &f.instance_id,
        Some(Arc::new(move || {
            let _ = entered_tx.send(());
            let _ = release_rx
                .lock()
                .unwrap()
                .recv_timeout(Duration::from_secs(5));
        })),
    );
    let service = Arc::clone(&f.service);
    let enable = tokio::task::spawn_blocking(move || service.enable());
    let entered = tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if entered_rx.try_recv().is_ok() {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await;
    // 新关闭意图（纪元 #2）在 enable 入口之后到达——不得被这次 enable 清除。
    let second_epoch = crate::services::dsh_control::production::mark_closing(&f.instance_id);
    let _ = release_tx.send(());
    let view = tokio::time::timeout(Duration::from_secs(5), enable).await;
    assert!(entered.is_ok(), "enable 必须到达入口插队窗");
    assert_ne!(first_epoch, second_epoch, "两次 mark_closing 必须推进纪元");
    let view = view
        .expect("enable 收尾")
        .expect("enable 任务未 panic")
        .expect("开启成功");
    assert!(
        view.enabled,
        "旗标在场不阻断本次开启（清除只在提交点条件生效）"
    );
    // 更高纪元的关闭意图必须仍在场：拒绝面保持（旧实现在此失败——被拆）。
    assert!(
        crate::services::dsh_control::production::is_closing(&f.instance_id),
        "enable 不得清除比自己新的关闭意图"
    );
    let refusal = f
        .service
        .execution_gate()
        .expect_err("新关闭意图的拒绝面必须保持");
    assert_eq!(refusal.code, codes::SWITCH_OFF);
    // 清理：拒绝面验完即按纪元退场（同进程并行用例不串扰）。
    assert!(
        crate::services::dsh_control::production::clear_closing_if(&f.instance_id, second_epoch),
        "清理必须按纪元清旗——若失败说明 enable 侧已越权清掉新关闭"
    );
    assert!(
        !crate::services::dsh_control::production::is_closing(&f.instance_id),
        "清理后旗标必须退场"
    );
    arm_test_hooks::set_before_enable(&f.instance_id, None);
    assert!(
        arm_test_hooks::before_enable_keys_empty(),
        "enable 接缝必须随测试清理"
    );
}

/// 销项复审 P1-3（附件上传原子代次边界）：实例代次推进挂接点锁定——
/// ① mark_closing（关闭侧）推进 +1；② **真实 enable 路径**在 `clear_closing_if`
/// 成功后再推进 +1（关闭+重开完整周期 = 两次推进）。关闭前受理的旧附件上传
/// 代次与重开后当前代次必然失配——「旧上传不按新开关状态写盘」的代次面根基。
#[tokio::test(flavor = "multi_thread")]
async fn enable_advances_instance_generation_after_close() {
    use crate::services::dsh_control::production;
    let f = Fixture::new();
    let gen_before = production::instance_generation(&f.instance_id);
    // 关闭侧：disable 首步同款原语（真实 disable 亦经此路径推进代次）。
    let _epoch = production::mark_closing(&f.instance_id);
    let gen_after_close = production::instance_generation(&f.instance_id);
    assert!(
        gen_after_close > gen_before,
        "mark_closing 必须推进实例代次: before={gen_before}, after={gen_after_close}"
    );
    // 真实 enable：入口观察纪元 → 提交 enabled → 按观察值条件清旗 + 推进代次。
    let view = f.service.enable().expect("开启成功");
    assert!(view.enabled);
    assert!(
        !production::is_closing(&f.instance_id),
        "本次 enable 必须按入口观察纪元清旗（无插队关闭）"
    );
    let gen_after_enable = production::instance_generation(&f.instance_id);
    assert!(
        gen_after_enable > gen_after_close,
        "enable 重开必须再推进实例代次（完整周期两次推进）: close={gen_after_close}, enable={gen_after_enable}"
    );
    // 收尾：旗标已由 enable 清除；代次持久单调，不跨用例清理（只增不减）。
}

/// 等待/安装中关闭：上一轮安装作业被取消，卸载自动接续（等待退出后完成）。
#[tokio::test]
async fn disable_while_installing_cancels_and_uninstalls() {
    let f = Fixture::new();
    f.service.enable().unwrap();
    wait_view(&f.service, "安装中", |v| {
        v.phase == SwitchPhase::Installing
    })
    .await;
    f.service.disable().unwrap();
    // 宿主退出 → 安装作业已被取消、卸载完成 → 终态关闭。
    f.host.active.store(false, Ordering::SeqCst);
    let view = wait_view(&f.service, "关闭完成", |v| v.phase == SwitchPhase::Off).await;
    assert!(!view.enabled);
    assert!(
        f.runner.calls.load(Ordering::SeqCst) > 0,
        "卸载 remove 必须被调用"
    );
}

/// 卸载失败：保持远控关闭 + 失败步骤 + 重试入口；重试后完成。
#[tokio::test]
async fn uninstall_failure_keeps_off_with_retry() {
    let f = Fixture::new();
    f.seed_plugin_installed();
    f.seed_bound_binding();
    f.service.enable().unwrap();
    wait_view(&f.service, "就绪", |v| v.phase == SwitchPhase::Ready).await;
    f.runner.fail_remove.store(true, Ordering::SeqCst);
    f.host.active.store(false, Ordering::SeqCst);
    f.service.disable().unwrap();
    let view = wait_view(&f.service, "卸载失败", |v| {
        v.phase == SwitchPhase::Failed
    })
    .await;
    assert!(!view.enabled, "失败时保持远控关闭");
    assert_eq!(view.failed_step, Some(SwitchStep::Uninstall));
    // 重试（再次关闭意图）：修复失败源后卸载完成。
    f.runner.fail_remove.store(false, Ordering::SeqCst);
    f.service.disable().unwrap();
    wait_view(&f.service, "关闭完成", |v| v.phase == SwitchPhase::Off).await;
}

/// 迟到的回执/旧代次不得改写新一代意图：CAS 拒绝，记录保持关闭侧。
#[tokio::test]
async fn late_receipt_cannot_reenable_closed_instance() {
    let f = Fixture::new();
    f.seed_plugin_installed();
    f.service.enable().unwrap();
    let enabling_generation = f.service.read().unwrap().generation;
    assert!(enabling_generation > 0);
    f.service.disable().unwrap();
    let closed = f.service.read().unwrap();
    assert!(!closed.enabled);
    assert_eq!(closed.phase, SwitchPhase::WaitingUninstall);
    // 旧代次的迟到提交（例如迟到的安装成功想把相位推进到就绪）必须被拒。
    let stale = f
        .service
        .commit(enabling_generation, |record| {
            record.enabled = true;
            record.phase = SwitchPhase::Ready;
        })
        .unwrap();
    assert!(stale.is_none(), "旧代次提交必须被 CAS 拒绝");
    let after = f.service.read().unwrap();
    assert!(!after.enabled);
    assert_eq!(after.phase, SwitchPhase::WaitingUninstall);
    assert_eq!(after.generation, closed.generation);
}

/// 等待中切换实例：旧意图暂停（不失败、不推进），不作用于另一实例。
#[tokio::test]
async fn instance_switch_pauses_old_intent() {
    let f = Fixture::new();
    f.seed_plugin_installed();
    f.service.enable().unwrap();
    // 宿主退场：A 停在「等待重开」。
    f.host.active.store(false, Ordering::SeqCst);
    wait_view(&f.service, "等待重开", |v| {
        v.phase == SwitchPhase::AwaitingReopen
    })
    .await;
    // 登记并选择第二实例：A 的开启意图暂停。
    let physical_b = f.root.join("physical/profile-b");
    std::fs::create_dir_all(&physical_b).unwrap();
    std::fs::write(physical_b.join("package.json"), b"{}").unwrap();
    let instance_b = f
        .registry
        .register(
            HostKind::Desktop,
            &physical_b,
            &auth::key_digest(b"switch-fixture-b"),
        )
        .unwrap();
    f.registry.select(&instance_b.instance_id).unwrap();
    for _ in 0..8 {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let record_a = f.service.read().unwrap();
    assert!(
        record_a.enabled && record_a.phase == SwitchPhase::AwaitingReopen,
        "非选定实例的开启意图必须暂停（不失败不推进）: {record_a:?}"
    );
    assert_ne!(record_a.generation, 0);
    let view_a = f.service.view().unwrap();
    assert_eq!(view_a.instance_id, f.instance_id, "view 恒按本实例作用域");
    // 另一实例不残留 A 的任何状态：A 的记录仍在 A 的账本里（绑定 A 的实例目录）。
    assert!(f
        .service
        .record_path
        .to_string_lossy()
        .contains(&f.instance_id));
}

/// 升级恢复：无开关记录 + 插件在场 + 绑定 Bound → 按现有事实恢复「已启用」。
#[tokio::test]
async fn upgrade_restores_switch_from_existing_binding() {
    let f = Fixture::new();
    f.seed_plugin_installed();
    f.seed_bound_binding();
    // 首次读取即派生并落盘，不要求重新授权，也没有任何作业。
    let view = f.service.view().unwrap();
    assert!(view.enabled, "已有合法绑定必须恢复开关: {view:?}");
    assert_eq!(view.phase, SwitchPhase::Ready);
    assert!(
        f.service.read_raw().unwrap().is_some(),
        "派生结果必须持久化"
    );
    assert_eq!(
        f.runner.calls.load(Ordering::SeqCst),
        0,
        "恢复不得误卸载/重装"
    );
    // 再次开启幂等。
    let view = f.service.enable().unwrap();
    assert_eq!(view.phase, SwitchPhase::Ready);
}

/// 身份漂移（登记指纹 ≠ 新钥）进可信重绑面板：布防意图被指纹不连续烧掉、新钥
/// 落待批 → RebindRequired；用户经既有重绑机制确认后到就绪（身份变化不得无提示
/// 信任）。场景 = 撤权墓碑在场但**未经卸载退役**（登记锚仍钉旧钥）——关闭→再
/// 开启的完整循环不进本面板：旧身份随卸载退役（登记锚一并清空），再开启的新钥
/// 是首绑 TOFU（见 `same_identity_dir_cycle_reenable_auto_authorizes`）。
#[tokio::test]
async fn rebind_required_on_identity_drift_then_ready() {
    let f = Fixture::new();
    f.seed_plugin_installed();
    // 既有受信身份 K1：绑定 Bound + 登记锚，再直接撤权（墓碑落账、锚不退役——
    // 未经卸载退役段的中间态）。
    let old_client =
        HandshakeClient::connect_with_seed(&f.factory, b"drift-old-key-seed-0000000000000").await;
    f.registry
        .register(
            HostKind::Desktop,
            &f.profile,
            &auth::key_digest(&old_client.keys.spki),
        )
        .unwrap();
    f.auth.seed_bound_for_test(
        "drift-binding",
        &auth::b64url_encode(&old_client.keys.spki),
        &f.profile.to_string_lossy(),
    );
    f.revoke_bound_directly();
    f.service.enable().unwrap();
    wait_view(&f.service, "授权中", |v| {
        v.phase == SwitchPhase::Authorizing
    })
    .await;
    wait_intent_armed(&f).await;
    // 插件换代新钥 hello：第一次在意图布防下被消费，但登记指纹 ≠ 新钥 →
    // 自动提交失败（错误帧，意图随之烧掉）；插件重试的第二次 hello 落待批
    // → 开关转重绑面板。
    let new_client = HandshakeClient::connect(&f.factory).await;
    let burned = new_client
        .hello(&f, b"drift-newkey-nonce-0000000000000")
        .await
        .unwrap();
    assert!(
        server_state(&burned).starts_with("error:"),
        "identity change must not pass through the armed intent silently: {burned:?}"
    );
    let retried = new_client
        .hello(&f, b"drift-retry-nonce-00000000000000")
        .await
        .unwrap();
    assert_eq!(
        server_state(&retried),
        "pending_approval",
        "意图烧掉后新钥落待批: {retried:?}"
    );
    let view = wait_view(&f.service, "等待重绑确认", |v| {
        v.phase == SwitchPhase::RebindRequired || v.phase == SwitchPhase::Ready
    })
    .await;
    assert_eq!(
        view.phase,
        SwitchPhase::RebindRequired,
        "身份变化不得无提示信任（不得自动批）: {view:?}"
    );
    assert!(view.rebind_required);
    // 用户经既有可信重绑机制确认（expected_old = 登记旧指纹）。
    let registered = f.registry.registered(&f.instance_id).unwrap();
    assert_eq!(
        registered.identity_fingerprint,
        auth::key_digest(&old_client.keys.spki)
    );
    let pending = f.auth.pending_binding().expect("待批新钥在场");
    f.auth
        .approve_rebind(
            &f.instance_id,
            &pending.binding.binding_id,
            &auth::key_digest(&old_client.keys.spki),
        )
        .await
        .unwrap();
    let view = wait_view(&f.service, "重绑后就绪", |v| {
        v.phase == SwitchPhase::Ready
    })
    .await;
    assert!(!view.rebind_required);
    assert_eq!(
        f.registry
            .registered(&f.instance_id)
            .unwrap()
            .identity_fingerprint,
        auth::key_digest(&new_client.keys.spki),
        "重绑后登记指纹必须推进到新钥"
    );
}

/// P1-2 核心回归：**同一份插件身份目录**走完「开启 → 关闭 → 自动卸载 → 再开启」，
/// 全程无手工步骤（测试不删任何文件、无另外点绑定、无核对码），插件客户端最终
/// authorized。
///
/// 真实插件语义（`packages/dsh-remote-access` instanceFileStorage + auth.ts）：
/// 身份（钥 + 本地 revoked 标志）持久在 `<dshHome>/tuvis-remote/<hostId>/` 且
/// **启动会话前**落盘（`generateAndStore()` 先于 connect/handshake）——重启读回；
/// 旧构建卸载不动它 → 关闭撤权后旧钥被「本地 revoked 标志 + 撤权墓碑」双重拒绝，
/// 重装读回旧钥永远连不上。修复：卸载完成段退役身份目录并清空登记锚 → 再开启
/// 装出全新钥（先写身份再握手——复审 Critical 的生命周期保真），经本次开启的
/// 布防意图自动绑定（首绑 TOFU）；死钥判据按**钥域**比对（新钥 ≠ 被撤钥 → 不得
/// 误判，否则无限重装循环）。断言全程恰好一次重装（无循环）。
#[tokio::test]
async fn same_identity_dir_cycle_reenable_auto_authorizes() {
    let f = Fixture::new();
    let identity_file = f.plugin_identity_file();
    // 第一轮：干净开启（插件在场 → 跳过安装；插件启动即落盘身份，先于握手）。
    f.seed_plugin_installed();
    let first =
        HandshakeClient::connect_with_seed(&f.factory, b"cycle-key-seed-00000000000000000").await;
    f.write_plugin_identity(&first.keys.spki);
    assert!(identity_file.exists(), "前置：插件身份先于握手落盘");
    f.service.enable().unwrap();
    wait_view(&f.service, "授权中", |v| {
        v.phase == SwitchPhase::Authorizing
    })
    .await;
    wait_intent_armed(&f).await;
    let reply = first
        .hello(&f, b"cycle-first-nonce-00000000000000")
        .await
        .unwrap();
    assert_eq!(server_state(&reply), "authorized", "首轮新钥自动授权");
    wait_view(&f.service, "就绪", |v| v.phase == SwitchPhase::Ready).await;
    let anchored = f
        .registry
        .registered(&f.instance_id)
        .unwrap()
        .identity_fingerprint;
    assert_eq!(anchored, auth::key_digest(&first.keys.spki));

    // 关闭（撤权 → 墓碑 + 闸门）→ 宿主退出后自动卸载。
    f.service.disable().unwrap();
    f.host.active.store(false, Ordering::SeqCst);
    wait_view(&f.service, "关闭完成", |v| v.phase == SwitchPhase::Off).await;
    assert!(!f.service.read().unwrap().enabled);
    // 卸载完成段必须退役插件身份目录（同钥循环复装的根因修复）。
    assert!(
        !identity_file.exists(),
        "卸载完成必须删除插件身份目录: {}",
        identity_file.display()
    );
    assert!(
        !f.plugin_identity_dir().exists(),
        "身份目录整目录退役（不留残件）"
    );
    // 登记锚随旧钥一并退役（再开启的新钥走首绑，不进重绑面板）。
    assert!(
        f.registry
            .registered(&f.instance_id)
            .unwrap()
            .identity_fingerprint
            .is_empty(),
        "旧钥退役必须清空登记指纹锚"
    );

    // 再开启：插件缺席 → 安装作业（撤权墓碑在证、旧身份已不在场）→ 等待重开。
    f.service.enable().unwrap();
    wait_view(&f.service, "等待重开", |v| {
        v.phase == SwitchPhase::AwaitingReopen
    })
    .await;
    assert_eq!(
        f.runner.adds.load(Ordering::SeqCst),
        1,
        "首轮复装恰好一次 add（seed 不计）"
    );
    // 用户重开 DSH：插件重启读身份 → 身份缺席 → **全新钥**生成并先落盘再握手
    // （真实生命周期）。死钥判据按钥域比对：新钥 ≠ 被撤钥 → 不得路由重装。
    let second = HandshakeClient::connect(&f.factory).await;
    f.write_plugin_identity(&second.keys.spki);
    f.host.active.store(true, Ordering::SeqCst);
    wait_view(&f.service, "授权中", |v| {
        v.phase == SwitchPhase::Authorizing
    })
    .await;
    wait_intent_armed(&f).await;
    let reply = second
        .hello(&f, b"cycle-second-nonce-0000000000000")
        .await
        .unwrap();
    assert_eq!(
        server_state(&reply),
        "authorized",
        "身份退役后新钥必须经本次开启意图自动授权（无另外点绑定/无核对码）: {reply:?}"
    );
    let view = wait_view(&f.service, "就绪", |v| v.phase == SwitchPhase::Ready).await;
    assert!(view.enabled);
    assert!(!view.rebind_required, "再开启不得进重绑面板");
    // 无重装循环（复审 Critical）：新钥在场期间不得再触发安装/退役。
    assert_eq!(
        f.runner.adds.load(Ordering::SeqCst),
        1,
        "新钥身份在场 → 探针必须放行（profile 级判据会误删新钥形成死循环）"
    );
    assert!(identity_file.exists(), "新钥身份必须存活到就绪（未被退役）");
    // 旧钥自恢复仍被拒（墓碑与闸门不因退役松动）。
    let stale =
        HandshakeClient::connect_with_seed(&f.factory, b"cycle-key-seed-00000000000000000").await;
    let reply = stale
        .hello(&f, b"cycle-stale-nonce-00000000000000")
        .await
        .unwrap();
    assert_eq!(
        server_state(&reply),
        "revoked",
        "关闭期间旧钥不得自行恢复: {reply:?}"
    );
}

/// P1-2 升级兼容①：旧构建终态 =「已撤权墓碑 + 旧身份目录仍在场 + 包已移除」，
/// 登记锚仍钉着旧钥。升级后再开启：安装路径必须先退役旧身份（目录删除 + 登记锚
/// 清空）再装新代——新钥经布防意图自动绑定，升级用户不被墙。
#[tokio::test]
async fn upgrade_compat_revoked_tombstone_and_stale_identity_retired_on_reenable() {
    let f = Fixture::new();
    f.seed_plugin_installed();
    // 旧构建状态：绑定 Bound（登记锚钉旧钥）→ 直接撤权（墓碑落账）→ 包移除；
    // 旧身份目录与登记锚原样残留。
    let old_key =
        HandshakeClient::connect_with_seed(&f.factory, b"compat-old-key-seed-000000000000").await;
    f.registry
        .register(
            HostKind::Desktop,
            &f.profile,
            &auth::key_digest(&old_key.keys.spki),
        )
        .unwrap();
    f.auth.seed_bound_for_test(
        "compat-binding",
        &auth::b64url_encode(&old_key.keys.spki),
        &f.profile.to_string_lossy(),
    );
    f.write_plugin_identity(&old_key.keys.spki);
    let identity_file = f.plugin_identity_file();
    f.revoke_bound_directly();
    f.host.active.store(false, Ordering::SeqCst);
    f.remove_plugin_files();
    assert!(
        identity_file.exists(),
        "前置：旧身份目录仍在场（旧构建终态）"
    );

    // 升级后再开启：安装路径先退役旧身份，再装新代。
    f.service.enable().unwrap();
    wait_view(&f.service, "等待重开", |v| {
        v.phase == SwitchPhase::AwaitingReopen
    })
    .await;
    assert!(
        !identity_file.exists(),
        "升级兼容：再开启的安装路径必须退役旧身份目录"
    );
    assert!(
        f.registry
            .registered(&f.instance_id)
            .unwrap()
            .identity_fingerprint
            .is_empty(),
        "升级兼容：旧钥的登记锚必须一并退役"
    );
    // 重开 → 新钥（登记锚已清空）**先落盘身份再握手**（真实生命周期）→ 经布防
    // 意图自动绑定 → 就绪；钥域判据必须放行新钥（不得误删成死循环）。
    let fresh = HandshakeClient::connect(&f.factory).await;
    f.write_plugin_identity(&fresh.keys.spki);
    f.host.active.store(true, Ordering::SeqCst);
    wait_view(&f.service, "授权中", |v| {
        v.phase == SwitchPhase::Authorizing
    })
    .await;
    wait_intent_armed(&f).await;
    let reply = fresh
        .hello(&f, b"compat-fresh-nonce-0000000000000")
        .await
        .unwrap();
    assert_eq!(
        server_state(&reply),
        "authorized",
        "升级用户再开启必须无墙自动授权: {reply:?}"
    );
    wait_view(&f.service, "就绪", |v| v.phase == SwitchPhase::Ready).await;
    assert_eq!(
        f.runner.adds.load(Ordering::SeqCst),
        1,
        "新钥身份在场 → 不得再触发重装（无死循环）"
    );
    assert!(identity_file.exists(), "新钥身份存活到就绪");
}

/// P1-2 升级兼容②：插件仍装着但带死钥（撤权墓碑 + 旧身份在场）——再开启不得
/// 进授权死等（插件本地 revoked 标志拒连，永远等不到），必须路由进安装段：
/// 等待宿主正常退出 → 退役旧身份 → 重装 → 新钥自动绑定。
#[tokio::test]
async fn upgrade_compat_installed_dead_identity_reinstalls_before_reopen() {
    let f = Fixture::new();
    f.seed_plugin_installed();
    let old_key =
        HandshakeClient::connect_with_seed(&f.factory, b"dead-key-seed-000000000000000000").await;
    f.registry
        .register(
            HostKind::Desktop,
            &f.profile,
            &auth::key_digest(&old_key.keys.spki),
        )
        .unwrap();
    f.auth.seed_bound_for_test(
        "dead-binding",
        &auth::b64url_encode(&old_key.keys.spki),
        &f.profile.to_string_lossy(),
    );
    f.write_plugin_identity(&old_key.keys.spki);
    let identity_file = f.plugin_identity_file();
    f.revoke_bound_directly();
    // 宿主在场：插件仍装着（带死钥）。
    f.service.enable().unwrap();
    // 路由进安装段（不是 AwaitingReopen/VerifyingPlugin 的授权死等）。
    wait_view(&f.service, "安装中", |v| {
        v.phase == SwitchPhase::Installing
    })
    .await;
    // 宿主正常退出 → 退役旧身份 + 重装 → 等待重开。
    f.host.active.store(false, Ordering::SeqCst);
    wait_view(&f.service, "等待重开", |v| {
        v.phase == SwitchPhase::AwaitingReopen
    })
    .await;
    assert!(!identity_file.exists(), "重装前必须退役旧身份目录");
    // 重开 → 新钥**先落盘身份再握手**（真实生命周期）→ 自动绑定 → 就绪；
    // 钥域判据必须放行新钥（旧 K1 仍被识别为死钥、新 K2 不得误判）。
    let fresh = HandshakeClient::connect(&f.factory).await;
    f.write_plugin_identity(&fresh.keys.spki);
    f.host.active.store(true, Ordering::SeqCst);
    wait_view(&f.service, "授权中", |v| {
        v.phase == SwitchPhase::Authorizing
    })
    .await;
    wait_intent_armed(&f).await;
    let reply = fresh
        .hello(&f, b"dead-fresh-nonce-000000000000000")
        .await
        .unwrap();
    assert_eq!(
        server_state(&reply),
        "authorized",
        "死钥退役后新钥自动授权: {reply:?}"
    );
    wait_view(&f.service, "就绪", |v| v.phase == SwitchPhase::Ready).await;
    assert_eq!(
        f.runner.adds.load(Ordering::SeqCst),
        1,
        "新钥身份在场 → 不得再触发重装（无死循环）"
    );
    assert!(identity_file.exists(), "新钥身份存活到就绪");
}

/// P1-2 复审补口（Important）+ 复审 Critical 收敛面：死钥识别必须覆盖「等待重开
/// → 重开」过渡，且**收敛**——时点检查（Installed + 宿主未运行 → 等待重开）与
/// 宿主回归不同拍；重开时刻（tick_activation）以同一钥域谓词识别死钥 → 安装段
/// 退役重装；随后插件重启**先落盘全新身份再握手**（真实生命周期 auth.ts:478/648）
/// → 钥域判据放行新钥 → 授权 → 就绪。profile 级判据在此把刚生成的新钥误判成
/// 死钥 → 无限重装循环（Authorizing 永不可达）；钥域判据下全程恰好一次重装。
#[tokio::test]
async fn upgrade_compat_dead_identity_detected_on_reopen_after_awaiting_reopen() {
    let f = Fixture::new();
    f.seed_plugin_installed();
    let old_key =
        HandshakeClient::connect_with_seed(&f.factory, b"reopen-dead-key-seed-00000000000").await;
    f.registry
        .register(
            HostKind::Desktop,
            &f.profile,
            &auth::key_digest(&old_key.keys.spki),
        )
        .unwrap();
    f.auth.seed_bound_for_test(
        "reopen-dead-binding",
        &auth::b64url_encode(&old_key.keys.spki),
        &f.profile.to_string_lossy(),
    );
    f.write_plugin_identity(&old_key.keys.spki);
    let identity_file = f.plugin_identity_file();
    f.revoke_bound_directly();
    // 宿主已退场：时点检查只见「Installed + 未运行」→ 等待重开（本拍不路由安装）。
    f.host.active.store(false, Ordering::SeqCst);
    f.service.enable().unwrap();
    wait_view(&f.service, "等待重开", |v| {
        v.phase == SwitchPhase::AwaitingReopen
    })
    .await;
    assert!(identity_file.exists(), "前置：死钥身份仍在场");
    // 用户重开 DSH：死钥识别必须在重开过渡点命中 → 路由安装段（不是授权死等）。
    // 旧 K1 身份在场 + K1 = 被撤钥 → 钥域判据命中。
    f.host.active.store(true, Ordering::SeqCst);
    wait_view(&f.service, "安装中", |v| {
        v.phase == SwitchPhase::Installing
    })
    .await;
    // 宿主正常退出 → 退役旧身份 + 重装 → 等待重开。
    f.host.active.store(false, Ordering::SeqCst);
    wait_view(&f.service, "等待重开", |v| {
        v.phase == SwitchPhase::AwaitingReopen
    })
    .await;
    assert!(!identity_file.exists(), "重装前必须退役死钥身份");
    assert_eq!(f.runner.adds.load(Ordering::SeqCst), 1, "死钥重装恰好一次");
    // 重开：插件重启生成**全新钥**并先落盘身份再握手（真实生命周期）→
    // 钥域判据放行（新钥 ≠ 被撤钥）→ 授权 → 就绪；全程不得再触发重装。
    let fresh = HandshakeClient::connect(&f.factory).await;
    f.write_plugin_identity(&fresh.keys.spki);
    f.host.active.store(true, Ordering::SeqCst);
    wait_view(&f.service, "授权中", |v| {
        v.phase == SwitchPhase::Authorizing
    })
    .await;
    wait_intent_armed(&f).await;
    let reply = fresh
        .hello(&f, b"reopen-fresh-nonce-0000000000000")
        .await
        .unwrap();
    assert_eq!(
        server_state(&reply),
        "authorized",
        "死钥退役后新钥自动授权: {reply:?}"
    );
    wait_view(&f.service, "就绪", |v| v.phase == SwitchPhase::Ready).await;
    assert_eq!(
        f.runner.adds.load(Ordering::SeqCst),
        1,
        "新钥身份在场 → 不得再触发重装（profile 级判据会形成无限循环）"
    );
    assert!(identity_file.exists(), "新钥身份存活到就绪");
}

/// P1-2 卸载完成段的可重试性：身份目录删除失败 → 卸载**如实报失败**（旧钥仍在
/// 场时绝不谎报成功——重装读回旧钥会被双重拒绝），失败步骤 = 卸载 + 重试入口；
/// 注入解除后重试完成（身份退役 + 关闭终态）。
#[tokio::test]
async fn uninstall_identity_removal_failure_reports_retryable_then_retry_succeeds() {
    // Minor-3：identity_removal_probe 是全局单槽（set 语义 = 替换），与其他注入
    // 用例共持串行门，覆盖完整注入周期（设置 → 跨 await 等待 → 清理）。
    let _serial = probe_serial().await;
    let f = Fixture::new();
    f.seed_plugin_installed();
    f.service.enable().unwrap();
    wait_view(&f.service, "授权中", |v| {
        v.phase == SwitchPhase::Authorizing
    })
    .await;
    wait_intent_armed(&f).await;
    let client = HandshakeClient::connect(&f.factory).await;
    // 插件启动会话前落盘身份（真实生命周期——auth.ts:478/648）。
    f.write_plugin_identity(&client.keys.spki);
    let reply = client
        .hello(&f, b"identity-fail-nonce-000000000000")
        .await
        .unwrap();
    assert_eq!(server_state(&reply), "authorized", "首轮意图自动授权");
    wait_view(&f.service, "就绪", |v| v.phase == SwitchPhase::Ready).await;

    // 注入身份目录删除失败（按本夹具身份目录路径筛选）。
    let probe_dir = f.plugin_identity_dir();
    install::identity_removal_probe::set_fail(Some(Arc::new(move |path: &Path| path == probe_dir)));
    f.service.disable().unwrap();
    f.host.active.store(false, Ordering::SeqCst);
    let view = wait_view(&f.service, "卸载失败", |v| {
        v.phase == SwitchPhase::Failed
    })
    .await;
    install::identity_removal_probe::set_fail(None);
    assert_eq!(view.failed_step, Some(SwitchStep::Uninstall));
    assert_eq!(
        view.reason_code.as_deref(),
        Some(codes::STATE_IO),
        "身份退役失败必须给出真实原因: {view:?}"
    );
    assert!(!view.enabled, "旧钥仍在场时绝不谎报成功（保持关闭侧）");
    assert!(
        f.plugin_identity_file().exists(),
        "删除失败时旧身份必须仍在场（不得假清）"
    );

    // 重试（再次关闭意图）：注入已解除 → 卸载完成（含身份退役）。
    f.service.disable().unwrap();
    wait_view(&f.service, "关闭完成", |v| v.phase == SwitchPhase::Off).await;
    assert!(!f.plugin_identity_file().exists(), "重试必须完成身份退役");
}

/// P1-2 等待期可关闭 + 迟到回执双保险：安装作业停在等待退出时关闭 → 作业被取消、
/// 关闭段照常完成；关闭之后迟到的安装成功回执（作业级）不得把已关闭实例标成启用
/// （执行门 + 代次双保险的记录面保持关闭侧）。
#[tokio::test]
async fn disable_during_install_wait_cancels_job_and_late_receipt_stays_off() {
    let f = Fixture::new();
    f.service.enable().unwrap();
    wait_view(&f.service, "安装中", |v| {
        v.phase == SwitchPhase::Installing
    })
    .await;
    let view = wait_view(&f.service, "等待退出", |v| {
        v.job
            .as_ref()
            .is_some_and(|j| j.phase == ManagementPhase::WaitingForDesktopExit)
    })
    .await;
    let install_job_id = view.job.as_ref().expect("安装作业在场").job_id.clone();
    // 等待退出期间关闭：关闭段先行落盘（enabled=false），卸载自动接续。
    // 宿主先保持在场 → 安装作业停在等待退出、被关闭取消（等待轮询先查取消旗标，
    // 宿主未退出就不会竞越过取消点——取消结果确定）。
    f.service.disable().unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        if f.manager.job(&install_job_id).unwrap().phase == ManagementPhase::Cancelled {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "等待退出中的安装作业必须被关闭取消"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    // 宿主退出 → 卸载完成 → 终态关闭。
    f.host.active.store(false, Ordering::SeqCst);
    let view = wait_view(&f.service, "关闭完成", |v| v.phase == SwitchPhase::Off).await;
    assert!(!view.enabled);
    assert!(
        f.runner.calls.load(Ordering::SeqCst) > 0,
        "卸载 remove 必须被调用"
    );

    // 迟到的安装回执：关闭完成后才成功的安装作业（作业级迟到）不得改写记录。
    let late = f.manager.start(ManagementKind::Install).unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        if f.manager.job(&late).unwrap().phase == ManagementPhase::Succeeded {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "迟到安装作业应完成（宿主已退出）"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    // 记录保持关闭侧：多次读取（跨驱动器拍）均不得出现开启侧相位。
    for _ in 0..10 {
        let record = f.service.read().unwrap();
        assert!(!record.enabled, "迟到安装回执不得激活已关闭实例");
        assert_eq!(record.phase, SwitchPhase::Off);
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let view = f.service.view().unwrap();
    assert!(!view.enabled);
    assert_eq!(view.phase, SwitchPhase::Off);
}

/// 跨兔维斯重启恢复：账本非终态而进程内无本代驱动器 → 首次 view() 重新拉起
/// 驱动器并接续原流程（设置页重开不需要任何额外操作）。
#[tokio::test]
async fn view_respawns_driver_after_restart() {
    let f = Fixture::new();
    f.seed_plugin_installed();
    f.service.enable().unwrap();
    f.host.active.store(false, Ordering::SeqCst);
    wait_view(&f.service, "等待重开", |v| {
        v.phase == SwitchPhase::AwaitingReopen
    })
    .await;
    // 模拟兔维斯重启：代次经 CAS 推进（不经命令壳 → 不 spawn），进程内驱动器
    // 登记清空。旧驱动器随后退场，新代次无驱动器。
    let previous = f.service.read().unwrap();
    let bumped = f
        .service
        .commit(previous.generation, |record| {
            record.generation += 1;
        })
        .unwrap()
        .unwrap();
    drivers().lock().unwrap().clear();
    // 首次 view() 必须复活本代驱动器：宿主回来后继续推进到核验/授权。
    f.service.view().unwrap();
    f.host.active.store(true, Ordering::SeqCst);
    wait_view(&f.service, "重启后接续", |v| {
        v.phase == SwitchPhase::VerifyingPlugin || v.phase == SwitchPhase::Authorizing
    })
    .await;
    let record = f.service.read().unwrap();
    assert_eq!(record.generation, bumped.generation);
}

/// 迟到布防（跨任务复审 Important：Task 2 意图机制 × Task 4 关闭）：
/// 驱动器「已读账本 → 布防未落地」窗口内关闭落账时，布防返回后的复核必须
/// 立即撤销刚布防的意图——否则它会在关闭之后存活一个挑战窗口（60s），把
/// 关闭窗口内的新钥握手自动批准成绑定（记录显示 Off/等待卸载，凭据却活着）。
///
/// 确定性插队：`arm_test_hooks::set_before_arm` 让本代驱动器停在布防前 →
/// 测试完成关闭（cancel[此刻无意图] + 落账换代）→ 放行布防（旧代次视角落地，
/// 用「意图确实armed过」的复核计数证明非空转）→ 复核发现已换代 → 立即撤销。
#[tokio::test(flavor = "multi_thread")]
async fn disable_cancels_intent_armed_during_in_flight_driver_tick() {
    let f = Fixture::new();
    f.seed_plugin_installed();
    let (started_tx, started_rx) = std::sync::mpsc::channel::<()>();
    let (gate_tx, gate_rx) = std::sync::mpsc::channel::<()>();
    let gate_rx = std::sync::Mutex::new(gate_rx);
    // 接缝按实例键注册：并发 lib 单测里其他实例的驱动器查表落空、照常布防。
    super::arm_test_hooks::set_before_arm(
        &f.instance_id,
        Some(Arc::new(move || {
            let _ = started_tx.send(());
            let _ = gate_rx.lock().unwrap().recv();
        })),
    );
    f.service.enable().unwrap();
    // 驱动器已读到账本（gen G / enabled）并确定性停在布防前。
    started_rx
        .recv_timeout(Duration::from_secs(10))
        .expect("驱动器应在布防前到达时序缝");
    let arming_generation = f.service.read().unwrap().generation;
    assert!(arming_generation > 0);

    // 关闭完成（此刻无意图可撤、无绑定可撤权）：gen G+1 / WaitingUninstall。
    let view = f.service.disable().unwrap();
    assert_eq!(view.phase, SwitchPhase::WaitingUninstall);
    assert!(
        !f.auth.authorization_intent_armed_for_test(),
        "关闭返回时（布防尚未落地）意图必须未布防"
    );

    // 放行：布防（旧代次视角）落地 → 布防后复核发现已换代 → 立即撤销。
    let _ = gate_tx.send(());
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        let cancelled = super::arm_test_hooks::stale_arm_cancelled(&f.instance_id);
        let armed = f.auth.authorization_intent_armed_for_test();
        if cancelled && !armed {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "迟到布防未被复核撤销（cancelled={cancelled} armed={armed}）"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    super::arm_test_hooks::set_before_arm(&f.instance_id, None);
    assert!(
        super::arm_test_hooks::before_arm_keys_empty(),
        "布防接缝必须清理干净，泄漏键会毒化后续运行"
    );
    assert!(
        !f.auth.authorization_intent_armed_for_test(),
        "意图不得存活到关闭之后"
    );
    let record = f.service.read().unwrap();
    assert!(!record.enabled, "记录保持关闭侧");
    assert_eq!(record.phase, SwitchPhase::WaitingUninstall);
    assert_ne!(
        record.generation, arming_generation,
        "关闭的换代必须在复核中可见"
    );
}

/// P1-1 验收①（回归 a）+ Round-8 Ruling 9：撤权写盘失败 → 开关 Failed(Revoke)
/// 且 `enabled=false` 先行落盘（执行门立即生效）；**内存面先行且不回滚**——受信
/// 连接被切断、内存账本记 Revoked（已失效是事实，落盘是记账），盘上绑定行按
/// 「可证明未生效」保持 Bound（落盘重试收敛）；重试撤权成功后进等待卸载、宿主
/// 退出后关闭完成。
#[tokio::test]
async fn revoke_write_failure_keeps_switch_off_then_retry_uninstalls() {
    let _serial = probe_serial().await;
    let f = Fixture::new();
    f.seed_plugin_installed();
    f.service.enable().unwrap();
    wait_view(&f.service, "授权中", |v| {
        v.phase == SwitchPhase::Authorizing
    })
    .await;
    wait_intent_armed(&f).await;
    // 真实握手自动绑定（bindings 账本落盘 + 内存一致——撤权写盘失败的「可证明
    // 未生效」分支需要「写入前磁盘快照在场」）。
    let client = HandshakeClient::connect(&f.factory).await;
    let reply = client
        .hello(&f, b"revoke-fail-nonce-00000000000000")
        .await
        .unwrap();
    assert_eq!(server_state(&reply), "authorized", "首轮意图自动授权");
    wait_view(&f.service, "就绪", |v| v.phase == SwitchPhase::Ready).await;
    let ledger_path = auth::instance_state_dir(&f.env, &f.instance_id)
        .unwrap()
        .join("bindings.json");
    assert!(ledger_path.exists(), "绑定账本必须已落盘（撤权目标）");

    // 注入撤权写盘失败（按本夹具账本路径筛选；写前即拒 → 磁盘原样）。
    let probe_path = ledger_path.clone();
    auth::ledger_write_probe::set_fail_write(Some(Arc::new(move |path: &std::path::Path| {
        path == probe_path
    })));

    let view = f.service.disable().unwrap();
    auth::ledger_write_probe::set_fail_write(None);
    assert!(!view.enabled, "关闭意图先行落盘（执行门立即生效）");
    assert_eq!(view.phase, SwitchPhase::Failed);
    assert_eq!(view.failed_step, Some(SwitchStep::Revoke));
    assert_eq!(
        view.reason_code.as_deref(),
        Some(codes::IPC_STATE),
        "失败步骤必须给出撤权写盘失败的真实原因: {view:?}"
    );
    let record = f.service.read().unwrap();
    assert!(!record.enabled);
    assert_eq!(record.phase, SwitchPhase::Failed);
    // Ruling 9：内存面先行且不回滚——受信连接已被切断（插件侧写立即不可用），
    // 内存账本记 Revoked（授权状态如实反映「已失效」）；盘上绑定行按「可证明
    // 未生效」保持 Bound（落盘重试收敛；复活面由开关执行门 fail-closed 兜底）。
    assert!(
        !client.conn.write_available(),
        "撤权写盘失败时受信连接也必须被切断（内存面先行，不因落盘失败回滚）"
    );
    assert_eq!(
        f.auth.authorization_state(),
        AuthorizationState::Revoked,
        "写盘失败保留内存失效态（授权状态如实反映已失效，不回滚）"
    );
    let ledger: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&ledger_path).unwrap()).unwrap();
    assert_eq!(
        ledger["bindings"][0]["state"], "bound",
        "盘上绑定行按「可证明未生效」保持 Bound（重试撤权收敛落盘）"
    );
    // 执行门立即拒绝：发送/附件/控制台共用的执行判定 + 授权提交点判定（含代次面）。
    assert_eq!(
        f.service.execution_gate().unwrap_err().code,
        codes::SWITCH_OFF,
        "发送/附件/控制台执行门必须立即拒绝"
    );
    assert_eq!(
        f.service.authorization_gate_check(None).unwrap_err().code,
        codes::SWITCH_OFF,
        "授权提交点必须立即拒绝（钉钥重连同判）"
    );
    assert_eq!(
        f.service
            .authorization_gate_check(Some(record.generation - 1))
            .unwrap_err()
            .code,
        codes::SWITCH_OFF,
        "关闭前的旧代次布防意图同样拒绝"
    );

    // 重试撤权成功（待写盘收敛）→ 进等待卸载；宿主退出后关闭完成。
    let view = f.service.disable().unwrap();
    assert_eq!(view.phase, SwitchPhase::WaitingUninstall);
    assert!(!view.enabled);
    assert_eq!(
        f.auth.authorization_state(),
        AuthorizationState::Revoked,
        "重试撤权必须成功"
    );
    let ledger: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&ledger_path).unwrap()).unwrap();
    assert_eq!(
        ledger["bindings"][0]["state"], "revoked",
        "重试撤权必须把墓碑落盘（重启后不再复活）"
    );
    f.host.active.store(false, Ordering::SeqCst);
    let view = wait_view(&f.service, "关闭完成", |v| v.phase == SwitchPhase::Off).await;
    assert!(!view.enabled);
    assert!(
        f.runner.calls.load(Ordering::SeqCst) > 0,
        "卸载必须真实发生（官方 remove 被调用）"
    );
}

/// Round-8 Ruling 10（回归 b · 交错面一）：握手已过外层门、提交停驻在**绑定互斥内**
/// （账本写盘前，互斥内门已过）→ disable 必须等在 `binding_mutation` 上
/// （enabled=false 已先行落盘但不完成失效段）；放行后提交落地 → disable 的撤权段
/// 串行其后 → **新鲜绑定被撤走**（不得出现「Off 却 Bound」）。旧实现 disable 不取
/// 绑定互斥：在停驻窗口内直接跑完（看到无绑定 → 跳过撤权）→ 本用例红。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn disable_waits_for_in_flight_commit_then_revokes_fresh_binding() {
    let f = Fixture::new();
    f.seed_plugin_installed();
    f.service.enable().unwrap();
    wait_view(&f.service, "授权中", |v| {
        v.phase == SwitchPhase::Authorizing
    })
    .await;
    wait_intent_armed(&f).await;

    // 提交互斥内停点（按实例键 + 一次性——并发单测透明）。停点线程持
    // binding_mutation，hook 内不得触碰任何授权域锁。
    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let release_rx = std::sync::Mutex::new(release_rx);
    let entered_tx = std::sync::Mutex::new(Some(entered_tx));
    let hook_instance = f.instance_id.clone();
    auth::commit_test_hooks::set_commit_write_pause(
        &hook_instance,
        Some(Arc::new(move || {
            // 一次性停点：sender 只发一次（接缝本身也是一次性，双保险取走即发）。
            if let Some(entered) = entered_tx.lock().unwrap().take() {
                let _ = entered.send(());
            }
            let _ = release_rx.lock().unwrap().recv();
        })),
    );

    // 提交在途：hello → 消费意图 → 过外层门 → 进绑定互斥 → 停在写账本前。
    let factory = Arc::clone(&f.factory);
    let inputs = HandshakeInputs::of(&f);
    let _hello_task = tokio::spawn(async move {
        let client = HandshakeClient::connect(&factory).await;
        client
            .hello_with(&inputs, b"disable-race-nonce-0000000000000")
            .await
    });
    tokio::time::timeout(Duration::from_secs(10), entered_rx)
        .await
        .expect("提交应到达互斥内停点")
        .expect("停点信号送达");

    // disable 放后台：enabled=false 先行落盘，失效段必须等在绑定互斥上。
    let service = Arc::clone(&f.service);
    let (done_tx, done_rx) = tokio::sync::oneshot::channel();
    tokio::task::spawn_blocking(move || {
        let view = service.disable();
        let _ = done_tx.send(view);
    });
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        let off = f
            .service
            .read_raw()
            .unwrap()
            .is_some_and(|record| !record.enabled);
        if off {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "关闭的 enabled=false 先行落盘未到达"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    // 停驻窗口内 disable 不得完成（旧实现此刻已跑完并跳过撤权）。
    let mut done_rx = done_rx;
    let premature = tokio::time::timeout(Duration::from_millis(400), &mut done_rx)
        .await
        .is_ok();
    assert!(
        !premature,
        "提交互斥停驻期间 disable 不得完成（Ruling 10 串行边界）"
    );

    // 放行提交：账本落地 → disable 获得互斥 → 撤权段串行其后 → 新鲜绑定被撤走。
    release_tx.send(()).unwrap();
    let view = tokio::time::timeout(Duration::from_secs(10), &mut done_rx)
        .await
        .expect("放行后 disable 必须完成")
        .expect("disable 有返回")
        .expect("disable 必须成功");
    assert_eq!(
        view.phase,
        SwitchPhase::WaitingUninstall,
        "撤权成功收口: {view:?}"
    );
    assert_eq!(
        f.auth.authorization_state(),
        AuthorizationState::Revoked,
        "disable 的撤权段必须串行在提交落地之后——新鲜绑定必须被撤走（不得 Off 却 Bound）"
    );
    let ledger: serde_json::Value = serde_json::from_slice(
        &std::fs::read(
            auth::instance_state_dir(&f.env, &f.instance_id)
                .unwrap()
                .join("bindings.json"),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(
        ledger["bindings"][0]["state"], "revoked",
        "墓碑必须落盘（提交在途插入的绑定不得存活到关闭之后）"
    );
    auth::commit_test_hooks::set_commit_write_pause(&hook_instance, None);
    assert!(
        !auth::commit_test_hooks::key_armed(&format!("{hook_instance}::commit_write_pause")),
        "提交停点接缝必须清理干净（按本用例的键查——全局空断言会被并发用例的在途 hook 打破）"
    );

    // 收敛面：宿主退出后卸载照常完成（撤权先行的证据前置已由墓碑满足）。
    f.host.active.store(false, Ordering::SeqCst);
    wait_view(&f.service, "关闭完成", |v| v.phase == SwitchPhase::Off).await;
}

/// P1-1 验收②/回归(b)：握手已消费授权意图、尚未写绑定的窗口内关闭完成 →
/// 迟到握手在授权提交点被拒（错误帧 `DSH_SWITCH_OFF`），不产生绑定、绑定账本
/// 字节不变——竞态由提交点核对（enabled + 代次）关闭，而不是靠 disable 抢跑。
#[tokio::test(flavor = "multi_thread")]
async fn late_handshake_refused_at_commit_point_after_disable() {
    let f = Fixture::new();
    f.seed_plugin_installed();
    f.service.enable().unwrap();
    wait_view(&f.service, "授权中", |v| {
        v.phase == SwitchPhase::Authorizing
    })
    .await;
    wait_intent_armed(&f).await;
    let armed_generation = f.service.read().unwrap().generation;

    // 接缝：意图消费后停驻（已取意图、未写绑定）。
    let (paused_tx, paused_rx) = std::sync::mpsc::channel::<()>();
    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
    let release_rx = std::sync::Mutex::new(release_rx);
    let hook_instance = f.instance_id.clone();
    auth::commit_test_hooks::set_after_intent_consumed(
        &hook_instance,
        Some(Arc::new(move || {
            let _ = paused_tx.send(());
            let _ = release_rx.lock().unwrap().recv();
        })),
    );

    // 迟到握手（独立任务）：hello → 消费意图 → 停在提交点。
    let factory = Arc::clone(&f.factory);
    let inputs = HandshakeInputs::of(&f);
    let hello_task = tokio::spawn(async move {
        let client = HandshakeClient::connect(&factory).await;
        client
            .hello_with(&inputs, b"late-commit-nonce-00000000000000")
            .await
    });
    paused_rx
        .recv_timeout(Duration::from_secs(10))
        .expect("握手应到达提交点停点");
    let ledger_path = auth::instance_state_dir(&f.env, &f.instance_id)
        .unwrap()
        .join("bindings.json");
    let ledger_before = std::fs::read(&ledger_path).ok();

    // 停驻窗口内关闭完成（此刻无意图可撤、无绑定可撤权）：gen+1 / enabled=false。
    let view = f.service.disable().unwrap();
    assert_eq!(view.phase, SwitchPhase::WaitingUninstall);
    assert_ne!(
        f.service.read().unwrap().generation,
        armed_generation,
        "关闭必须推进代次"
    );

    // 放行迟到握手：提交点核对（enabled=false）→ 拒绝。
    let _ = release_tx.send(());
    let reply = hello_task
        .await
        .expect("hello 任务不应 panic")
        .expect("服务端应答");
    assert_eq!(
        server_state(&reply),
        "error:DSH_SWITCH_OFF",
        "迟到握手必须在提交点被拒: {reply:?}"
    );
    assert_eq!(
        f.auth.authorization_state(),
        AuthorizationState::Unbound,
        "迟到握手不得产生绑定"
    );
    let ledger_after = std::fs::read(&ledger_path).ok();
    assert_eq!(
        ledger_before, ledger_after,
        "绑定账本字节不得变化（不写绑定）"
    );

    auth::commit_test_hooks::set_after_intent_consumed(&hook_instance, None);
    assert!(
        auth::commit_test_hooks::hooks_empty(),
        "提交点停点接缝必须清理干净"
    );
}

/// Round-8 Ruling 10（回归 b · 交错面二）：握手已过**外层门**、停在绑定互斥之前
/// → disable 完整跑完（此刻提交不持互斥）→ 放行后提交进入 `pin_new_binding` 互斥
/// → **互斥内门**（enabled=false 已落盘）拒绝 `DSH_SWITCH_OFF`，不产生绑定。
/// 锁定「disable 先完成 → 提交后到」交错的拒绝面（Ruling 10 依赖的另一半）。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn commit_past_outer_gate_refused_at_in_mutex_gate_after_disable() {
    let f = Fixture::new();
    f.seed_plugin_installed();
    f.service.enable().unwrap();
    wait_view(&f.service, "授权中", |v| {
        v.phase == SwitchPhase::Authorizing
    })
    .await;
    wait_intent_armed(&f).await;
    let armed_generation = f.service.read().unwrap().generation;

    // 停点：外层门已过、绑定互斥未取（pin_new_binding 调用前）。
    let (paused_tx, paused_rx) = std::sync::mpsc::channel::<()>();
    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
    let release_rx = std::sync::Mutex::new(release_rx);
    let hook_instance = f.instance_id.clone();
    auth::commit_test_hooks::set_before_commit_mutex(
        &hook_instance,
        Some(Arc::new(move || {
            let _ = paused_tx.send(());
            let _ = release_rx.lock().unwrap().recv();
        })),
    );

    let factory = Arc::clone(&f.factory);
    let inputs = HandshakeInputs::of(&f);
    let hello_task = tokio::spawn(async move {
        let client = HandshakeClient::connect(&factory).await;
        client
            .hello_with(&inputs, b"in-mutex-gate-nonce-000000000000")
            .await
    });
    paused_rx
        .recv_timeout(Duration::from_secs(10))
        .expect("提交应到达互斥前停点");
    let ledger_path = auth::instance_state_dir(&f.env, &f.instance_id)
        .unwrap()
        .join("bindings.json");
    let ledger_before = std::fs::read(&ledger_path).ok();

    // disable 完整跑完（提交线程未持互斥）：gen+1 / enabled=false。
    let view = f.service.disable().unwrap();
    assert_eq!(view.phase, SwitchPhase::WaitingUninstall);
    assert_ne!(
        f.service.read().unwrap().generation,
        armed_generation,
        "关闭必须推进代次"
    );

    // 放行：提交进入互斥 → 互斥内门看到关闭 → 拒绝，不写绑定。
    let _ = release_tx.send(());
    let reply = hello_task
        .await
        .expect("hello 任务不应 panic")
        .expect("服务端应答");
    assert_eq!(
        server_state(&reply),
        "error:DSH_SWITCH_OFF",
        "已过外层门的迟到提交必须在互斥内门被拒: {reply:?}"
    );
    assert_eq!(
        f.auth.authorization_state(),
        AuthorizationState::Unbound,
        "迟到提交不得产生绑定"
    );
    let ledger_after = std::fs::read(&ledger_path).ok();
    assert_eq!(
        ledger_before, ledger_after,
        "绑定账本字节不得变化（不写绑定）"
    );

    auth::commit_test_hooks::set_before_commit_mutex(&hook_instance, None);
    assert!(
        !auth::commit_test_hooks::key_armed(&format!("{hook_instance}::before_commit_mutex")),
        "提交点停点接缝必须清理干净（按本用例的键查——全局空断言会被并发用例的在途 hook 打破）"
    );
}

/// Round-8（回归 c）：Failed(Revoke) 态 enable 必须拒绝（撤权从未完成，旧绑定
/// 仍在盘上——放行开启等于让旧钥免撤权重新获得写权限）；唯一出路 = 再次关闭
/// （重试撤权）收敛后按既有流程等待卸载。同时锁定 disable 幂等守卫不吞重试
/// （phase=Failed ≠ Off，重试直跑撤权段）。
#[tokio::test]
async fn failed_revoke_enable_refused_then_retry_disable_converges() {
    let _serial = probe_serial().await;
    let f = Fixture::new();
    f.seed_plugin_installed();
    f.service.enable().unwrap();
    wait_view(&f.service, "授权中", |v| {
        v.phase == SwitchPhase::Authorizing
    })
    .await;
    wait_intent_armed(&f).await;
    let client = HandshakeClient::connect(&f.factory).await;
    let reply = client
        .hello(&f, b"failed-revoke-nonce-000000000000")
        .await
        .unwrap();
    assert_eq!(server_state(&reply), "authorized", "首轮意图自动授权");
    wait_view(&f.service, "就绪", |v| v.phase == SwitchPhase::Ready).await;
    let ledger_path = auth::instance_state_dir(&f.env, &f.instance_id)
        .unwrap()
        .join("bindings.json");

    // 撤权写盘失败 → Failed(Revoke)（盘上绑定行保持 Bound——根因证据面）。
    let probe_path = ledger_path.clone();
    auth::ledger_write_probe::set_fail_write(Some(Arc::new(move |path: &std::path::Path| {
        path == probe_path
    })));
    let view = f.service.disable().unwrap();
    auth::ledger_write_probe::set_fail_write(None);
    assert_eq!(view.phase, SwitchPhase::Failed);
    assert_eq!(view.failed_step, Some(SwitchStep::Revoke));

    // Failed(Revoke) 态 enable 拒绝（专用码）：不得让旧绑定免撤权复活。
    let error = f
        .service
        .enable()
        .expect_err("Failed(Revoke) 态 enable 必须拒绝");
    assert_eq!(
        error.code,
        codes::SWITCH_REVOKE_PENDING,
        "拒绝必须携带专用码（UI 指引「再次关闭以重试」）: {error}"
    );
    let record = f.service.read().unwrap();
    assert!(
        !record.enabled && record.phase == SwitchPhase::Failed,
        "被拒绝的 enable 不得改写关闭侧记录: {record:?}"
    );

    // 重试关闭：幂等守卫不吞（phase=Failed ≠ Off）→ 直跑撤权 → 等待卸载 → 完成。
    let view = f.service.disable().unwrap();
    assert_eq!(
        view.phase,
        SwitchPhase::WaitingUninstall,
        "重试关闭必须直跑撤权段（不得被幂等守卫原样退回）"
    );
    assert_eq!(f.auth.authorization_state(), AuthorizationState::Revoked);
    f.host.active.store(false, Ordering::SeqCst);
    let view = wait_view(&f.service, "关闭完成", |v| v.phase == SwitchPhase::Off).await;
    assert!(!view.enabled);
}

/// Round-8（回归 e）：重启恢复面——Failed(Revoke) 留下的「盘上 Bound 行 + 开关
/// 关闭」跨重启装载后，四类执行门全部拒绝（授权提交点/发送/附件/控制台共用判定
/// + 钉钥重连真实握手被拒）。Ruling 9 的「盘 Bound + 开关关 → 执行门兜底」依赖面。
#[tokio::test]
async fn post_restart_bound_row_with_switch_off_gates_refuse() {
    let _serial = probe_serial().await;
    let f = Fixture::new();
    f.seed_plugin_installed();
    f.service.enable().unwrap();
    wait_view(&f.service, "授权中", |v| {
        v.phase == SwitchPhase::Authorizing
    })
    .await;
    wait_intent_armed(&f).await;
    // 固定种子客户端：重启后同钥重连走钉钥分支（真实握手核对面）。
    let client_seed = [7u8; 32];
    let client = HandshakeClient::connect_with_seed(&f.factory, &client_seed).await;
    let reply = client
        .hello(&f, b"restart-gates-nonce-000000000000")
        .await
        .unwrap();
    assert_eq!(server_state(&reply), "authorized", "首轮意图自动授权");
    wait_view(&f.service, "就绪", |v| v.phase == SwitchPhase::Ready).await;
    let ledger_path = auth::instance_state_dir(&f.env, &f.instance_id)
        .unwrap()
        .join("bindings.json");

    // 撤权写盘失败 → Failed(Revoke)：盘上 Bound 行 + 开关关闭（关闭侧已落盘）。
    let probe_path = ledger_path.clone();
    auth::ledger_write_probe::set_fail_write(Some(Arc::new(move |path: &std::path::Path| {
        path == probe_path
    })));
    f.service.disable().unwrap();
    auth::ledger_write_probe::set_fail_write(None);
    let ledger: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&ledger_path).unwrap()).unwrap();
    assert_eq!(
        ledger["bindings"][0]["state"], "bound",
        "前置：盘上 Bound 行"
    );

    // —— 模拟重启：新授权域 + 新开关服务（同状态目录，盘面即全部事实）——
    let (listener2, factory2) = mem::mem_listener(PeerOsIdentity {
        pid: HOST_PID,
        uid: Some(501),
    });
    let auth2 = Arc::new(
        auth::DshAuthService::new_for_instance((*f.env).clone(), &f.instance_id)
            .unwrap()
            .without_liveness_watcher()
            .with_listener(Box::new(listener2)),
    );
    let services: Arc<SwitchServices> = {
        let manager = Arc::clone(&f.manager);
        let auth2 = Arc::clone(&auth2);
        Arc::new(move || Ok((Arc::clone(&manager), Arc::clone(&auth2))))
    };
    let service2 = Arc::new(
        RemoteSwitchService::new(
            Arc::clone(&f.env),
            Arc::clone(&f.registry),
            f.instance_id.clone(),
            services,
            Arc::clone(&f.evidence_source),
        )
        .unwrap(),
    );
    auth2.set_switch_gate(switch_authorization_gate(Arc::clone(&service2)));

    // 恢复面装载：盘上 Bound 行照实载入（墓碑未落盘——这正是要兜底的形态）。
    assert_eq!(
        auth2.authorization_state(),
        AuthorizationState::Bound,
        "重启后按盘面装载（Bound 行在场）"
    );
    // 四类执行门：发送/附件/控制台共用执行门 + 授权提交点（含代次面）。
    assert_eq!(
        service2.execution_gate().unwrap_err().code,
        codes::SWITCH_OFF,
        "发送/附件/控制台执行门必须拒绝"
    );
    assert_eq!(
        service2.authorization_gate_check(None).unwrap_err().code,
        codes::SWITCH_OFF,
        "授权提交点必须拒绝（钉钥重连同判）"
    );

    // 真实钉钥重连：Bound 行在册也拿不到授权（互斥内门按关闭侧拒绝）。
    auth2.serve().await.expect("重启域服务启动");
    let inputs = HandshakeInputs {
        now_ms: f.auth.now_ms(),
        server_digest: auth2.key_digest().to_string(),
        epoch: f.host_epoch(),
        home: f.home.clone(),
        instance_id: f.instance_id.clone(),
        host_exe: fixture_host_exe(&f),
    };
    let client2 = HandshakeClient::connect_with_seed(&factory2, &client_seed).await;
    let reply = client2
        .hello_with(&inputs, b"restart-reconnect-nonce-00000000")
        .await
        .unwrap();
    assert_eq!(
        server_state(&reply),
        "error:DSH_SWITCH_OFF",
        "重启后钉钥重连必须被授权门拒绝（Bound 行 + 开关关）: {reply:?}"
    );
}

/// P1-1 回归(c)：迟到握手携带旧代次布防意图，关闭→再开启（换代）后恢复 →
/// 提交点核对代次不匹配 → 拒绝（即使开关已再次开启）；新代次意图照常自动授权
/// （无误伤对照）。
#[tokio::test(flavor = "multi_thread")]
async fn stale_generation_handshake_refused_after_reenable() {
    let f = Fixture::new();
    f.seed_plugin_installed();
    f.service.enable().unwrap();
    wait_view(&f.service, "授权中", |v| {
        v.phase == SwitchPhase::Authorizing
    })
    .await;
    wait_intent_armed(&f).await;
    let armed_generation = f.service.read().unwrap().generation;

    let (paused_tx, paused_rx) = std::sync::mpsc::channel::<()>();
    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
    let release_rx = std::sync::Mutex::new(release_rx);
    let hook_instance = f.instance_id.clone();
    auth::commit_test_hooks::set_after_intent_consumed(
        &hook_instance,
        Some(Arc::new(move || {
            let _ = paused_tx.send(());
            let _ = release_rx.lock().unwrap().recv();
        })),
    );

    let factory = Arc::clone(&f.factory);
    let inputs = HandshakeInputs::of(&f);
    let hello_task = tokio::spawn(async move {
        let client = HandshakeClient::connect(&factory).await;
        client
            .hello_with(&inputs, b"stale-gen-nonce-0000000000000000")
            .await
    });
    paused_rx
        .recv_timeout(Duration::from_secs(10))
        .expect("握手应到达提交点停点");

    // 停驻窗口：关闭（卸载完成 → Off）→ 再开启（新代次意图布防）。
    f.service.disable().unwrap();
    f.host.active.store(false, Ordering::SeqCst);
    wait_view(&f.service, "关闭完成", |v| v.phase == SwitchPhase::Off).await;
    f.seed_plugin_installed();
    f.host.active.store(true, Ordering::SeqCst);
    f.service.enable().unwrap();
    wait_view(&f.service, "再授权中", |v| {
        v.phase == SwitchPhase::Authorizing
    })
    .await;
    wait_intent_armed(&f).await;
    let reenabled_generation = f.service.read().unwrap().generation;
    assert!(
        reenabled_generation > armed_generation,
        "再开启必须换代（{reenabled_generation} vs {armed_generation}）"
    );

    // 放行旧代次迟到握手：提交点核对代次 → 拒绝（开关虽已再次开启）。
    let _ = release_tx.send(());
    let reply = hello_task
        .await
        .expect("hello 任务不应 panic")
        .expect("服务端应答");
    assert_eq!(
        server_state(&reply),
        "error:DSH_SWITCH_OFF",
        "旧代次意图的迟到握手必须在提交点被拒: {reply:?}"
    );
    assert_eq!(
        f.auth.authorization_state(),
        AuthorizationState::Unbound,
        "旧代次握手不得产生绑定"
    );

    // 无误伤对照：新代次意图照常自动授权（新钥握手 → authorized）。
    let fresh = HandshakeClient::connect(&f.factory).await;
    let reply = fresh
        .hello(&f, b"fresh-after-reenable-nonce-00000")
        .await
        .unwrap();
    assert_eq!(
        server_state(&reply),
        "authorized",
        "再开启后的新代次授权不受影响: {reply:?}"
    );

    auth::commit_test_hooks::set_after_intent_consumed(&hook_instance, None);
    assert!(
        auth::commit_test_hooks::hooks_empty(),
        "提交点停点接缝必须清理干净"
    );
}

/// P1-1 无记录边界（复审 Important：两个面的分界必须有测试——执行门/授权门
/// 的记录读取内核 = `read_raw` 缺席 → **只读派生**，与 `view()`/`read_locked`
/// 的「派生即落盘」是两条路径，后者已有覆盖、前者此前无覆盖）：
/// * 升级面：无开关记录 + 绑定 Bound + 插件在场 → 门**放行**（升级用户零误伤，
///   不需要先开一次设置页物化记录），且派生**不落盘**；
/// * 新实例面：无记录 + 无绑定 + 无插件 → 门拒绝 `DSH_SWITCH_OFF`，拒绝路径
///   同样不得物化记录。
/// 防回归面：若把「缺席 → 只读派生」改回「缺席 = Off / 派生即落盘」，本用例必红
/// （升级用户假性被关 / 门带副作用）。
#[tokio::test]
async fn gate_no_record_boundary_derives_upgrade_and_refuses_new_instance() {
    // 升级面：无开关记录 + 绑定 Bound + 插件在场 → 放行，且派生不落盘。
    let f = Fixture::new();
    assert!(
        f.service.read_raw().unwrap().is_none(),
        "前置：夹具尚未物化开关记录（不经过 enable/view）"
    );
    f.seed_plugin_installed();
    f.seed_bound_binding();
    f.service
        .execution_gate()
        .expect("升级面（无记录+Bound+插件在场）执行门必须放行");
    f.service
        .authorization_gate_check(None)
        .expect("升级面授权门（钉钥重连）必须放行");
    assert!(
        f.service.read_raw().unwrap().is_none(),
        "门的缺席读取必须只读派生——不得物化记录（不得带落盘副作用）"
    );

    // 新实例面：无记录 + 无绑定 + 无插件 → 拒绝，拒绝路径同样不物化记录。
    let f2 = Fixture::new();
    assert!(
        f2.service.read_raw().unwrap().is_none(),
        "前置：新实例夹具尚未物化开关记录"
    );
    let error = f2
        .service
        .execution_gate()
        .expect_err("新实例面（无记录+无绑定+无插件）执行门必须拒绝");
    assert_eq!(error.code, codes::SWITCH_OFF);
    let error = f2
        .service
        .authorization_gate_check(None)
        .expect_err("新实例面授权门必须拒绝");
    assert_eq!(error.code, codes::SWITCH_OFF);
    assert!(
        f2.service.read_raw().unwrap().is_none(),
        "拒绝路径同样不得物化记录"
    );
}

// ============================================================
// P1-3（Round-7 Task 3）：自动恢复补全两条真实启动路径
//
// * (c) 等待卸载 + 兔维斯重启（新服务实例、同状态目录）+ **全程不查设置页**
//   → 卸载自动继续完成（启动扫描恢复驱动器）；
// * (d) 重启后经生产 select 入口切回已开启实例 → 该实例驱动器在场、推进续跑、
//   发送/控制台就绪恢复；
// * (e) 坏记录（垃圾 remote-switch.json）→ 启动恢复 fail-soft：不 panic、跳过、
//   应用启动面不受影响。
//
// 重启模拟纪律：进程内没有杀任务接缝——以代次更替（驱动器既有退场路径）逼退
// 旧驱动器，等退场计数递增确认后恢复盘面（等价于「重启时刻的磁盘世界」），
// 再以新服务实例 + 启动恢复内核驱动。
// ============================================================

/// 轮询等待开关账本相位断言成立（**只读 `read()`，不调 `view()`**——view 是
/// 次级恢复点，用它等待会掩盖「驱动器没恢复」的回归）。
async fn wait_record(
    service: &Arc<RemoteSwitchService>,
    what: &str,
    pred: impl Fn(&SwitchRecord) -> bool,
) -> SwitchRecord {
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    loop {
        let record = service.read().unwrap();
        if pred(&record) {
            return record;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "等待 {what} 超时: {record:?}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// 轮询等待旧代驱动器退场（测试接缝计数；10s 上限）。
async fn wait_driver_exit(instance_id: &str, baseline: u64) {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        if arm_test_hooks::driver_exit_count(instance_id) > baseline {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "等待旧代驱动器退场超时（代次更替必须逼退驱动器）"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// (c) 等待卸载时重启兔维斯且全程不进设置页 → 卸载自动继续完成。
#[tokio::test]
async fn startup_resume_continues_waiting_uninstall_without_settings_query() {
    let f = Fixture::new();
    f.seed_plugin_installed();
    // 开启（推进中）→ 关闭：等待卸载（宿主在场：卸载作业等宿主退出）。旧驱动器在跑。
    f.service.enable().unwrap();
    let closed = f.service.disable().unwrap();
    assert_eq!(closed.phase, SwitchPhase::WaitingUninstall);
    let generation = f.service.read().unwrap().generation;

    // —— 模拟兔维斯重启：逼退旧驱动器 → 恢复「重启时刻」的盘面 ——
    let record_path = f.service.record_path.clone();
    let snapshot = std::fs::read(&record_path).unwrap();
    let mut bumped: serde_json::Value = serde_json::from_slice(&snapshot).unwrap();
    bumped["generation"] = serde_json::json!(generation + 1);
    std::fs::write(&record_path, serde_json::to_vec(&bumped).unwrap()).unwrap();
    wait_driver_exit(&f.instance_id, 0).await;
    std::fs::write(&record_path, &snapshot).unwrap();
    // 兔维斯停机期间 DSH 宿主正常退出（卸载作业等待的前置消失）。
    f.host.active.store(false, Ordering::SeqCst);

    // 新服务实例（同状态目录、同服务事实面）——「重启后的兔维斯」。
    let restarted = Arc::new(
        RemoteSwitchService::new(
            Arc::clone(&f.env),
            Arc::clone(&f.registry),
            f.instance_id.clone(),
            Arc::clone(&f.services),
            Arc::clone(&f.evidence_source),
        )
        .unwrap()
        .with_poll(Duration::from_millis(20)),
    );

    // 启动扫描给出该实例（P1-3 内核；当前 stub 返回空 → 本用例红）。
    let pending = incomplete_startup_records(&f.env);
    assert!(
        pending.contains(&f.instance_id),
        "启动扫描必须给出非终态记录的实例: {pending:?}"
    );
    // 恢复驱动器（不经 view()/设置页）。
    assert!(restarted.resume_incomplete(), "启动恢复必须拉起本代驱动器");

    // 卸载自动继续完成（只读等待；全程未调 view()）。
    let record = wait_record(&restarted, "卸载完成（Off）", |r| {
        r.phase == SwitchPhase::Off
    })
    .await;
    assert!(!record.enabled);
    assert_eq!(record.generation, generation, "同代驱动器续跑，不得换代");
    assert!(!f.runner.installed.load(Ordering::SeqCst), "插件必须已卸载");
}

/// (d) 重启兔维斯后切回另一已开启实例（生产 select 入口）→ 该实例驱动器在场、
/// 推进续跑、发送/控制台就绪恢复。
#[tokio::test]
async fn select_after_restart_resumes_driver_and_readiness_recovers() {
    let f = Fixture::new();
    f.seed_plugin_installed();
    // 开启推进到「授权中 + 意图已布防」（驱动器在跑）。
    f.service.enable().unwrap();
    wait_view(&f.service, "授权中", |v| {
        v.phase == SwitchPhase::Authorizing
    })
    .await;
    wait_intent_armed(&f).await;
    let snapshot = std::fs::read(&f.service.record_path).unwrap();
    let generation = f.service.read().unwrap().generation;

    // 切到第二实例（离线登记即可——暂停语义只看登记选择面）。
    let physical_b = f.root.join("physical/profile-b");
    std::fs::create_dir_all(&physical_b).unwrap();
    std::fs::write(physical_b.join("package.json"), b"{}").unwrap();
    let instance_b = f
        .registry
        .register(
            HostKind::Desktop,
            &physical_b,
            &auth::key_digest(b"switch-fixture-b"),
        )
        .unwrap();
    f.registry.select(&instance_b.instance_id).unwrap();

    // —— 模拟兔维斯重启：逼退旧驱动器 → 恢复盘面 → 新服务实例 + 启动恢复 ——
    let mut bumped: serde_json::Value = serde_json::from_slice(&snapshot).unwrap();
    bumped["generation"] = serde_json::json!(generation + 1);
    std::fs::write(&f.service.record_path, serde_json::to_vec(&bumped).unwrap()).unwrap();
    wait_driver_exit(&f.instance_id, 0).await;
    std::fs::write(&f.service.record_path, &snapshot).unwrap();
    let restarted = Arc::new(
        RemoteSwitchService::new(
            Arc::clone(&f.env),
            Arc::clone(&f.registry),
            f.instance_id.clone(),
            Arc::clone(&f.services),
            Arc::clone(&f.evidence_source),
        )
        .unwrap()
        .with_poll(Duration::from_millis(20)),
    );
    assert!(
        incomplete_startup_records(&f.env).contains(&f.instance_id),
        "启动扫描必须给出授权中实例"
    );
    assert!(restarted.resume_incomplete(), "启动恢复必须拉起本代驱动器");
    // 非选定实例：驱动器暂停（不推进不失败——意图保留，切回后续跑）。
    tokio::time::sleep(Duration::from_millis(120)).await;
    let paused = restarted.read().unwrap();
    assert_eq!(
        paused.phase,
        SwitchPhase::Authorizing,
        "暂停面如实: {paused:?}"
    );
    assert_eq!(paused.generation, generation, "暂停不得换代");
    // 清掉重启前遗留的布防意图——让「切回后驱动器重新布防」成为可观测事实
    // （否则 wait_intent_armed 会被陈旧意图假阳性骗过）。
    f.auth.cancel_authorization_intent();

    // 生产 select 入口切回 A：恢复闭包 = 生产装配形状（对新服务实例补位）。
    let restart_for_hook = Arc::clone(&restarted);
    let fixture_id = f.instance_id.clone();
    crate::commands::dsh::instance_select_core(
        &f.env,
        &f.registry,
        crate::commands::dsh::DshInstanceRequest {
            instance_id: f.instance_id.clone(),
        },
        &move |id: &str| {
            if id == fixture_id {
                restart_for_hook.resume_incomplete();
            }
        },
    )
    .expect("生产 select 入口必须受理切回");

    // 驱动器在场且续跑：切回后重新布防（陈旧意图已清——armed 只能来自续跑的
    // 驱动器）→ 握手自动授权 → 就绪（只读等待推进）。
    wait_intent_armed(&f).await;
    let client = HandshakeClient::connect(&f.factory).await;
    let reply = client
        .hello(&f, b"select-resume-nonce-000000000000")
        .await
        .unwrap();
    assert_eq!(
        server_state(&reply),
        "authorized",
        "续跑驱动器布防的意图必须自动授权: {reply:?}"
    );
    // 凭据交付（真实 wire；发送面 Ready 的前置——peers 槽位按绑定落位）。
    let auth::ServerFrame::ServerHello(hello) = &reply else {
        panic!("authorized 应答必须是 serverHello: {reply:?}")
    };
    client
        .conn
        .send(&auth::client_frame_line(&auth::ClientFrame::Credential(
            auth::CredentialFrame {
                binding_id: hello
                    .binding_id
                    .clone()
                    .expect("authorized 必带 binding_id"),
                cookie: "fixture-session-cookie".into(),
                browser_cookie: None,
            },
        )))
        .await
        .unwrap();
    let record = wait_record(&restarted, "就绪", |r| r.phase == SwitchPhase::Ready).await;
    assert!(record.enabled);
    assert_eq!(record.generation, generation, "续跑同代，不得换代");
    // 发送/控制台就绪恢复（实时就绪面；经 view 观测——驱动器在场已由推进证明）。
    let view = restarted.view().unwrap();
    assert_eq!(
        view.send,
        Some(ChannelReadiness::Ready),
        "发送通道就绪必须恢复: {view:?}"
    );
    assert_eq!(
        view.console,
        Some(ChannelReadiness::Recovering),
        "控制台通道按凭据交付面如实呈恢复中（Bound 在场）: {view:?}"
    );
}

/// (e) 坏记录（垃圾 remote-switch.json）→ 启动恢复 fail-soft：不 panic、跳过、
/// 应用启动面（登记/发现）不受影响；`resume_incomplete` 不谎报恢复。
#[tokio::test]
async fn corrupt_startup_record_is_fail_soft() {
    let f = Fixture::new();
    std::fs::create_dir_all(f.service.record_path.parent().unwrap()).unwrap();
    std::fs::write(&f.service.record_path, b"\x00 not json garbage {").unwrap();
    // 扫描跳过坏记录（不 panic、不猜相位）。
    let pending = incomplete_startup_records(&f.env);
    assert!(
        !pending.contains(&f.instance_id),
        "坏记录不得进恢复名单: {pending:?}"
    );
    // 恢复面 fail-soft：不 panic、不谎报。
    assert!(!f.service.resume_incomplete());
    // 应用启动面不受影响：登记/发现照常可用。
    assert!(f.registry.registered(&f.instance_id).is_ok());
    // 次级恢复点仍在：设置页 view() 仍可读账本（报 STATE_IO，不吞成 Off）。
    let error = f.service.view().unwrap_err();
    assert_eq!(error.code, codes::STATE_IO);
}

// ============================================================
// P1-2（Round-8 Task 2）：切回已就绪实例恢复监听
//
// 根因（controller 源码实锤）：启动段只为「当时选中」的实例起授权监听
// （lib.rs → production_auth_state → serve）；选择后的恢复钩子
// （`resume_switch_driver_after_select`）只恢复**非终态**开关记录——Ready
// 终态被跳过。于是「A、B 都就绪 → 选 B → 重启兔维斯 → 切回 A」后，A 的插件
// 既无当代 rendezvous 记录也无人 accept——发送/控制台直到重新绑定都死路。
//
// 修复面：选择钩子对已开启（任意相位，含 Ready 终态）的新选实例补起授权服务
// （serve 幂等：bind + accept 循环 + 活体跟踪 + 记录发布；身份/归属核验逐连接
// 不变），fail-soft 不阻塞选择本身。
// ============================================================

async fn credential_ack_after_handshake(closing: bool) {
    use crate::remote::console_lifecycle;
    use crate::services::dsh_control::production;

    let f = Fixture::new();
    f.seed_plugin_installed();
    let client = HandshakeClient::connect(&f.factory).await;
    f.service.enable().unwrap();
    wait_view(&f.service, "授权中", |v| {
        v.phase == SwitchPhase::Authorizing
    })
    .await;
    wait_intent_armed(&f).await;
    let hello = client.hello(&f, &[42; 32]).await.unwrap();
    assert_eq!(server_state(&hello), "authorized");
    let auth::ServerFrame::ServerHello(hello) = hello else {
        panic!("授权应答必须是 ServerHello");
    };
    let binding_id = hello.binding_id.expect("授权应答必须带 binding_id");
    assert!(console_lifecycle::upstream_credential(&f.instance_id).is_none());
    let epoch = closing.then(|| production::mark_closing(&f.instance_id));
    client
        .conn
        .send(&auth::client_frame_line(&auth::ClientFrame::Credential(
            auth::CredentialFrame {
                binding_id: binding_id.clone(),
                cookie: "fixture-session-cookie".into(),
                browser_cookie: Some("browser=fixture".into()),
            },
        )))
        .await
        .unwrap();
    let reply = tokio::time::timeout(Duration::from_secs(5), client.conn.recv()).await;
    let api = console_lifecycle::upstream_credential(&f.instance_id);
    let browser = console_lifecycle::upstream_browser_cookie(&f.instance_id);
    console_lifecycle::clear_upstream_credentials_for(&f.instance_id);
    if let Some(epoch) = epoch {
        assert!(production::clear_closing_if(&f.instance_id, epoch));
    }
    let reply = reply.unwrap().unwrap();
    if closing {
        let auth::ServerFrame::Error { error, .. } = auth::parse_server_frame(&reply).unwrap()
        else {
            panic!("closing instance must reject the credential frame");
        };
        assert_eq!(error, codes::SWITCH_OFF);
    } else {
        let line: serde_json::Value = serde_json::from_str(&reply).unwrap();
        assert_eq!(
            line["event"], "credential_ack",
            "expected credential acknowledgement"
        );
        assert_eq!(line["binding_id"], binding_id);
        assert!(line["eventSignature"]
            .as_str()
            .is_some_and(|s| !s.is_empty()));
    }
    assert_eq!(
        api.as_deref(),
        (!closing).then_some("fixture-session-cookie")
    );
    assert_eq!(browser.as_deref(), (!closing).then_some("browser=fixture"));
}

#[tokio::test]
async fn late_credential_refused_for_closing_instance() {
    credential_ack_after_handshake(true).await;
}

#[tokio::test]
async fn credential_installed_for_open_instance() {
    credential_ack_after_handshake(false).await;
}

/// 轮询等待实例域 rendezvous 记录发布为本代当代（未停用 + epoch 对位）。
async fn wait_rendezvous_published(
    env: &DshEnvironment,
    instance_id: &str,
    expected_epoch: &str,
    what: &str,
) -> auth::RendezvousRecord {
    let path = auth::instance_state_dir(env, instance_id)
        .unwrap()
        .join("rendezvous.json");
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        if let Ok(bytes) = std::fs::read(&path) {
            if let Ok(record) = serde_json::from_slice::<auth::RendezvousRecord>(&bytes) {
                if !record.retired && record.epoch == expected_epoch {
                    return record;
                }
            }
        }
        assert!(
            std::time::Instant::now() < deadline,
            "等待 {what} 超时（监听补位未发生或发布缺席）"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// 验收主场景：A 开启到 Ready → 切到 B → 重启兔维斯（新服务实例、同状态目录、
/// 选中位是 B——启动段不会为 A 起监听）→ 生产 select 入口切回 A → A 无需重新
/// 绑定即恢复：当代 rendezvous 记录发布 + 同钥钉公钥重连（免批面）真实握手
/// authorized + 凭据重交付后发送门 Ready。
#[tokio::test]
async fn select_back_to_ready_instance_restores_listener_and_pinned_reconnect_authorizes() {
    let f = Fixture::new();
    f.seed_plugin_installed();
    // A 开启推进到 Ready：固定种子客户端 = A 插件的钥（真实握手 + 凭据交付链）。
    let client_seed = [11u8; 32];
    let first = HandshakeClient::connect_with_seed(&f.factory, &client_seed).await;
    f.service.enable().unwrap();
    wait_view(&f.service, "授权中", |v| {
        v.phase == SwitchPhase::Authorizing
    })
    .await;
    wait_intent_armed(&f).await;
    let reply = first
        .hello(&f, b"ready-back-bind-nonce-0000000000")
        .await
        .unwrap();
    assert_eq!(server_state(&reply), "authorized", "首轮意图自动授权");
    let auth::ServerFrame::ServerHello(hello) = &reply else {
        panic!("authorized 应答必须是 serverHello: {reply:?}")
    };
    first
        .conn
        .send(&auth::client_frame_line(&auth::ClientFrame::Credential(
            auth::CredentialFrame {
                binding_id: hello
                    .binding_id
                    .clone()
                    .expect("authorized 必带 binding_id"),
                cookie: "fixture-session-cookie".into(),
                browser_cookie: None,
            },
        )))
        .await
        .unwrap();
    wait_record(&f.service, "就绪", |r| r.phase == SwitchPhase::Ready).await;

    // 切到 B（离线登记即可——监听面只看登记选择面）。
    let physical_b = f.root.join("physical/profile-b");
    std::fs::create_dir_all(&physical_b).unwrap();
    std::fs::write(physical_b.join("package.json"), b"{}").unwrap();
    let instance_b = f
        .registry
        .register(
            HostKind::Desktop,
            &physical_b,
            &auth::key_digest(b"switch-fixture-b"),
        )
        .unwrap();
    f.registry.select(&instance_b.instance_id).unwrap();

    // —— 模拟兔维斯重启：逼退旧驱动器 → 恢复盘面 → 新服务实例。A 不在选中位：
    //    启动段（production_auth_state → serve）只为选中实例 B 起监听——A 的
    //    授权域在本代进程内从未装配，正是 P1-2 缺陷场景。旧记录退场：重启后
    //    无进程为 A 维护在场记录，切回后的发布是唯一权威。 ——
    let snapshot = std::fs::read(&f.service.record_path).unwrap();
    let generation = f.service.read().unwrap().generation;
    let mut bumped: serde_json::Value = serde_json::from_slice(&snapshot).unwrap();
    bumped["generation"] = serde_json::json!(generation + 1);
    std::fs::write(&f.service.record_path, serde_json::to_vec(&bumped).unwrap()).unwrap();
    wait_driver_exit(&f.instance_id, 0).await;
    std::fs::write(&f.service.record_path, &snapshot).unwrap();
    let rendezvous_path = auth::instance_state_dir(&f.env, &f.instance_id)
        .unwrap()
        .join("rendezvous.json");
    std::fs::remove_file(&rendezvous_path).ok();

    // 重启域：新授权服务实例（同状态目录——盘上 Bound 行照实装载）+ 新开关服务，
    // **不 serve**（A 非选中，启动段不为它起监听）。
    let (listener2, factory2) = mem::mem_listener(PeerOsIdentity {
        pid: HOST_PID,
        uid: Some(501),
    });
    let auth2 = Arc::new(
        auth::DshAuthService::new_for_instance((*f.env).clone(), &f.instance_id)
            .unwrap()
            .without_liveness_watcher()
            .with_listener(Box::new(listener2)),
    );
    let services2: Arc<SwitchServices> = {
        let manager = Arc::clone(&f.manager);
        let auth2 = Arc::clone(&auth2);
        Arc::new(move || Ok((Arc::clone(&manager), Arc::clone(&auth2))))
    };
    let service2 = Arc::new(
        RemoteSwitchService::new(
            Arc::clone(&f.env),
            Arc::clone(&f.registry),
            f.instance_id.clone(),
            services2,
            Arc::clone(&f.evidence_source),
        )
        .unwrap()
        .with_poll(Duration::from_millis(20)),
    );
    auth2.set_switch_gate(switch_authorization_gate(Arc::clone(&service2)));
    assert_eq!(
        auth2.authorization_state(),
        AuthorizationState::Bound,
        "前置：重启域按盘面装载 Bound 行"
    );
    assert!(
        !rendezvous_path.exists(),
        "前置：重启后 A 无在场记录（监听面只能靠切回补位）"
    );

    // 生产 select 入口切回 A（恢复闭包 = 生产钩子装配形状：驱动器补位 + 监听
    // 补位升级感知内核；装配面按夹具注入——生产单测进程绝不触 production_env
    // 单例）。持久 Ready 记录在场 → 走记录分支（升级证据闭包不被消费）。
    let fixture_id = f.instance_id.clone();
    let hook_service = Arc::clone(&service2);
    let hook_auth = Arc::clone(&auth2);
    crate::commands::dsh::instance_select_core(
        &f.env,
        &f.registry,
        crate::commands::dsh::DshInstanceRequest {
            instance_id: f.instance_id.clone(),
        },
        &move |id: &str| {
            if id != fixture_id {
                return;
            }
            hook_service.resume_incomplete();
            let record_read = hook_service.read_raw();
            let evidence_service = Arc::clone(&hook_service);
            let _ = crate::services::dsh_control::production::ensure_listener_for_selected_switch(
                record_read,
                move || evidence_service.upgrade_recovery_evidence(),
                || Ok(Arc::clone(&hook_auth)),
                |auth| {
                    tokio::spawn(async move {
                        if let Err(e) = auth.serve().await {
                            log::warn!("dsh auth listener bring-up failed: {}", e.detail);
                        }
                    });
                },
            );
        },
    )
    .expect("生产 select 入口必须受理切回");

    // 钉公钥重连（免批面：同钥 + 盘上 Bound 行，无需任何再绑定/再批准）。
    // 真实握手经 mem 监听端——accept 循环缺席时 recv 无应答，超时即监听未补位。
    let inputs = HandshakeInputs {
        now_ms: f.auth.now_ms(),
        server_digest: auth2.key_digest().to_string(),
        epoch: f.host_epoch(),
        home: f.home.clone(),
        instance_id: f.instance_id.clone(),
        host_exe: fixture_host_exe(&f),
    };
    let client2 = HandshakeClient::connect_with_seed(&factory2, &client_seed).await;
    let reply = tokio::time::timeout(
        Duration::from_secs(5),
        client2.hello_with(&inputs, b"ready-back-reconnect-nonce-00000"),
    )
    .await
    .expect("切回后必须补起监听：钉钥重连应答超时（accept 循环缺席）")
    .expect("服务端应答");
    assert_eq!(
        server_state(&reply),
        "authorized",
        "切回后同钥重连必须免批放行（无需重新绑定）: {reply:?}"
    );
    let auth::ServerFrame::ServerHello(reconnect_hello) = &reply else {
        panic!("authorized 应答必须是 serverHello: {reply:?}")
    };
    let binding_id = reconnect_hello
        .binding_id
        .clone()
        .expect("免批重连必须携带既有 binding_id");

    // ② 当代 rendezvous 记录发布（serve 的发布面；epoch = 当代宿主代际）。
    let epoch = f.host_epoch();
    let published =
        wait_rendezvous_published(&f.env, &f.instance_id, &epoch, "切回后记录发布").await;
    assert_eq!(
        auth::key_digest(&auth::b64url_decode(&published.public_key_spki).unwrap()).to_string(),
        auth2.key_digest(),
        "记录必须发布当代信任根"
    );

    // ③ 插件重连链重交付凭据（真实 wire；同一条受信连接）→ 发送门 Ready。
    client2
        .conn
        .send(&auth::client_frame_line(&auth::ClientFrame::Credential(
            auth::CredentialFrame {
                binding_id,
                cookie: "fixture-session-cookie".into(),
                browser_cookie: None,
            },
        )))
        .await
        .unwrap();
    // 凭据在 accept 任务内异步落位（spawn_blocking 交付链）：轮询等待就绪面。
    let view = wait_view(&service2, "发送就绪", |v| {
        v.send == Some(ChannelReadiness::Ready)
    })
    .await;
    assert_eq!(
        view.console,
        Some(ChannelReadiness::Recovering),
        "控制台通道按凭据交付面如实呈恢复中（Bound 在场，网页凭据未交付）: {view:?}"
    );
}

/// 第九轮 P2（升级形态切回）：登记锚 + Bound 行 + 插件安装三证在场、但
/// `remote-switch.json` **从未物化**（开关功能上线前已绑定+安装的升级用户）→
/// 模拟重启后切回该实例，必须按现有迁移证据补起授权监听——不得要求先进设置页
/// 补出记录（view() 的派生面只进内存视图且 Ready 是终态、不拉驱动器），不得
/// 重新绑定，不得二次选择。
///
/// 验收映射（本批主链）：同钥钉公钥重连免批放行（真实 hello authorized）→
/// 当代 rendezvous 记录发布 → API 凭据重交付后发送门 Ready（凭据分轴对照组：
/// 只交 API 凭据 → 控制台如实呈恢复中，明确非 Ready）。
#[tokio::test]
async fn select_back_to_upgrade_form_instance_bringups_listener() {
    let f = Fixture::new();
    f.seed_plugin_installed();
    // 升级形态三证（enable 从未被调）：**盘面**账本 Bound 行（旧构建迁移世界，
    // 固定种子客户端钥 = 免批重连面）+ 同钥摘要的登记锚 + 插件文件在场。
    let client_seed = [21u8; 32];
    let upgrade_key = auth::RustSigningKey::from_seed(&client_seed);
    f.write_bound_row_to_disk("upgrade-binding", &auth::b64url_encode(&upgrade_key.spki));
    f.registry
        .register(
            HostKind::Desktop,
            &f.profile,
            &auth::key_digest(&upgrade_key.spki),
        )
        .expect("升级用户登记锚必须可落账（候选行空指纹 → 锚定）");
    let record_path = f.service.record_path.clone();
    assert!(
        !record_path.exists(),
        "前置：升级形态无开关记录（从未物化）"
    );
    let rendezvous_path = auth::instance_state_dir(&f.env, &f.instance_id)
        .unwrap()
        .join("rendezvous.json");
    assert!(!rendezvous_path.exists(), "前置：升级形态无在场记录");
    assert_eq!(
        auth::pinned_fingerprint_for(&f.env, &f.instance_id),
        Some(auth::key_digest(&upgrade_key.spki)),
        "前置：账本 Bound 行按盘面可核验（与登记锚同钥）"
    );

    // 切到 B（离线登记即可）。
    let physical_b = f.root.join("physical/profile-b");
    std::fs::create_dir_all(&physical_b).unwrap();
    std::fs::write(physical_b.join("package.json"), b"{}").unwrap();
    let instance_b = f
        .registry
        .register(
            HostKind::Desktop,
            &physical_b,
            &auth::key_digest(b"switch-fixture-b"),
        )
        .unwrap();
    f.registry.select(&instance_b.instance_id).unwrap();

    // —— 模拟兔维斯重启：无开关记录 → 无驱动器可逼退；新授权服务实例（同状态
    //    目录，Bound 行照实装载）+ 新开关服务，**不 serve**（A 非选中，启动段
    //    不为它起监听）——升级形态缺陷现场。 ——
    let (listener2, factory2) = mem::mem_listener(PeerOsIdentity {
        pid: HOST_PID,
        uid: Some(501),
    });
    let auth2 = Arc::new(
        auth::DshAuthService::new_for_instance((*f.env).clone(), &f.instance_id)
            .unwrap()
            .without_liveness_watcher()
            .with_listener(Box::new(listener2)),
    );
    let services2: Arc<SwitchServices> = {
        let manager = Arc::clone(&f.manager);
        let auth2 = Arc::clone(&auth2);
        Arc::new(move || Ok((Arc::clone(&manager), Arc::clone(&auth2))))
    };
    let service2 = Arc::new(
        RemoteSwitchService::new(
            Arc::clone(&f.env),
            Arc::clone(&f.registry),
            f.instance_id.clone(),
            services2,
            Arc::clone(&f.evidence_source),
        )
        .unwrap()
        .with_poll(Duration::from_millis(20)),
    );
    auth2.set_switch_gate(switch_authorization_gate(Arc::clone(&service2)));
    assert_eq!(
        auth2.authorization_state(),
        AuthorizationState::Bound,
        "前置：重启域按盘面装载 Bound 行"
    );
    assert!(!record_path.exists(), "前置：重启后升级形态仍无开关记录");

    // 生产 select 入口切回 A（恢复闭包 = 生产钩子装配形状：驱动器补位 + 监听
    // 补位内核；装配面按夹具注入——生产单测进程绝不触 production_env 单例）。
    // 选择计数器：切回必须恰好一次恢复（不二次选择）。
    let selections = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let fixture_id = f.instance_id.clone();
    let hook_service = Arc::clone(&service2);
    let hook_auth = Arc::clone(&auth2);
    let hook_selections = Arc::clone(&selections);
    crate::commands::dsh::instance_select_core(
        &f.env,
        &f.registry,
        crate::commands::dsh::DshInstanceRequest {
            instance_id: f.instance_id.clone(),
        },
        &move |id: &str| {
            if id != fixture_id {
                return;
            }
            hook_selections.fetch_add(1, Ordering::SeqCst);
            hook_service.resume_incomplete();
            let record_read = hook_service.read_raw();
            let evidence_service = Arc::clone(&hook_service);
            let _ = crate::services::dsh_control::production::ensure_listener_for_selected_switch(
                record_read,
                move || evidence_service.upgrade_recovery_evidence(),
                || Ok(Arc::clone(&hook_auth)),
                |auth| {
                    tokio::spawn(async move {
                        if let Err(e) = auth.serve().await {
                            log::warn!("dsh auth listener bring-up failed: {}", e.detail);
                        }
                    });
                },
            );
        },
    )
    .expect("生产 select 入口必须受理切回");
    assert_eq!(
        selections.load(Ordering::SeqCst),
        1,
        "切回必须恰好走一次选择恢复（不二次选择）"
    );

    // 钉公钥重连（免批面：同钥 + 盘上 Bound 行，无需任何再绑定/再批准）。
    // 真实握手经 mem 监听端——accept 循环缺席时 recv 无应答，超时即监听未补位。
    let inputs = HandshakeInputs {
        now_ms: f.auth.now_ms(),
        server_digest: auth2.key_digest().to_string(),
        epoch: f.host_epoch(),
        home: f.home.clone(),
        instance_id: f.instance_id.clone(),
        host_exe: fixture_host_exe(&f),
    };
    let client2 = HandshakeClient::connect_with_seed(&factory2, &client_seed).await;
    let reply = tokio::time::timeout(
        Duration::from_secs(5),
        client2.hello_with(&inputs, b"upgrade-back-reconnect-nonce-000"),
    )
    .await
    .expect("切回后必须按升级证据补起监听：钉钥重连应答超时（accept 循环缺席）")
    .expect("服务端应答");
    assert_eq!(
        server_state(&reply),
        "authorized",
        "升级形态切回后同钥重连必须免批放行（无需重新绑定）: {reply:?}"
    );
    let auth::ServerFrame::ServerHello(reconnect_hello) = &reply else {
        panic!("authorized 应答必须是 serverHello: {reply:?}")
    };
    let binding_id = reconnect_hello
        .binding_id
        .clone()
        .expect("免批重连必须携带既有 binding_id");

    // 当代 rendezvous 记录发布（serve 的发布面；epoch = 当代宿主代际）。
    let epoch = f.host_epoch();
    let published =
        wait_rendezvous_published(&f.env, &f.instance_id, &epoch, "升级形态切回后记录发布").await;
    assert_eq!(
        auth::key_digest(&auth::b64url_decode(&published.public_key_spki).unwrap()).to_string(),
        auth2.key_digest(),
        "记录必须发布当代信任根"
    );
    // 补位面只读派生（不冒充状态）：监听补起本身不得物化开关记录——
    // 派生落盘是设置页 read() 的既有语义，不是恢复面的副作用。
    assert!(
        !record_path.exists(),
        "监听补位不得物化 remote-switch.json（升级恢复只读派生）"
    );

    // 凭据分轴对照组：只交 API 凭据（网页凭据缺席）→ 发送门 Ready；
    // 控制台通道如实呈恢复中（明确非 Ready——凭据未交付不得含糊两可）。
    client2
        .conn
        .send(&auth::client_frame_line(&auth::ClientFrame::Credential(
            auth::CredentialFrame {
                binding_id,
                cookie: "fixture-session-cookie".into(),
                browser_cookie: None,
            },
        )))
        .await
        .unwrap();
    let view = wait_view(&service2, "发送就绪", |v| {
        v.send == Some(ChannelReadiness::Ready)
    })
    .await;
    assert_eq!(
        view.console,
        Some(ChannelReadiness::Recovering),
        "控制台通道按凭据交付面如实呈恢复中（Bound 在场，网页凭据未交付）: {view:?}"
    );
}

/// 升级恢复三证核验矩阵（[`RemoteSwitchService::upgrade_recovery_evidence`] 直测；
/// 「重启后新服务实例」形态——盘面写好后新装配的 auth 域按盘装载，与生产重启
/// 同形）。证据齐 → true；任一证缺席（无 Bound / 锚缺失 / 安装 Missing）→
/// false；**锚漂移**（登记锚与账本 Bound 摘要不一致）→ false（防锚漂移误恢复
/// ——登记指向别的钥时，免批重连也会被指纹门拒收，补起监听只是白装配）。
#[tokio::test]
async fn upgrade_recovery_evidence_requires_three_proofs() {
    /// 重启模拟：盘面之上的新开关服务（同状态目录；不 serve）。授权域**惰性**
    /// 装配（生产形状：首次真正取用才构造）——证据路径本就不触授权域，预装配
    /// 会在「待落盘撤权标记」变体里提前消费标记（真实域构造的合法行为），掩盖
    /// 只读取证面要验证的事实。
    fn restarted_domain(f: &Fixture) -> Arc<RemoteSwitchService> {
        let services2: Arc<SwitchServices> = {
            let env = Arc::clone(&f.env);
            let manager = Arc::clone(&f.manager);
            let id = f.instance_id.clone();
            Arc::new(move || {
                let auth2 = auth::DshAuthService::new_for_instance((*env).clone(), &id)?;
                Ok((Arc::clone(&manager), Arc::new(auth2)))
            })
        };
        Arc::new(
            RemoteSwitchService::new(
                Arc::clone(&f.env),
                Arc::clone(&f.registry),
                f.instance_id.clone(),
                services2,
                Arc::clone(&f.evidence_source),
            )
            .unwrap()
            .with_poll(Duration::from_millis(20)),
        )
    }
    let upgrade_key = auth::RustSigningKey::from_seed(&[51u8; 32]);
    let drifted_key = auth::RustSigningKey::from_seed(&[52u8; 32]);

    // 证据齐：盘面 Bound 行 + 同钥登记锚 + 插件在场 → true。
    let f = Fixture::new();
    f.seed_plugin_installed();
    f.write_bound_row_to_disk("ev-bound", &auth::b64url_encode(&upgrade_key.spki));
    f.registry
        .register(
            HostKind::Desktop,
            &f.profile,
            &auth::key_digest(&upgrade_key.spki),
        )
        .unwrap();
    assert!(
        restarted_domain(&f).upgrade_recovery_evidence(),
        "三证齐（Bound 行 + 安装在场 + 登记锚一致）必须判为升级用户"
    );

    // 无 Bound 行（从未授权）→ false（锚核验对不上任何账本行）。
    let f = Fixture::new();
    f.seed_plugin_installed();
    f.registry
        .register(
            HostKind::Desktop,
            &f.profile,
            &auth::key_digest(&upgrade_key.spki),
        )
        .unwrap();
    assert!(
        !restarted_domain(&f).upgrade_recovery_evidence(),
        "无 Bound 行（从未授权）不得判为升级用户"
    );

    // 锚缺失（有账本行、登记指纹空 = 尚未受信锚定）→ false。
    let f = Fixture::new();
    f.seed_plugin_installed();
    f.write_bound_row_to_disk("ev-no-anchor", &auth::b64url_encode(&upgrade_key.spki));
    assert!(
        !restarted_domain(&f).upgrade_recovery_evidence(),
        "登记锚缺失（空指纹）不得判为升级用户"
    );

    // 安装 Missing（Bound + 锚在场、插件不在场）→ false。
    let f = Fixture::new();
    f.write_bound_row_to_disk("ev-not-installed", &auth::b64url_encode(&upgrade_key.spki));
    f.registry
        .register(
            HostKind::Desktop,
            &f.profile,
            &auth::key_digest(&upgrade_key.spki),
        )
        .unwrap();
    assert!(
        !restarted_domain(&f).upgrade_recovery_evidence(),
        "插件不在场（安装 Missing）不得判为升级用户"
    );

    // 锚漂移（登记锚 ≠ 账本 Bound 摘要）→ false（防锚漂移误恢复）。
    let f = Fixture::new();
    f.seed_plugin_installed();
    f.write_bound_row_to_disk("ev-drift", &auth::b64url_encode(&upgrade_key.spki));
    f.registry
        .register(
            HostKind::Desktop,
            &f.profile,
            &auth::key_digest(&drifted_key.spki),
        )
        .unwrap();
    assert_ne!(
        auth::pinned_fingerprint_for(&f.env, &f.instance_id).unwrap_or_default(),
        f.registry
            .registered(&f.instance_id)
            .unwrap()
            .identity_fingerprint,
        "前置：锚与账本摘要已漂移"
    );
    assert!(
        !restarted_domain(&f).upgrade_recovery_evidence(),
        "锚漂移（登记锚与账本不一致）不得判为升级用户"
    );

    // 待落盘撤权标记在册（Bound + 锚 + 安装都在，但撤权意图未收敛）→ false，
    // 且标记字节原样不动（只读取证绝不消费标记——补刀仍走真实域装配）。
    let f = Fixture::new();
    f.seed_plugin_installed();
    f.write_bound_row_to_disk("ev-pending", &auth::b64url_encode(&upgrade_key.spki));
    f.registry
        .register(
            HostKind::Desktop,
            &f.profile,
            &auth::key_digest(&upgrade_key.spki),
        )
        .unwrap();
    let marker_path = auth::instance_state_dir(&f.env, &f.instance_id)
        .unwrap()
        .join("revoke-pending.json");
    std::fs::create_dir_all(marker_path.parent().unwrap()).unwrap();
    std::fs::write(
        &marker_path,
        serde_json::json!({ "profiles": [f.profile.to_string_lossy()] }).to_string(),
    )
    .unwrap();
    let marker_before = std::fs::read(&marker_path).unwrap();
    assert!(
        !restarted_domain(&f).upgrade_recovery_evidence(),
        "待落盘撤权标记在册（撤权意图未收敛）不得判为升级用户"
    );
    assert_eq!(
        std::fs::read(&marker_path).unwrap(),
        marker_before,
        "升级证据读取不得消费待落盘撤权标记"
    );
}

/// 记录损坏 ≠ 缺席（经生产 select 入口的钩子级负例）：升级三证齐但
/// `remote-switch.json` 是垃圾字节 → 切回**不补起、不装配、不落升级恢复**——
/// 读取失败不是「从未开启」，坏账本不得被升级恢复面洗成已开启（记录字节原样
/// 不动）。
#[tokio::test]
async fn select_back_with_corrupted_record_skips_bringup_and_upgrade_recovery() {
    let f = Fixture::new();
    f.seed_plugin_installed();
    let upgrade_key = auth::RustSigningKey::from_seed(&[53u8; 32]);
    f.write_bound_row_to_disk("corrupt-bound", &auth::b64url_encode(&upgrade_key.spki));
    f.registry
        .register(
            HostKind::Desktop,
            &f.profile,
            &auth::key_digest(&upgrade_key.spki),
        )
        .unwrap();
    // 腐坏记录（升级三证照旧在场——损坏面必须独立于证据面被拒绝）。
    let record_path = f.service.record_path.clone();
    std::fs::create_dir_all(record_path.parent().unwrap()).unwrap();
    std::fs::write(&record_path, b"{not-json-at-all").unwrap();
    let corrupted = std::fs::read(&record_path).unwrap();

    // 切到 B → 重启模拟（新 auth 域/开关服务，不 serve）→ 切回 A。
    let physical_b = f.root.join("physical/profile-b");
    std::fs::create_dir_all(&physical_b).unwrap();
    std::fs::write(physical_b.join("package.json"), b"{}").unwrap();
    let instance_b = f
        .registry
        .register(
            HostKind::Desktop,
            &physical_b,
            &auth::key_digest(b"switch-fixture-b"),
        )
        .unwrap();
    f.registry.select(&instance_b.instance_id).unwrap();
    let (listener2, _factory2) = mem::mem_listener(PeerOsIdentity {
        pid: HOST_PID,
        uid: Some(501),
    });
    let auth2 = Arc::new(
        auth::DshAuthService::new_for_instance((*f.env).clone(), &f.instance_id)
            .unwrap()
            .without_liveness_watcher()
            .with_listener(Box::new(listener2)),
    );
    let services2: Arc<SwitchServices> = {
        let manager = Arc::clone(&f.manager);
        let auth2 = Arc::clone(&auth2);
        Arc::new(move || Ok((Arc::clone(&manager), Arc::clone(&auth2))))
    };
    let service2 = Arc::new(
        RemoteSwitchService::new(
            Arc::clone(&f.env),
            Arc::clone(&f.registry),
            f.instance_id.clone(),
            services2,
            Arc::clone(&f.evidence_source),
        )
        .unwrap()
        .with_poll(Duration::from_millis(20)),
    );
    auth2.set_switch_gate(switch_authorization_gate(Arc::clone(&service2)));

    let served = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let fixture_id = f.instance_id.clone();
    let hook_instance_id = f.instance_id.clone();
    let hook_service = Arc::clone(&service2);
    let hook_auth = Arc::clone(&auth2);
    let hook_served = Arc::clone(&served);
    crate::commands::dsh::instance_select_core(
        &f.env,
        &f.registry,
        crate::commands::dsh::DshInstanceRequest {
            instance_id: fixture_id,
        },
        &move |id: &str| {
            if id != hook_instance_id {
                return;
            }
            // 生产钩子装配形状（夹具面）：腐坏记录路径证据闭包被消费即失败。
            hook_service.resume_incomplete();
            let record_read = hook_service.read_raw();
            assert!(
                record_read.is_err(),
                "前置：腐坏记录读取必须失败（缺席与损坏可区分）"
            );
            let served_slot = Arc::clone(&hook_served);
            let outcome =
                crate::services::dsh_control::production::ensure_listener_for_selected_switch(
                    record_read,
                    move || panic!("记录损坏路径不得消费升级证据闭包（不落升级恢复）"),
                    || Ok(Arc::clone(&hook_auth)),
                    move |_auth| served_slot.store(true, Ordering::SeqCst),
                );
            assert!(
                outcome.as_ref().is_ok_and(|brought| !brought),
                "记录损坏必须不补起（Ok(false)），实际 {outcome:?}"
            );
        },
    )
    .expect("生产 select 入口必须受理切回");
    assert!(!served.load(Ordering::SeqCst), "记录损坏不得发起 serve");
    assert_eq!(
        std::fs::read(&record_path).unwrap(),
        corrupted,
        "升级恢复面不得改写腐坏记录（不落升级恢复）"
    );
}

/// 冷缓存负例（第九轮 P2 复审 Important）：无开关记录、从未绑定（登记锚空）、
/// 私钥文件缺席 + 待落盘撤权标记在册的冷缓存实例被切回 → 升级证据读取必须
/// **零装配副作用**：授权域不构造（装配计数器为 0）、私钥文件不被创建、待落
/// 盘撤权标记原样在册，证据不足时内核装配闭包也不被调。证据读取与生产服务
/// 装配必须分开——`load_or_create_key` / `consume_revoke_pending` 只允许发生
/// 在证据通过后的真实装配（`assemble_auth` → `auth_for_instance`）。
#[tokio::test]
async fn upgrade_evidence_read_never_assembles_auth_domain() {
    let f = Fixture::new();
    f.seed_plugin_installed();
    // 冷缓存：删掉夹具预创建的授权私钥（模拟「授权域从未装配」）。
    let instance_dir = auth::instance_state_dir(&f.env, &f.instance_id).unwrap();
    let key_file = instance_dir.join("authorization.key");
    assert!(key_file.exists(), "前置：夹具预装配已创建私钥文件");
    std::fs::remove_file(&key_file).unwrap();
    // 待落盘撤权标记在册（撤权意图在先；无论证据面结论如何都不得被消费）。
    let marker_path = instance_dir.join("revoke-pending.json");
    std::fs::write(
        &marker_path,
        serde_json::json!({ "profiles": [f.profile.to_string_lossy()] }).to_string(),
    )
    .unwrap();
    let marker_before = std::fs::read(&marker_path).unwrap();
    assert!(
        !f.service.record_path.exists(),
        "前置：无开关记录（切回即触升级证据面）"
    );

    // 生产装配形状的惰性 services：首次取用才构造授权域（= load_or_create_key
    // 创建私钥 + consume_revoke_pending 消费标记），装配计数器监测。
    let assemblies = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let services2: Arc<SwitchServices> = {
        let env = Arc::clone(&f.env);
        let manager = Arc::clone(&f.manager);
        let id = f.instance_id.clone();
        let assemblies_slot = Arc::clone(&assemblies);
        Arc::new(move || {
            assemblies_slot.fetch_add(1, Ordering::SeqCst);
            let auth2 = auth::DshAuthService::new_for_instance((*env).clone(), &id)?;
            Ok((Arc::clone(&manager), Arc::new(auth2)))
        })
    };
    let service2 = Arc::new(
        RemoteSwitchService::new(
            Arc::clone(&f.env),
            Arc::clone(&f.registry),
            f.instance_id.clone(),
            services2,
            Arc::clone(&f.evidence_source),
        )
        .unwrap()
        .with_poll(Duration::from_millis(20)),
    );

    // 经生产 select 入口钩子（内核形状）切回：记录缺席 → 升级证据面 → 证据
    // 不足 → 不补起；装配闭包不被调。
    let assembled = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let outcome: Arc<std::sync::Mutex<Option<Result<bool, DshError>>>> =
        Arc::new(std::sync::Mutex::new(None));
    let fixture_id = f.instance_id.clone();
    let hook_instance_id = f.instance_id.clone();
    let hook_service = Arc::clone(&service2);
    let hook_assembled = Arc::clone(&assembled);
    let hook_outcome = Arc::clone(&outcome);
    crate::commands::dsh::instance_select_core(
        &f.env,
        &f.registry,
        crate::commands::dsh::DshInstanceRequest {
            instance_id: fixture_id,
        },
        &move |id: &str| {
            if id != hook_instance_id {
                return;
            }
            hook_service.resume_incomplete();
            let record_read = hook_service.read_raw();
            let evidence_service = Arc::clone(&hook_service);
            let assembled_slot = Arc::clone(&hook_assembled);
            let result =
                crate::services::dsh_control::production::ensure_listener_for_selected_switch(
                    record_read,
                    move || evidence_service.upgrade_recovery_evidence(),
                    move || {
                        assembled_slot.store(true, Ordering::SeqCst);
                        Err(DshError::new(codes::IPC_STATE, "装配闭包被调即失败"))
                    },
                    move |_auth| panic!("装配闭包返回 Err 后 serve 不得被调"),
                );
            *hook_outcome.lock().unwrap() = Some(result);
        },
    )
    .expect("生产 select 入口必须受理切回");

    // 证据不足 → 不补起、装配闭包不被调。
    let result = outcome
        .lock()
        .unwrap()
        .clone()
        .expect("钩子必须执行（恢复闭包恰好一次）");
    assert!(
        result.as_ref().is_ok_and(|brought| !brought),
        "证据不足（从未绑定 + 登记锚空）不得补起监听: {result:?}"
    );
    assert!(
        !assembled.load(Ordering::SeqCst),
        "证据不足不得装配 auth 域（内核装配闭包不被调）"
    );
    // 证据读取零装配副作用（本批 Important 核心面）。
    assert_eq!(
        assemblies.load(Ordering::SeqCst),
        0,
        "升级证据读取不得经 services 装配授权域（冷缓存构造 = 落钥 + 消费标记）"
    );
    assert!(!key_file.exists(), "升级证据读取不得创建授权私钥文件");
    assert_eq!(
        std::fs::read(&marker_path).unwrap(),
        marker_before,
        "升级证据读取不得消费待落盘撤权标记"
    );
}

/// 标记不可证明 ≠ 无标记（第九轮 P2 复审第二轮 Important）：三证齐备（**非空
/// 登记锚** + 盘面 Bound 行 + 安装在场——证据源本体必然执行到标记读取）但标记
/// 文件**不可读（chmod 000）或损坏 JSON** → 恢复核验必须保守拒绝（读不出的
/// 撤权意图不得当「无待撤权」）：不装配、不 serve、私钥不创建、标记字节原样。
/// 保守口径照 `revocation_settled_for`：仅 `NotFound` 视为无标记，其余读取
/// 错误/解析失败一律不可证明——否则核验通过后的真实装配会经域构造消费掉
/// 畸形标记（畸形删除分支），把「读不出撤权意图」洗成「撤权已收敛」。
#[tokio::test]
async fn upgrade_evidence_rejects_unprovable_revocation_marker() {
    let upgrade_key = auth::RustSigningKey::from_seed(&[54u8; 32]);
    /// 标记不可证明的两形态。
    enum MarkerForm {
        /// 合法内容 + 权限 000（读取错误面）。
        Unreadable,
        /// 损坏 JSON（解析失败面；域构造的畸形删除分支会消费掉它）。
        Corrupted,
    }
    /// 三证齐备 + 冷缓存 + 指定形态的标记（Fixture 随返回值保活）。返回
    /// （fixture, service2, 装配计数器, 私钥路径, 标记路径, 标记原始字节）。
    fn unprovable_marker_world(
        upgrade_key: &auth::RustSigningKey,
        form: MarkerForm,
    ) -> (
        Fixture,
        Arc<RemoteSwitchService>,
        Arc<std::sync::atomic::AtomicUsize>,
        PathBuf,
        PathBuf,
        Vec<u8>,
    ) {
        let f = Fixture::new();
        f.seed_plugin_installed();
        f.write_bound_row_to_disk("ev-marker", &auth::b64url_encode(&upgrade_key.spki));
        f.registry
            .register(
                HostKind::Desktop,
                &f.profile,
                &auth::key_digest(&upgrade_key.spki),
            )
            .expect("登记锚必须可落账（非空锚：证据源本体执行到标记读取）");
        let instance_dir = auth::instance_state_dir(&f.env, &f.instance_id).unwrap();
        let key_file = instance_dir.join("authorization.key");
        assert!(key_file.exists(), "前置：夹具预装配已创建私钥文件");
        std::fs::remove_file(&key_file).unwrap();
        let marker_path = instance_dir.join("revoke-pending.json");
        let valid_body = serde_json::json!({ "profiles": [f.profile.to_string_lossy()] });
        std::fs::write(&marker_path, valid_body.to_string()).unwrap();
        let mut marker_before = std::fs::read(&marker_path).unwrap();
        match form {
            MarkerForm::Unreadable => {
                // 权限面：字节不变，基线沿用合法内容。
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&marker_path, std::fs::Permissions::from_mode(0o000))
                    .expect("标记权限可改");
                // 确定性前置（复审 Minor）：本形态必须真实走到「标记读取失败 →
                // 不可证明」分支——有合法 Bound 行在盘，探针此刻返回 None 只能
                // 是标记读不出（可读且合法时会返回 Some(_, pending=true)）。
                // 特权进程下 chmod 000 仍可读时本断言失败，测试不静默走样。
                assert!(
                    auth::latest_bound_evidence_for(&f.env, &f.instance_id).is_none(),
                    "precondition: unreadable marker must read as unprovable (None), \
                     got Some - the chmod-000 form did not exercise the read-failure branch"
                );
            }
            MarkerForm::Corrupted => {
                // 内容面：改写为损坏 JSON，基线随之更新（断言「改写后的损坏
                // 字节原样」——核验与域构造都不得动它）。
                std::fs::write(&marker_path, b"{\"profiles\": [not-json-at-all")
                    .expect("标记可改写为损坏 JSON");
                marker_before = std::fs::read(&marker_path).unwrap();
            }
        }
        assert!(
            !f.service.record_path.exists(),
            "前置：无开关记录（切回即触升级证据面）"
        );
        let assemblies = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let services2: Arc<SwitchServices> = {
            let env = Arc::clone(&f.env);
            let manager = Arc::clone(&f.manager);
            let id = f.instance_id.clone();
            let assemblies_slot = Arc::clone(&assemblies);
            Arc::new(move || {
                assemblies_slot.fetch_add(1, Ordering::SeqCst);
                let auth2 = auth::DshAuthService::new_for_instance((*env).clone(), &id)?;
                Ok((Arc::clone(&manager), Arc::new(auth2)))
            })
        };
        let service2 = Arc::new(
            RemoteSwitchService::new(
                Arc::clone(&f.env),
                Arc::clone(&f.registry),
                f.instance_id.clone(),
                services2,
                Arc::clone(&f.evidence_source),
            )
            .unwrap()
            .with_poll(Duration::from_millis(20)),
        );
        (
            f,
            service2,
            assemblies,
            key_file,
            marker_path,
            marker_before,
        )
    }
    /// 经生产 select 入口钩子（内核形状）切回并断言零装配副作用 + 核验拒绝。
    fn drive_and_assert(
        f: &Fixture,
        service2: &Arc<RemoteSwitchService>,
        assemblies: &Arc<std::sync::atomic::AtomicUsize>,
        key_file: &Path,
        marker_path: &Path,
        marker_before: &[u8],
    ) {
        let assembled = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let outcome: Arc<std::sync::Mutex<Option<Result<bool, DshError>>>> =
            Arc::new(std::sync::Mutex::new(None));
        let hook_instance_id = f.instance_id.clone();
        let hook_service = Arc::clone(service2);
        let hook_assembled = Arc::clone(&assembled);
        let hook_outcome = Arc::clone(&outcome);
        crate::commands::dsh::instance_select_core(
            &f.env,
            &f.registry,
            crate::commands::dsh::DshInstanceRequest {
                instance_id: f.instance_id.clone(),
            },
            &move |id: &str| {
                if id != hook_instance_id {
                    return;
                }
                hook_service.resume_incomplete();
                let record_read = hook_service.read_raw();
                let evidence_service = Arc::clone(&hook_service);
                let assembled_slot = Arc::clone(&hook_assembled);
                let result =
                    crate::services::dsh_control::production::ensure_listener_for_selected_switch(
                        record_read,
                        move || evidence_service.upgrade_recovery_evidence(),
                        move || {
                            assembled_slot.store(true, Ordering::SeqCst);
                            Err(DshError::new(codes::IPC_STATE, "装配闭包被调即失败"))
                        },
                        move |_auth| panic!("装配闭包返回 Err 后 serve 不得被调"),
                    );
                *hook_outcome.lock().unwrap() = Some(result);
            },
        )
        .expect("生产 select 入口必须受理切回");
        // 核验拒绝面（本批 Important 核心）：标记不可证明 → 零装配副作用。
        assert_eq!(
            assemblies.load(Ordering::SeqCst),
            0,
            "标记不可证明（读取失败/畸形 ≠ 无待撤权）不得经 services 装配授权域"
        );
        assert!(
            !assembled.load(Ordering::SeqCst),
            "标记不可证明时内核装配闭包不得被调"
        );
        assert!(!key_file.exists(), "标记不可证明时不得创建授权私钥文件");
        // 字节原样断言（chmod 000 形态先恢复读权限——断言面要读内容）。
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(marker_path, std::fs::Permissions::from_mode(0o644));
        }
        assert_eq!(
            std::fs::read(marker_path).unwrap(),
            marker_before,
            "标记不可证明时不得消费/改写待落盘撤权标记"
        );
        let result = outcome
            .lock()
            .unwrap()
            .clone()
            .expect("钩子必须执行（恢复闭包恰好一次）");
        assert!(
            result.as_ref().is_ok_and(|brought| !brought),
            "标记不可证明 = 恢复核验必须拒绝（不补起）: {result:?}"
        );
    }
    // 形态一：标记文件不可读（chmod 000——权限错误 ≠ 无标记）。
    let (f, service2, assemblies, key_file, marker_path, marker_before) =
        unprovable_marker_world(&upgrade_key, MarkerForm::Unreadable);
    drive_and_assert(
        &f,
        &service2,
        &assemblies,
        &key_file,
        &marker_path,
        &marker_before,
    );
    // 形态二：标记损坏 JSON（畸形 → 域构造的删除分支会消费掉它——核验必须先拦）。
    let (f, service2, assemblies, key_file, marker_path, marker_before) =
        unprovable_marker_world(&upgrade_key, MarkerForm::Corrupted);
    drive_and_assert(
        &f,
        &service2,
        &assemblies,
        &key_file,
        &marker_path,
        &marker_before,
    );
}

/// 监听补位内核判定矩阵（纯判定面；装配面注入；第九轮 P2 升级恢复感知修订）：
/// 持久 enabled（任意相位，含 Ready 终态）→ 补起（升级证据闭包不被消费）；
/// 关闭侧（enabled=false：Off / 等待卸载 / 关闭侧 Failed）→ 不补起且**不装配**
/// auth 域（证据闭包不被消费）；**记录缺席 + 升级证据齐 → 补起（新行为）**；
/// 记录缺席 + 证据不齐 → 不补起、不装配；**记录损坏（读取失败 ≠ 缺席）→ 不补
/// 起、不装配、不落升级恢复**；auth 域装配失败 → Err 上抛且不补起（fail-soft
/// 由生产钩子落）。
#[tokio::test]
async fn listener_bringup_core_predicate_matrix() {
    /// 升级证据闭包的负控形态：被消费即 panic（断言「证据面不被触达」的路径）。
    fn evidence_must_not_be_consulted() -> bool {
        panic!("该路径不得消费升级证据闭包（记录在场/损坏时证据面无涉）")
    }
    /// 内核直调面（装配/serve 计数注入）。
    fn core(
        record_read: Result<Option<SwitchRecord>, DshError>,
        evidence: impl FnOnce() -> bool,
        auth: Result<Arc<DshAuthService>, DshError>,
        served: &Arc<std::sync::atomic::AtomicBool>,
        assembled: &Arc<std::sync::atomic::AtomicBool>,
    ) -> Result<bool, DshError> {
        let served_slot = Arc::clone(served);
        let assembled_slot = Arc::clone(assembled);
        crate::services::dsh_control::production::ensure_listener_for_selected_switch(
            record_read,
            evidence,
            move || {
                assembled_slot.store(true, Ordering::SeqCst);
                auth
            },
            move |_auth| served_slot.store(true, Ordering::SeqCst),
        )
    }
    let f = Fixture::new();
    let served = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let assembled = Arc::new(std::sync::atomic::AtomicBool::new(false));
    // 记录缺席 + 升级证据齐 → 补起（第九轮 P2 新行为：升级用户不再死路）。
    served.store(false, Ordering::SeqCst);
    assembled.store(false, Ordering::SeqCst);
    assert!(
        core(
            Ok(None),
            || true,
            Ok(Arc::clone(&f.auth)),
            &served,
            &assembled
        )
        .unwrap(),
        "记录缺席 + 升级证据齐必须补起监听"
    );
    assert!(
        served.load(Ordering::SeqCst),
        "记录缺席 + 升级证据齐必须发起 serve"
    );
    assert!(
        assembled.load(Ordering::SeqCst),
        "记录缺席 + 升级证据齐必须装配 auth 域"
    );
    // 记录缺席 + 升级证据不齐 → 不补起、不装配（无谓落钥副作用必须避免）。
    served.store(false, Ordering::SeqCst);
    assembled.store(false, Ordering::SeqCst);
    assert!(
        !core(
            Ok(None),
            || false,
            Ok(Arc::clone(&f.auth)),
            &served,
            &assembled
        )
        .unwrap(),
        "记录缺席 + 证据不齐不得补起监听"
    );
    assert!(
        !served.load(Ordering::SeqCst),
        "记录缺席 + 证据不齐不得发起 serve"
    );
    assert!(
        !assembled.load(Ordering::SeqCst),
        "记录缺席 + 证据不齐不得装配 auth 域"
    );
    // 记录损坏（读取失败）→ 不补起、不装配、**不落升级恢复**（坏账本不得被
    // 升级恢复面洗成「已开启」——证据闭包被消费即失败）。
    served.store(false, Ordering::SeqCst);
    assembled.store(false, Ordering::SeqCst);
    assert!(
        !core(
            Err(DshError::new(codes::STATE_IO, "corrupted switch record")),
            evidence_must_not_be_consulted,
            Ok(Arc::clone(&f.auth)),
            &served,
            &assembled
        )
        .unwrap(),
        "记录损坏不得补起监听"
    );
    assert!(!served.load(Ordering::SeqCst), "记录损坏不得发起 serve");
    assert!(
        !assembled.load(Ordering::SeqCst),
        "记录损坏不得装配 auth 域"
    );
    // 关闭面（enabled=false）→ 不补起、不装配（证据闭包不被消费）：Off /
    // 等待卸载 / 关闭侧 Failed(Revoke)。
    for (what, record) in [
        ("Off", SwitchRecord::default_with(SwitchPhase::Off)),
        (
            "等待卸载",
            SwitchRecord::default_with(SwitchPhase::WaitingUninstall),
        ),
        (
            "关闭侧 Failed(Revoke)",
            SwitchRecord {
                phase: SwitchPhase::Failed,
                failed_step: Some(SwitchStep::Revoke),
                ..Default::default()
            },
        ),
    ] {
        served.store(false, Ordering::SeqCst);
        assembled.store(false, Ordering::SeqCst);
        assert!(
            !core(
                Ok(Some(record)),
                evidence_must_not_be_consulted,
                Ok(Arc::clone(&f.auth)),
                &served,
                &assembled
            )
            .unwrap(),
            "{what} 不得补起监听"
        );
        assert!(
            !served.load(Ordering::SeqCst),
            "{what} 不得发起 serve（关闭面 = 不起监听、不发布记录）"
        );
        assert!(
            !assembled.load(Ordering::SeqCst),
            "{what} 不得装配 auth 域（未开启实例的单纯选择不落钥）"
        );
    }
    // 开启面（enabled=true，任意相位）→ 补起（证据闭包不被消费）：Ready 终态
    // + 推进中。
    for (what, record) in [
        (
            "Ready 终态",
            SwitchRecord {
                phase: SwitchPhase::Ready,
                ..Default::default()
            },
        ),
        (
            "授权中",
            SwitchRecord {
                phase: SwitchPhase::Authorizing,
                ..Default::default()
            },
        ),
    ] {
        let record = SwitchRecord {
            enabled: true,
            ..record
        };
        served.store(false, Ordering::SeqCst);
        assembled.store(false, Ordering::SeqCst);
        assert!(
            core(
                Ok(Some(record)),
                evidence_must_not_be_consulted,
                Ok(Arc::clone(&f.auth)),
                &served,
                &assembled
            )
            .unwrap(),
            "{what} 必须补起监听"
        );
        assert!(served.load(Ordering::SeqCst), "{what} 必须发起 serve");
        assert!(assembled.load(Ordering::SeqCst), "{what} 必须装配 auth 域");
    }
    // 装配失败 → Err 原样上抛、不补起（生产钩子把它折成日志，不阻塞选择）：
    // 持久开启记录与升级证据两形态同责。
    served.store(false, Ordering::SeqCst);
    let failing_auth = Err(DshError::new(codes::IPC_STATE, "fixture assembly failure"));
    let ready = SwitchRecord {
        enabled: true,
        phase: SwitchPhase::Ready,
        ..Default::default()
    };
    let error = core(
        Ok(Some(ready)),
        evidence_must_not_be_consulted,
        Err(DshError::new(codes::IPC_STATE, "fixture assembly failure")),
        &served,
        &assembled,
    )
    .expect_err("持久开启记录的装配失败必须原样上抛");
    assert_eq!(error.code, codes::IPC_STATE);
    assert!(!served.load(Ordering::SeqCst), "装配失败不得发起 serve");
    served.store(false, Ordering::SeqCst);
    let error = core(Ok(None), || true, failing_auth, &served, &assembled)
        .expect_err("升级证据形态的装配失败必须原样上抛");
    assert_eq!(error.code, codes::IPC_STATE);
    assert!(!served.load(Ordering::SeqCst), "装配失败不得发起 serve");
}

/// 关闭面（经生产 select 入口）：从未开启的实例（无开关记录）被选中 → 监听
/// 补位内核不发起 serve——不为已关闭实例起监听、不发布记录（钉住正确面）。
#[tokio::test]
async fn select_to_never_enabled_instance_does_not_bring_listener_up() {
    let f = Fixture::new();
    let rendezvous_path = auth::instance_state_dir(&f.env, &f.instance_id)
        .unwrap()
        .join("rendezvous.json");
    assert!(!rendezvous_path.exists(), "前置：从未开启的实例无在场记录");
    let served = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let fixture_id = f.instance_id.clone();
    let hook_service = Arc::clone(&f.service);
    let hook_served = Arc::clone(&served);
    crate::commands::dsh::instance_select_core(
        &f.env,
        &f.registry,
        crate::commands::dsh::DshInstanceRequest {
            instance_id: fixture_id.clone(),
        },
        &move |id: &str| {
            if id != fixture_id {
                return;
            }
            // 生产钩子装配形状（夹具面）：驱动器补位 + 监听补位升级感知内核。
            // 从未开启的实例：无记录 → 走升级证据面（无 Bound 行 → 三证不齐 →
            // 不补起、不装配）。
            hook_service.resume_incomplete();
            let record_read = hook_service.read_raw();
            let served_slot = Arc::clone(&hook_served);
            let hook_auth = Arc::clone(&f.auth);
            let evidence_service = Arc::clone(&hook_service);
            let _ = crate::services::dsh_control::production::ensure_listener_for_selected_switch(
                record_read,
                move || evidence_service.upgrade_recovery_evidence(),
                || Ok(hook_auth),
                move |_auth| served_slot.store(true, Ordering::SeqCst),
            );
        },
    )
    .expect("生产 select 入口必须受理选择");
    assert!(
        !served.load(Ordering::SeqCst),
        "无开关记录（未开启）不得发起 serve"
    );
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!rendezvous_path.exists(), "未开启实例不得发布在场记录");
}

/// fail-soft（经生产 select 入口）：监听补位的 auth 域装配失败 → 选择本身照常
/// 成功（补位内核 Err 由钩子折成日志；绝不反噬选择），也不发起 serve。
#[tokio::test]
async fn select_back_bringup_assembly_failure_is_fail_soft() {
    let f = Fixture::new();
    f.seed_plugin_installed();
    // 产出 enabled 记录（enable 同步落盘；不等到推进完成——判定只看 enabled）。
    f.service.enable().unwrap();
    assert!(
        f.service.read_raw().unwrap().is_some_and(|r| r.enabled),
        "前置：盘上已开启记录"
    );
    let served = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let outcome: Arc<std::sync::Mutex<Option<Result<bool, DshError>>>> =
        Arc::new(std::sync::Mutex::new(None));
    let fixture_id = f.instance_id.clone();
    let hook_service = Arc::clone(&f.service);
    let hook_served = Arc::clone(&served);
    let hook_outcome = Arc::clone(&outcome);
    let selected = crate::commands::dsh::instance_select_core(
        &f.env,
        &f.registry,
        crate::commands::dsh::DshInstanceRequest {
            instance_id: fixture_id.clone(),
        },
        &move |id: &str| {
            if id != fixture_id {
                return;
            }
            // 生产钩子装配形状（夹具面）：装配失败 → Err 折成日志，选择不受累。
            hook_service.resume_incomplete();
            let record_read = hook_service.read_raw();
            let served_slot = Arc::clone(&hook_served);
            let evidence_service = Arc::clone(&hook_service);
            let result =
                crate::services::dsh_control::production::ensure_listener_for_selected_switch(
                    record_read,
                    move || evidence_service.upgrade_recovery_evidence(),
                    || Err(DshError::new(codes::IPC_STATE, "fixture assembly failure")),
                    move |_auth| served_slot.store(true, Ordering::SeqCst),
                );
            *hook_outcome.lock().unwrap() = Some(result);
        },
    );
    selected.expect("装配失败不得阻塞选择本身");
    assert!(
        outcome.lock().unwrap().as_ref().is_some_and(|r| r.is_err()),
        "装配失败必须由补位内核如实上抛（由钩子落日志）"
    );
    assert!(!served.load(Ordering::SeqCst), "装配失败不得发起 serve");
}

// ============================================================
// Round-8 P1-3：自定义 home 旧钥探测 + 只读历史归属
// ============================================================

/// P1-3(a)：自定义 DSH_HOME 的升级实例（兔维斯推导 env.home ≠ 宿主真实 home）
/// 带死钥再开启：死钥探测必须走**登记核验 home**（与安装管理器装配同源），不能
/// 用兔维斯推导 env.home——查错目录漏检死钥 → tick_checking 不路由安装段 →
/// Authorizing 死等。修复后：探测在核验 home 检出死钥 → 安装段先退役旧身份
/// （核验 home 的目录被删、env.home 不被误动）→ 重装 → 重开新钥自动授权 →
/// 就绪；全程恰好一次安装（无循环）。
#[tokio::test]
async fn custom_home_dead_identity_reinstalls_via_verified_home() {
    let f = Fixture::new_with_custom_host_home();
    f.seed_plugin_installed();
    let old_key =
        HandshakeClient::connect_with_seed(&f.factory, b"custom-home-dead-seed-0000000000").await;
    f.registry
        .register(
            HostKind::Desktop,
            &f.profile,
            &auth::key_digest(&old_key.keys.spki),
        )
        .unwrap();
    f.auth.seed_bound_for_test(
        "custom-dead-binding",
        &auth::b64url_encode(&old_key.keys.spki),
        &f.profile.to_string_lossy(),
    );
    // 死钥身份目录只存在于宿主真实 home（插件 instanceFileStorage 真实落点）；
    // 兔维斯推导 env.home 下没有任何身份文件。
    f.write_plugin_identity(&old_key.keys.spki);
    let identity_file = f.plugin_identity_file();
    f.revoke_bound_directly();
    let env_home_identity = f.env.home.join("tuvis-remote");

    f.service.enable().unwrap();
    // 宿主运行中 + Installed + 死钥 → 时点检查必须路由安装段（而非授权死等）。
    wait_view(&f.service, "安装中", |v| {
        v.phase == SwitchPhase::Installing
    })
    .await;
    // 宿主正常退出 → 退役核验 home 的旧身份 + 重装 → 等待重开。
    f.host.active.store(false, Ordering::SeqCst);
    wait_view(&f.service, "等待重开", |v| {
        v.phase == SwitchPhase::AwaitingReopen
    })
    .await;
    assert!(
        !identity_file.exists(),
        "重装前必须退役核验 home 的旧身份目录"
    );
    assert!(
        !env_home_identity.exists(),
        "探测/退役绝不落在兔维斯推导 env.home（它不是身份目录落点）"
    );
    // 重开 → 新钥先落盘身份再握手（真实生命周期）→ 自动绑定 → 就绪。
    let fresh = HandshakeClient::connect(&f.factory).await;
    f.write_plugin_identity(&fresh.keys.spki);
    f.host.active.store(true, Ordering::SeqCst);
    wait_view(&f.service, "授权中", |v| {
        v.phase == SwitchPhase::Authorizing
    })
    .await;
    wait_intent_armed(&f).await;
    let reply = fresh
        .hello(&f, b"custom-home-fresh-nonce-00000000")
        .await
        .unwrap();
    assert_eq!(
        server_state(&reply),
        "authorized",
        "死钥退役后新钥自动授权: {reply:?}"
    );
    wait_view(&f.service, "就绪", |v| v.phase == SwitchPhase::Ready).await;
    assert_eq!(
        f.runner.adds.load(Ordering::SeqCst),
        1,
        "死钥重装恰好一次（无循环）"
    );
    assert!(identity_file.exists(), "新钥身份存活到就绪");
}

/// P1-3(c)：死钥探测的 home 单源 = 登记核验 home——探测绝不消费兔维斯推导
/// env.home（探测与安装路径清理各读各的 = 双谓词漂移，round-7 教训）。
/// 第九轮 P1-2：探测消费**已装配管理器**的固定基线（装配面与安装路径同源）。
#[tokio::test]
async fn dead_identity_probe_reads_verified_home_not_env_home() {
    let f = Fixture::new_with_custom_host_home();
    let old_key =
        HandshakeClient::connect_with_seed(&f.factory, b"probe-home-seed-0000000000000000").await;
    f.registry
        .register(
            HostKind::Desktop,
            &f.profile,
            &auth::key_digest(&old_key.keys.spki),
        )
        .unwrap();
    f.auth.seed_bound_for_test(
        "probe-binding",
        &auth::b64url_encode(&old_key.keys.spki),
        &f.profile.to_string_lossy(),
    );
    f.revoke_bound_directly();
    // 墓碑在证；身份文件只写在核验 home（env.home 下无任何身份文件）。
    f.write_plugin_identity(&old_key.keys.spki);
    assert!(
        dead_identity_in_force(
            &f.env,
            &f.registry,
            &f.instance_id,
            Some(f.manager.assembled_home()),
        )
        .unwrap(),
        "死钥在核验 home 必须被检出（探测不得因 env.home 查无而漏检）"
    );
    // 反向：身份文件只在推导 env.home 时**不构成**死钥证据——单源是核验 home。
    std::fs::remove_dir_all(f.plugin_identity_dir()).unwrap();
    let decoy_dir = f.env.home.join("tuvis-remote").join(&f.instance_id);
    std::fs::create_dir_all(&decoy_dir).unwrap();
    std::fs::write(
        decoy_dir.join("identity.json"),
        format!(
            r#"{{"version":1,"privateKeyPem":"fixture","publicKeySpki":"{}","trustAnchorSpki":null,"revoked":false,"revokedBindingId":null}}"#,
            auth::b64url_encode(&old_key.keys.spki)
        ),
    )
    .unwrap();
    assert!(
        !dead_identity_in_force(
            &f.env,
            &f.registry,
            &f.instance_id,
            Some(f.manager.assembled_home()),
        )
        .unwrap(),
        "env.home 下的身份文件不是探测证据（单源 = 登记核验 home）"
    );
}

/// P1-3(c)：核验 home 推导**装配单源**——安装管理器装配
/// （`init_instance_manager` 的 env.home）与开关探测共用同一 accessor
/// （登记 dsh_home 优先 / profile 入口权威推导兜底），不允许两处各推一份。
#[test]
fn verified_home_accessor_is_single_source_for_manager_and_probe() {
    let temp = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(temp.path()).unwrap();
    let custom_home = root.join("custom-home");
    let profile = custom_home.join("profiles/desktop");
    std::fs::create_dir_all(&profile).unwrap();
    std::fs::write(profile.join("package.json"), b"{}").unwrap();
    let mut env = (*crate::services::dsh_control::production::production_env()).clone();
    env.home = root.join("env-home");
    env.tuvis_home = root.join("tuvis");
    env.dsh_state_dir = Some(root.join("state"));
    let registry = instances::InstanceRegistry::open(root.join("instances.json")).unwrap();
    let instance = registry
        .register_candidate_with_home(HostKind::Desktop, &profile, Some(&custom_home))
        .unwrap();
    // 生产 revoker 形态（装配期不触发撤权，resolver 不会被调用）。
    let revoker = crate::services::dsh_control::production::InstanceBindingRevoker::with_resolver(
        Arc::new(env.clone()),
        instance.instance_id.clone(),
        Arc::new(|| Err(DshError::new(codes::IPC_STATE, "parity test: not called"))),
    );
    let manager = crate::services::dsh_control::production::init_instance_manager(
        &env,
        &instance,
        Arc::new(revoker),
    );
    let registered = registry.registered(&instance.instance_id).unwrap();
    let verified = instances::verified_instance_home(&registered);
    assert_eq!(
        verified,
        std::fs::canonicalize(&custom_home).unwrap(),
        "登记核验 home 优先（accessor）"
    );
    assert_eq!(
        manager.assembled_home(),
        verified,
        "安装管理器装配 home 必须与探测共用同一 accessor 推导"
    );
}

/// P1-3(b)：关闭 + 卸载后原会话**仍可读**（只读归属凭退役不清空的独立实例
/// 证据——登记行保留的 kind/profile/home + 绑定账本撤权墓碑），而发送/附件/
/// 控制台执行门与授权提交点照常拒绝。所有权硬下界不放松：不属于本实例的
/// 会话（同型 ID 不在本实例 home 会话树）依旧拒绝。
#[tokio::test]
async fn close_uninstall_keeps_history_readable_send_console_refused() {
    let f = Fixture::new();
    f.seed_plugin_installed();
    f.seed_bound_binding();
    // 登记锚钉住绑定钥（生产锚定点同款：授权落账写登记指纹）。
    f.registry
        .register(
            HostKind::Desktop,
            &f.profile,
            &auth::key_digest(HOST_BINDING_KEY),
        )
        .unwrap();
    // 原会话数据（读面 = registry.resolve_readonly_root 生产入口）。
    let session_dir = f.home.join("sessions/project/session-aaaaaaaa");
    std::fs::create_dir_all(&session_dir).unwrap();
    std::fs::write(
        session_dir.join("session.jsonl"),
        b"{\"id\":\"session-aaaaaaaa\"}\n",
    )
    .unwrap();
    f.service.enable().unwrap();
    wait_view(&f.service, "就绪", |v| v.phase == SwitchPhase::Ready).await;
    // （锚定开启态的可读由既有用例面覆盖：legacy 只读 Bound 行恢复 + 探针形态；
    // 本用例夹具的 Bound 行是种子态——落盘发生在关闭撤权段，开启态无盘上账本。）
    // 关闭 → 宿主退出 → 自动卸载完成（身份退役 + 登记锚清空）。
    let view = f.service.disable().unwrap();
    assert_eq!(
        view.send,
        Some(ChannelReadiness::NotAuthorized),
        "关闭后发送通道必须立即呈未授权"
    );
    f.host.active.store(false, Ordering::SeqCst);
    let view = wait_view(&f.service, "关闭完成", |v| v.phase == SwitchPhase::Off).await;
    assert!(!view.enabled);
    assert_eq!(
        f.service.execution_gate().unwrap_err().code,
        codes::SWITCH_OFF,
        "发送/附件/控制台执行门必须拒绝"
    );
    assert_eq!(
        f.service.authorization_gate_check(None).unwrap_err().code,
        codes::SWITCH_OFF,
        "授权提交点必须拒绝"
    );
    assert!(
        f.registry
            .registered(&f.instance_id)
            .unwrap()
            .identity_fingerprint
            .is_empty(),
        "前置：卸载已退役登记锚（旧语义在此 403 的根因现场）"
    );
    // 关键：原会话仍可读——卸载销毁锚与身份目录，不得销毁只读归属证据。
    assert_eq!(
        f.readonly_registry()
            .resolve_readonly_root(&f.instance_id, "session-aaaaaaaa")
            .unwrap(),
        std::fs::canonicalize(&f.home).unwrap()
    );
    // 硬下界：不属于本实例的会话依旧拒绝（不因证据保留而放行跨实例读）。
    assert_eq!(
        f.readonly_registry()
            .resolve_readonly_root(&f.instance_id, "session-zzzzzzzz")
            .unwrap_err()
            .code,
        codes::SESSION_OWNERSHIP_UNPROVEN
    );
    // 硬下界加严（复审 Minor 2）：同一会话 ID 在**已登记外部实例的 home**里真实
    // 在场（会话确实存在于世界、头部可解析出同 ID），退役路径的读依旧拒绝——
    // 拒绝是归属判定，不是「会话不存在」的退化面。
    let foreign_physical = f.root.join("foreign/physical");
    std::fs::create_dir_all(&foreign_physical).unwrap();
    std::fs::write(foreign_physical.join("package.json"), b"{}").unwrap();
    let foreign_home = f.root.join("foreign-home");
    std::fs::create_dir_all(foreign_home.join("profiles")).unwrap();
    std::os::unix::fs::symlink(&foreign_physical, foreign_home.join("profiles/desktop")).unwrap();
    let foreign_session = foreign_home.join("sessions/project/session-zzzzzzzz");
    std::fs::create_dir_all(&foreign_session).unwrap();
    std::fs::write(
        foreign_session.join("session.jsonl"),
        b"{\"id\":\"session-zzzzzzzz\"}\n",
    )
    .unwrap();
    f.registry
        .register_candidate_with_home(HostKind::Desktop, &foreign_physical, Some(&foreign_home))
        .unwrap();
    assert_eq!(
        f.readonly_registry()
            .resolve_readonly_root(&f.instance_id, "session-zzzzzzzz")
            .unwrap_err()
            .code,
        codes::SESSION_OWNERSHIP_UNPROVEN,
        "会话在外部实例 home 在场也不得跨实例归属"
    );
}

/// 复审 Important 回归：**升级携带的锚定行**（旧构建写的登记——锚在、只读证据
/// 字段缺省为空）在本构建走关闭+卸载：退役销锚前回填证据（证据 := 锚）→ 原会话
/// 仍可读（墓碑起源行对位回填证据）；发送/控制台执行门照常拒绝。没有回填时，
/// 锚与证据同时为空 → 只读门拒绝 = 本任务受命修复的 403 在每个存量默认 home
/// 用户的首次升级后卸载上复发。
#[tokio::test]
async fn upgrade_anchored_record_survives_close_uninstall_read_after_backfill() {
    let f = Fixture::new();
    f.seed_plugin_installed();
    f.seed_bound_binding();
    f.registry
        .register(
            HostKind::Desktop,
            &f.profile,
            &auth::key_digest(HOST_BINDING_KEY),
        )
        .unwrap();
    // 升级形态改造：模拟旧构建写的登记行——抹掉只读证据字段（serde 缺省空）。
    let registry_path = f.root.join("instances.json");
    let mut data: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&registry_path).unwrap()).unwrap();
    for instance in data["instances"].as_array_mut().unwrap() {
        instance
            .as_object_mut()
            .unwrap()
            .remove("readEvidenceFingerprint");
    }
    std::fs::write(&registry_path, serde_json::to_vec(&data).unwrap()).unwrap();
    assert_eq!(
        f.registry
            .registered(&f.instance_id)
            .unwrap()
            .read_evidence_fingerprint,
        "",
        "前置：升级形态行（锚在、证据缺）"
    );
    let session_dir = f.home.join("sessions/project/session-aaaaaaaa");
    std::fs::create_dir_all(&session_dir).unwrap();
    std::fs::write(
        session_dir.join("session.jsonl"),
        b"{\"id\":\"session-aaaaaaaa\"}\n",
    )
    .unwrap();
    f.service.enable().unwrap();
    wait_view(&f.service, "就绪", |v| v.phase == SwitchPhase::Ready).await;
    // 关闭 → 宿主退出 → 自动卸载完成 → 只读历史不断链。
    f.service.disable().unwrap();
    f.host.active.store(false, Ordering::SeqCst);
    let view = wait_view(&f.service, "关闭完成", |v| v.phase == SwitchPhase::Off).await;
    assert!(!view.enabled);
    assert_eq!(
        f.service.execution_gate().unwrap_err().code,
        codes::SWITCH_OFF,
        "发送/附件/控制台执行门必须拒绝"
    );
    let registered = f.registry.registered(&f.instance_id).unwrap();
    assert!(
        registered.identity_fingerprint.is_empty(),
        "前置：锚已随卸载退役"
    );
    assert_eq!(
        registered.read_evidence_fingerprint,
        auth::key_digest(HOST_BINDING_KEY),
        "退役必须为升级携带行回填只读证据（证据 := 锚）"
    );
    assert_eq!(
        f.readonly_registry()
            .resolve_readonly_root(&f.instance_id, "session-aaaaaaaa")
            .unwrap(),
        std::fs::canonicalize(&f.home).unwrap(),
        "升级携带行关闭+卸载后原会话仍可读（墓碑起源行对位回填证据）"
    );
}

// ============================================================
// 第九轮 P1-1 回归：关闭未收敛（close_pending）三门 + fail-closed 进程内恢复
// ============================================================

/// 开关账本 JSON 手术（负例构造/恢复用：只改盘上字节，不碰内存账本）。
fn rewrite_ledger_file(path: &std::path::Path, mutate: impl FnOnce(&mut serde_json::Value)) {
    let mut value: serde_json::Value =
        serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    mutate(&mut value);
    std::fs::write(path, serde_json::to_vec_pretty(&value).unwrap()).unwrap();
}

/// 第九轮 P1-1（回归 a）：Failed(Revoke)（关闭撤权失败，旧绑定仍在盘上）不得被
/// 「修复插件」洗成可推进态——修复完成曾按 resume_phase=Off 清掉 failed_step，
/// 此后 enable 的 Failed(Revoke) 拒绝不再命中，未撤权旧钥重新获得写权限。
/// 矩阵：repair 拒 / enable 拒（既有不回退）/ gate_lifecycle 直调拒——判定形态
/// 与生产壳层 `lifecycle_gate_for_selected` 同一入口（`gate_lifecycle × 持久化
/// 记录读取`）；夹具内用同一账本文件的 `read_raw` 替代生产单例读取
/// （`persisted_record` 会装配 production 单例、触碰真实 TUVIS_HOME，单元测试
/// 进程必须隔离；生产壳层穿线另见 tests/dsh_switch_gate_test.rs 的 close_pending
/// 直调用例）；
/// 重启面：重新实例化开关服务（读盘）后记录仍 Failed(Revoke)、门仍拒。
/// 副作用面：被拒的 repair 不得启动管理作业、不得改写开关记录（代次不变）；
/// 重试关闭收敛路径不得被新门挡住。
#[tokio::test]
async fn failed_revoke_blocks_repair_enable_and_direct_lifecycle() {
    let _serial = probe_serial().await;
    let f = Fixture::new();
    f.seed_plugin_installed();
    f.service.enable().unwrap();
    wait_view(&f.service, "授权中", |v| {
        v.phase == SwitchPhase::Authorizing
    })
    .await;
    wait_intent_armed(&f).await;
    let client = HandshakeClient::connect(&f.factory).await;
    let reply = client
        .hello(&f, b"close-gate-nonce-000000000000000")
        .await
        .unwrap();
    assert_eq!(server_state(&reply), "authorized", "首轮意图自动授权");
    wait_view(&f.service, "就绪", |v| v.phase == SwitchPhase::Ready).await;
    let ledger_path = auth::instance_state_dir(&f.env, &f.instance_id)
        .unwrap()
        .join("bindings.json");

    // 撤权写盘失败 → Failed(Revoke)（盘上绑定行保持 Bound——绕过即复活现场）。
    let probe_path = ledger_path.clone();
    auth::ledger_write_probe::set_fail_write(Some(Arc::new(move |path: &std::path::Path| {
        path == probe_path
    })));
    let view = f.service.disable().unwrap();
    auth::ledger_write_probe::set_fail_write(None);
    assert_eq!(view.phase, SwitchPhase::Failed);
    assert_eq!(view.failed_step, Some(SwitchStep::Revoke));

    // ① 修复插件拒绝（必须在启动作业之前拦下）。副作用数量断言：无作业、记录
    // 原样（failed_step 洗白 = 本回归要堵的根因）。
    let record_before = f.service.read().unwrap();
    let error = f
        .service
        .repair()
        .expect_err("Failed(Revoke) 态 repair 必须拒绝");
    assert_eq!(
        error.code,
        codes::SWITCH_ACTIVE,
        "修复拒绝必须走开关活跃码: {error}"
    );
    assert_eq!(
        f.service.read().unwrap(),
        record_before,
        "被拒的 repair 不得改写开关记录（failed_step/reason/代次全保持）"
    );
    assert!(
        f.manager.status().unwrap().operation.is_none(),
        "被拒的 repair 不得启动任何管理作业"
    );

    // ② enable 拒绝（既有语义不回退）。
    let error = f
        .service
        .enable()
        .expect_err("Failed(Revoke) 态 enable 必须拒绝");
    assert_eq!(error.code, codes::SWITCH_REVOKE_PENDING);
    assert_eq!(
        f.service.read().unwrap(),
        record_before,
        "被拒的 enable 不得改写关闭侧记录"
    );

    // ③ 直调生命周期命令拒绝（生产壳层形态：三个直调命令共享同一判定）。
    let record = f.service.read_raw().unwrap();
    assert!(record.is_some(), "前置：盘上开关记录在场");
    let error = gate_lifecycle(record.as_ref()).expect_err("close_pending 态直调必须拒绝");
    assert_eq!(
        error.code,
        codes::SWITCH_ACTIVE,
        "直调拒绝必须走开关活跃码: {error}"
    );

    // —— 重启面：重新实例化开关服务（读盘）后记录仍 Failed(Revoke)、门仍拒 ——
    let service2 = Arc::new(
        RemoteSwitchService::new(
            Arc::clone(&f.env),
            Arc::clone(&f.registry),
            f.instance_id.clone(),
            Arc::clone(&f.services),
            Arc::clone(&f.evidence_source),
        )
        .unwrap(),
    );
    let record2 = service2.read().unwrap();
    assert_eq!(
        record2.phase,
        SwitchPhase::Failed,
        "重启装载：记录仍 Failed"
    );
    assert_eq!(record2.failed_step, Some(SwitchStep::Revoke));
    assert!(service2.enable().is_err(), "重启后 enable 仍拒");
    assert!(service2.repair().is_err(), "重启后 repair 仍拒");
    let record2 = f.service.read_raw().unwrap();
    assert!(gate_lifecycle(record2.as_ref()).is_err(), "重启后直调仍拒");

    // ④ 重试关闭收敛 → 等待卸载（新门不得挡住既有重试收敛路径）；等待卸载态
    // repair / 直调同拒（同属 close_pending），enable 保持既有 SWITCH_ACTIVE。
    let view = f.service.disable().unwrap();
    assert_eq!(view.phase, SwitchPhase::WaitingUninstall);
    let error = f.service.repair().expect_err("等待卸载态 repair 必须拒绝");
    assert_eq!(error.code, codes::SWITCH_ACTIVE);
    let record = f.service.read_raw().unwrap();
    assert!(
        gate_lifecycle(record.as_ref()).is_err(),
        "等待卸载态直调必须拒绝"
    );
    let error = f.service.enable().expect_err("等待卸载态 enable 必须拒绝");
    assert_eq!(error.code, codes::SWITCH_ACTIVE);
}

/// 第九轮 P1-1（回归 b）：Failed(Uninstall)（关闭卸载失败）同属关闭未收敛——
/// enable 曾只拦 Failed(Revoke)，Failed(Uninstall) 态开启直接推进（连撤权兜底面
/// 都没有）。矩阵与 Failed(Revoke) 同形（enable 走统一重试关闭指引码、detail 区分
/// 步骤）；执行门保持拒绝；重试关闭收敛到真 Off 后，直调清理口放行（既有语义）。
#[tokio::test]
async fn failed_uninstall_blocks_enable_repair_and_direct_lifecycle() {
    let f = Fixture::new();
    f.seed_plugin_installed();
    f.seed_bound_binding();
    f.service.enable().unwrap();
    wait_view(&f.service, "就绪", |v| v.phase == SwitchPhase::Ready).await;
    f.runner.fail_remove.store(true, Ordering::SeqCst);
    f.host.active.store(false, Ordering::SeqCst);
    f.service.disable().unwrap();
    let view = wait_view(&f.service, "卸载失败", |v| {
        v.phase == SwitchPhase::Failed
    })
    .await;
    assert_eq!(view.failed_step, Some(SwitchStep::Uninstall));

    // 矩阵：enable / repair / 直调全拒（副作用：记录不改写）。
    let record_before = f.service.read().unwrap();
    let error = f
        .service
        .enable()
        .expect_err("Failed(Uninstall) 态 enable 必须拒绝");
    assert_eq!(
        error.code,
        codes::SWITCH_REVOKE_PENDING,
        "关闭侧 Failed 统一走「再次关闭重试」指引码: {error}"
    );
    let error = f
        .service
        .repair()
        .expect_err("Failed(Uninstall) 态 repair 必须拒绝");
    assert_eq!(error.code, codes::SWITCH_ACTIVE);
    let record = f.service.read_raw().unwrap();
    assert!(
        gate_lifecycle(record.as_ref()).is_err(),
        "close_pending 态直调必须拒绝"
    );
    assert_eq!(
        f.service.read().unwrap(),
        record_before,
        "被拒意图不得改写关闭侧记录"
    );

    // 执行门保持拒绝（发送/附件/控制台共用判定——关闭侧失败不复活执行面）。
    assert_eq!(
        f.service.execution_gate().unwrap_err().code,
        codes::SWITCH_OFF,
        "发送/附件/控制台执行门必须拒绝"
    );

    // 重试关闭收敛：修复失败源 → 卸载完成 → 真 Off → 直调清理口放行（不回退）。
    f.runner.fail_remove.store(false, Ordering::SeqCst);
    f.service.disable().unwrap();
    let view = wait_view(&f.service, "关闭完成", |v| v.phase == SwitchPhase::Off).await;
    assert!(!view.enabled);
    let record = f.service.read_raw().unwrap();
    assert!(record.is_some(), "Off 记录在场");
    assert!(
        gate_lifecycle(record.as_ref()).is_ok(),
        "真 Off 的直调清理口必须保留（既有语义不回退）"
    );
    assert_eq!(
        f.service.execution_gate().unwrap_err().code,
        codes::SWITCH_OFF,
        "真 Off 执行门照常拒绝"
    );
}

/// 第九轮 P1-1（回归 c·进程内恢复）：撤权写盘**结果不可确认**（写失败且盘上字节
/// 已变）→ 整域 fail-closed（binding_faulted 置位）。补写收敛（重试关闭 → 卸载
/// 完成）后，**不重启**即可再次布防意图并完成新绑定——恢复内核懒验证式核验盘上
/// 证据（待收敛空 / 无 Bound 行 / 墓碑对位 / 登记锚对位）后清屏障。本夹具带登记
/// 锚（Fixture 装配同生产），恢复核验覆盖 ④ 锚指纹路径（卸载退役后锚为空 = 关闭
/// 流程走完的证据面，允许通过；注释见实现）。
#[tokio::test]
async fn fail_closed_uncertain_write_recovers_in_process_then_rebinds() {
    let _serial = probe_serial().await;
    let f = Fixture::new();
    f.seed_plugin_installed();
    f.service.enable().unwrap();
    wait_view(&f.service, "授权中", |v| {
        v.phase == SwitchPhase::Authorizing
    })
    .await;
    wait_intent_armed(&f).await;
    let client = HandshakeClient::connect(&f.factory).await;
    let reply = client
        .hello(&f, b"failclosed-nonce-000000000000000")
        .await
        .unwrap();
    assert_eq!(server_state(&reply), "authorized", "首轮意图自动授权");
    wait_view(&f.service, "就绪", |v| v.phase == SwitchPhase::Ready).await;
    let ledger_path = auth::instance_state_dir(&f.env, &f.instance_id)
        .unwrap()
        .join("bindings.json");

    // ① 注入「写入结果不可确认」：探针拍先篡改盘上字节再拒绝写入——
    //    revoke_local_locked 读到 actual != previous → 整域 fail-closed。
    let probe_path = ledger_path.clone();
    auth::ledger_write_probe::set_fail_write(Some(Arc::new(move |path: &std::path::Path| {
        if path != probe_path {
            return false;
        }
        if let Ok(mut bytes) = std::fs::read(path) {
            bytes.extend_from_slice(b"\n");
            let _ = std::fs::write(path, &bytes);
        }
        true
    })));
    let view = f.service.disable().unwrap();
    auth::ledger_write_probe::set_fail_write(None);
    assert_eq!(view.phase, SwitchPhase::Failed);
    assert_eq!(view.failed_step, Some(SwitchStep::Revoke));
    assert!(
        !client.conn.write_available(),
        "fail-closed 域受信连接必须已断（内存面先行）"
    );
    let ledger: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&ledger_path).unwrap()).unwrap();
    assert_eq!(
        ledger["bindings"][0]["state"], "bound",
        "前置：写失败且盘上未收敛（绑定行原样）"
    );

    // ② 重试关闭：补写收敛（内存账本重写盘——墓碑落盘）→ 宿主退出 → Off。
    let view = f.service.disable().unwrap();
    assert_eq!(view.phase, SwitchPhase::WaitingUninstall);
    let ledger: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&ledger_path).unwrap()).unwrap();
    assert_eq!(
        ledger["bindings"][0]["state"], "revoked",
        "补写收敛：墓碑落盘"
    );
    assert_eq!(
        ledger["bindings"][0]["revokedKeySpki"],
        auth::b64url_encode(&client.keys.spki),
        "墓碑留存被撤钥（恢复核验 ③ 的对位材料）"
    );
    f.host.active.store(false, Ordering::SeqCst);
    wait_view(&f.service, "关闭完成", |v| v.phase == SwitchPhase::Off).await;

    // ③ 不重启：再次布防意图并完成新绑定（恢复内核在授权提交点自动收敛）。
    f.host.active.store(true, Ordering::SeqCst);
    f.seed_plugin_installed();
    f.service.enable().unwrap();
    wait_view(&f.service, "授权中", |v| {
        v.phase == SwitchPhase::Authorizing
    })
    .await;
    wait_intent_armed(&f).await;
    let fresh = HandshakeClient::connect(&f.factory).await;
    let reply = fresh
        .hello(&f, b"rebind-after-recovery-nonce-0000")
        .await
        .unwrap();
    assert_eq!(
        server_state(&reply),
        "authorized",
        "补写收敛后不重启即可完成新绑定（fail-closed 屏障必须能进程内恢复）: {reply:?}"
    );
    wait_view(&f.service, "重新就绪", |v| {
        v.phase == SwitchPhase::Ready
    })
    .await;

    // —— 负例 a：盘上仍有 Bound 行（不确定写入落盘的形态）→ 恢复失败、绑定仍拒 ——
    // 先重新走一遍 fail-closed 现场（再次关闭注入同一探针），收敛后篡改盘面再开启。
    let probe_path = ledger_path.clone();
    auth::ledger_write_probe::set_fail_write(Some(Arc::new(move |path: &std::path::Path| {
        if path != probe_path {
            return false;
        }
        if let Ok(mut bytes) = std::fs::read(path) {
            bytes.extend_from_slice(b"\n");
            let _ = std::fs::write(path, &bytes);
        }
        true
    })));
    f.service.disable().unwrap();
    auth::ledger_write_probe::set_fail_write(None);
    f.service.disable().unwrap();
    f.host.active.store(false, Ordering::SeqCst);
    wait_view(&f.service, "再次关闭", |v| v.phase == SwitchPhase::Off).await;
    let faulted_bytes = std::fs::read(&ledger_path).unwrap();
    // 篡改**引发本轮 fail-closed 的那行**（账本末行——证据槽对位的对象；首轮的
    // 旧墓碑行在前，不在核验对象内）为 Bound，模拟「不确定写入落盘」的形态。
    rewrite_ledger_file(&ledger_path, |value| {
        value["bindings"]
            .as_array_mut()
            .unwrap()
            .last_mut()
            .unwrap()["state"] = serde_json::json!("bound");
    });
    f.host.active.store(true, Ordering::SeqCst);
    f.seed_plugin_installed();
    f.service.enable().unwrap();
    wait_view(&f.service, "负例授权中", |v| {
        v.phase == SwitchPhase::Authorizing
    })
    .await;
    wait_intent_armed(&f).await;
    let refused = HandshakeClient::connect(&f.factory).await;
    let reply = refused
        .hello(&f, b"negative-bound-row-nonce-0000000")
        .await
        .unwrap();
    assert_eq!(
        server_state(&reply),
        "error:DSH_IPC_STATE",
        "盘上仍有 Bound 行 → 恢复尝试失败、保持 faulted、绑定仍拒: {reply:?}"
    );

    // —— 负例 b：墓碑不对位（被撤钥摘要对不上）→ 恢复失败、绑定仍拒 ——
    // 上一拍已消费布防意图（拒绝路径不可逆），本拍走人工批准面（同一提交点闸门）。
    std::fs::write(&ledger_path, &faulted_bytes).unwrap();
    rewrite_ledger_file(&ledger_path, |value| {
        value["bindings"]
            .as_array_mut()
            .unwrap()
            .last_mut()
            .unwrap()["revokedKeySpki"] =
            serde_json::json!(auth::b64url_encode(b"other-revoked-key"));
    });
    let refused_tomb = HandshakeClient::connect(&f.factory).await;
    let reply = refused_tomb
        .hello(&f, b"negative-tombstone-nonce-0000000")
        .await
        .unwrap();
    let pending_id = match &reply {
        auth::ServerFrame::ServerHello(hello) => {
            hello.binding_id.clone().expect("pending 必带 binding_id")
        }
        other => panic!("期待 pending_approval，得到 {other:?}"),
    };
    let error = f
        .auth
        .approve(&pending_id)
        .await
        .expect_err("墓碑不对位 → 恢复尝试失败，人工批准必须在提交点被拒");
    assert_eq!(
        error.code,
        codes::IPC_STATE,
        "恢复失败的拒绝必须走 IPC_STATE（binding state requires recovery 族）: {error}"
    );

    // —— 盘面复原后同一进程内恢复生效：人工批准在提交点触发恢复 → 绑定落地 ——
    // （复原到第二轮收敛态 faulted_bytes：含引发本轮 fail-closed 的末行墓碑；
    // converged_bytes 是首轮快照，不含该行——证据行缺席会保守拒绝。）
    std::fs::write(&ledger_path, &faulted_bytes).unwrap();
    let recovered = HandshakeClient::connect(&f.factory).await;
    let reply = recovered
        .hello(&f, b"recovery-positive-nonce-00000000")
        .await
        .unwrap();
    let pending_id = match &reply {
        auth::ServerFrame::ServerHello(hello) => {
            hello.binding_id.clone().expect("pending 必带 binding_id")
        }
        other => panic!("期待 pending_approval，得到 {other:?}"),
    };
    f.auth
        .approve(&pending_id)
        .await
        .expect("盘上证据齐备 → 恢复内核清屏障 → 批准落地");
    wait_view(&f.service, "恢复后就绪", |v| {
        v.phase == SwitchPhase::Ready
    })
    .await;
}

// ============================================================
// 第九轮 P1-2：关闭后历史可读 + 缺 home 卸载链 + 崩溃收尾 + 绑定失败补偿
// ============================================================

/// P1-2a 缺 home 旧登记的关闭+卸载链：撤权前回填登记 home（卸载受理门的证据
/// 面）→ 撤权翻墓碑 → 卸载收敛。不回填则受理门在撤权后永远 Unproven（Bound
/// 行已清零、登记无 home）——关闭+卸载死路（复审根因 2）。同时锁定「不碰其他
/// 根」：其他实例的登记与授权账本原样。
#[tokio::test]
async fn legacy_close_backfills_home_before_revocation_then_uninstall_converges() {
    let f = Fixture::new_legacy();
    f.seed_plugin_installed();
    // 第二实例 B（自带 home）：关闭链只准作用在 A 上。
    let physical_b = f.root.join("physical/profile-b");
    std::fs::create_dir_all(&physical_b).unwrap();
    std::fs::write(physical_b.join("package.json"), b"{}").unwrap();
    let home_b = f.root.join("home-b");
    std::fs::create_dir_all(home_b.join("profiles")).unwrap();
    std::os::unix::fs::symlink(&physical_b, home_b.join("profiles/desktop")).unwrap();
    let instance_b = f
        .registry
        .register_candidate_with_home(HostKind::Desktop, &physical_b, Some(&home_b))
        .unwrap();
    let b_ledger = auth::instance_state_dir(&f.env, &instance_b.instance_id)
        .expect("夹具实例 ID 恒合法")
        .join("bindings.json");

    f.service.enable().unwrap();
    wait_intent_armed(&f).await;
    let client =
        HandshakeClient::connect_with_seed(&f.factory, b"p12-legacy-key-seed-000000000000").await;
    let reply = client
        .hello(&f, b"p12-legacy-nonce-000000000000000")
        .await
        .unwrap();
    assert_eq!(server_state(&reply), "authorized", "首轮意图自动授权");
    wait_view(&f.service, "就绪", |v| v.phase == SwitchPhase::Ready).await;

    f.service.disable().unwrap();
    f.host.active.store(false, Ordering::SeqCst);
    let view = wait_view(&f.service, "关闭完成", |v| v.phase == SwitchPhase::Off).await;
    assert!(!view.enabled);
    assert!(
        f.runner.calls.load(Ordering::SeqCst) > 0,
        "卸载必须真实发生（官方 remove 被调用）"
    );

    // 回填生效：登记 dsh_home 已补、对位正确；只读证据保留、锚已随卸载退役。
    let registered = f.registry.registered(&f.instance_id).unwrap();
    assert_eq!(
        registered.dsh_home.as_ref(),
        Some(&f.canonical_home()),
        "撤权前必须把唯一可证的逻辑 home 回填进登记（卸载受理门证据面）"
    );
    assert!(registered.identity_fingerprint.is_empty(), "卸载退役清锚");
    assert_eq!(
        registered.read_evidence_fingerprint,
        auth::key_digest(&client.keys.spki),
        "只读证据保留（卸载后历史归属仍可证明）"
    );
    let ledger: serde_json::Value =
        serde_json::from_slice(&std::fs::read(f.bindings_ledger_file()).unwrap()).unwrap();
    assert_eq!(
        ledger["bindings"][0]["state"], "revoked",
        "关闭链必须真实撤权（墓碑落盘）"
    );
    // 不碰其他根：B 的登记与授权账本原样。
    let registered_b = f.registry.registered(&instance_b.instance_id).unwrap();
    assert_eq!(
        registered_b.dsh_home.as_ref(),
        Some(&std::fs::canonicalize(&home_b).unwrap()),
        "关闭链不得改写其他实例的登记"
    );
    assert!(
        !b_ledger.exists(),
        "关闭链不得在其他实例授权域留下任何绑定痕迹"
    );
}

/// P1-2b 回填失败注入（绑定行入口只剩物理形态、逻辑 home 不可推导）：保持关闭
/// + 失败步骤（证据保全面）+ 具体原因；宁可不撤（Bound 行原样、可重试）也不把
/// 猜测值写进登记。
#[tokio::test]
async fn legacy_close_backfill_failure_keeps_off_without_revoking() {
    let f = Fixture::new_legacy();
    f.seed_plugin_installed();
    f.service.enable().unwrap();
    wait_intent_armed(&f).await;
    let client =
        HandshakeClient::connect_with_seed(&f.factory, b"p12-bf-key-seed-0000000000000000").await;
    let reply = client
        .hello(&f, b"p12-bf-nonce-0000000000000000000")
        .await
        .unwrap();
    assert_eq!(server_state(&reply), "authorized");
    wait_view(&f.service, "就绪", |v| v.phase == SwitchPhase::Ready).await;
    // 注入：绑定行的 profile 入口只剩物理形态（无 profiles 祖先）→ 逻辑 home
    // 不可推导 → 回填不可证。
    let ledger_path = f.bindings_ledger_file();
    let mut ledger: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&ledger_path).unwrap()).unwrap();
    ledger["bindings"][0]["profileDir"] =
        serde_json::json!(f.profile.to_string_lossy().to_string());
    std::fs::write(&ledger_path, serde_json::to_vec(&ledger).unwrap()).unwrap();

    let view = f.service.disable().unwrap();
    assert!(!view.enabled, "回填失败保持关闭侧");
    let record: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&f.service.record_path).unwrap())
            .expect("开关记录可解析");
    assert_eq!(
        record["phase"], "failed",
        "回填失败必须落失败态（不进撤权段）: {record:?}"
    );
    assert_eq!(
        record["failedStep"],
        serde_json::json!("home_evidence"),
        "失败步骤指向证据保全面（wire 词）: {record:?}"
    );
    assert!(
        record["reasonCode"].is_string(),
        "回填失败必须报告具体原因: {record:?}"
    );
    // 宁可不撤：Bound 行原样（重试可再入）；登记不被写错。
    let ledger: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&ledger_path).unwrap()).unwrap();
    assert_eq!(
        ledger["bindings"][0]["state"], "bound",
        "回填失败不得撤权（保持可重试）"
    );
    let registered = f.registry.registered(&f.instance_id).unwrap();
    assert!(
        registered.dsh_home.is_none(),
        "证据不足时登记不得被写入猜测值"
    );
}

/// P1-2c 死钥探测消费装配面：登记无 dsh_home 的记录，管理器已按「绑定证据
/// 证明的 home」装配（受理门单源）——探测必须用**装配 home** 找身份目录，而不
/// 是从登记 profile 再推一份第二口径（外链/自定义 home 的宿主会查错目录 →
/// 死钥漏检 → 授权步死等）。生产对应面 = 管理器缓存命中（装配时证据可证、
/// 其后绑定失效）仍返回旧装配管理器——探测与装配必须同源。
/// 边界如实申报：本形态（墓碑在场 + 登记无 home）装配面可证死钥并路由安装段，
/// 但安装作业的受理门按 M6 硬下界仍拒（home 证据不足如实上报）——路由修复
/// 不放宽受理证据面。
#[tokio::test]
async fn dead_key_probe_consumes_assembled_home_for_legacy_records() {
    let f = Fixture::new_legacy();
    f.seed_plugin_installed();
    let old =
        HandshakeClient::connect_with_seed(&f.factory, b"p12-dead-old-seed-00000000000000").await;
    // 旧构建终态：锚定旧钥 + 撤权墓碑（逻辑入口行）+ 旧身份目录在**装配 home**。
    f.registry
        .register(
            HostKind::Desktop,
            &f.profile,
            &auth::key_digest(&old.keys.spki),
        )
        .unwrap();
    let ledger_path = f.bindings_ledger_file();
    std::fs::create_dir_all(ledger_path.parent().unwrap()).unwrap();
    let row = serde_json::json!({
        "bindingId": "legacy-dead",
        "publicKeySpki": "",
        "revokedKeySpki": auth::b64url_encode(&old.keys.spki),
        "executable": "/fixture/dsh", "pid": 1, "startedAtMs": 100,
        "profileDir": f.home.join("profiles/desktop").to_string_lossy(),
        "createdAtMs": 101, "state": "revoked"
    });
    std::fs::write(
        &ledger_path,
        serde_json::to_vec(&serde_json::json!({ "bindings": [row] })).unwrap(),
    )
    .unwrap();
    f.write_plugin_identity(&old.keys.spki);

    f.service.enable().unwrap();
    // 死钥识别 → 路由安装段 → 受理门。断言面取**终态**：failed_step == Install
    // 只能来自 tick_installing（安装段入口），而进入安装段的唯一通道是探测命中
    // 装配 home 的身份目录——探测漏检时相位停在核验/授权段（授权死等），永远
    // 不会出现 Failed(Install)。
    // 边界（如实）：无 home 证据的登记不得被受理门放行安装作业（M6 硬下界
    // 不因路由修复放宽）——失败原因如实指向证据缺失。
    let view = wait_view(&f.service, "死钥路由后受理门如实拒绝", |v| {
        v.phase == SwitchPhase::Failed
    })
    .await;
    assert_eq!(
        view.failed_step,
        Some(SwitchStep::Install),
        "必须经安装段（死钥路由命中）后失败: {view:?}"
    );
    assert_eq!(
        view.reason_code.as_deref(),
        Some(codes::INSTANCE_UNVERIFIED),
        "home 证据缺失必须如实上报，不猜测: {view:?}"
    );
}

/// P1-2d 崩溃收尾：移包后、身份退役前中断（作业回执停在 Verifying + 身份目录
/// 与登记锚残留）→ 重启后开关驱动器继续收尾：幽灵遗留就地结算（退役补完）→
/// 关闭收敛；完成回执对应实际状态（身份目录消失、锚清空、只读证据保留）。
#[tokio::test]
async fn interrupted_uninstall_converges_on_restart_and_retires_identity() {
    // Minor-3：identity_removal_probe 是全局单槽（set 语义 = 替换），与其他注入
    // 用例共持串行门，覆盖完整注入周期（设置 → 跨 await 等待 → 清理）。
    let _serial = probe_serial().await;
    let f = Fixture::new();
    f.seed_plugin_installed();
    f.seed_history_session("session-aaaaaaaa");
    f.service.enable().unwrap();
    wait_intent_armed(&f).await;
    let client =
        HandshakeClient::connect_with_seed(&f.factory, b"p12-crash-key-seed-0000000000000").await;
    let reply = client
        .hello(&f, b"p12-crash-nonce-0000000000000000")
        .await
        .unwrap();
    assert_eq!(server_state(&reply), "authorized");
    wait_view(&f.service, "就绪", |v| v.phase == SwitchPhase::Ready).await;
    f.write_plugin_identity(&client.keys.spki);
    // 注入身份目录删除失败：卸载在「移包完成、身份退役失败」点落 Failed——
    // 构造崩溃中断的等价盘面（身份目录 + 登记锚残留）。
    let probe_dir = f.plugin_identity_dir();
    install::identity_removal_probe::set_fail(Some(Arc::new(move |path: &Path| path == probe_dir)));
    f.service.disable().unwrap();
    f.host.active.store(false, Ordering::SeqCst);
    let failed = wait_view(&f.service, "卸载失败（身份退役失败）", |v| {
        v.phase == SwitchPhase::Failed
    })
    .await;
    install::identity_removal_probe::set_fail(None);
    assert_eq!(failed.failed_step, Some(SwitchStep::Uninstall));
    assert!(
        f.plugin_identity_file().exists(),
        "中断时身份目录必须仍在场"
    );
    assert!(
        !f.registry
            .registered(&f.instance_id)
            .unwrap()
            .identity_fingerprint
            .is_empty(),
        "中断时登记锚必须仍在"
    );
    assert!(
        !f.runner.installed.load(Ordering::SeqCst),
        "中断前移包已完成"
    );

    // —— 模拟崩溃：作业回执回退到非终态 Verifying、开关记录回到等待卸载，
    // 然后换「新进程」（新管理器从盘上账本装载 = 幽灵遗留）——
    let mgmt = f.management_ledger_file();
    let job_id = {
        let ledger: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&mgmt).unwrap()).unwrap();
        ledger["jobs"][0]["jobId"]
            .as_str()
            .expect("失败回执已落盘")
            .to_owned()
    };
    let generation = f.service.read().unwrap().generation;
    let record_path = f.service.record_path.clone();
    let snapshot = std::fs::read(&record_path).unwrap();
    // 逼退**当前代**驱动器：开启驱动器已随就绪终态退场（退出计数已 > 0），
    // 基线必须取当前值，否则等到的是旧行为的首次退场、旧驱动器仍在跑并把我
    // 刚写回的「等待卸载」盘面再次推回 Failed。
    let driver_baseline =
        crate::services::dsh_control::switch::arm_test_hooks::driver_exit_count(&f.instance_id);
    let mut bumped: serde_json::Value = serde_json::from_slice(&snapshot).unwrap();
    bumped["generation"] = serde_json::json!(generation + 1);
    std::fs::write(&record_path, serde_json::to_vec(&bumped).unwrap()).unwrap();
    wait_driver_exit(&f.instance_id, driver_baseline).await;
    let mut record: serde_json::Value = serde_json::from_slice(&snapshot).unwrap();
    record["phase"] = serde_json::json!("waiting_uninstall");
    record["failedStep"] = serde_json::Value::Null;
    record["reasonCode"] = serde_json::Value::Null;
    record["jobId"] = serde_json::json!(job_id);
    record["jobKind"] = serde_json::json!("uninstall");
    std::fs::write(&record_path, serde_json::to_vec(&record).unwrap()).unwrap();
    let mut mgmt_ledger: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&mgmt).unwrap()).unwrap();
    mgmt_ledger["jobs"][0]["phase"] = serde_json::json!("verifying");
    std::fs::write(&mgmt, serde_json::to_vec(&mgmt_ledger).unwrap()).unwrap();

    // 重启：新服务实例 + 新管理器（幽灵遗留）→ 启动恢复拉起驱动器。
    let fresh_manager = f.fresh_manager();
    let auth = Arc::clone(&f.auth);
    let services: Arc<SwitchServices> = {
        let manager = Arc::clone(&fresh_manager);
        Arc::new(move || Ok((Arc::clone(&manager), Arc::clone(&auth))))
    };
    let restarted = Arc::new(
        RemoteSwitchService::new(
            Arc::clone(&f.env),
            Arc::clone(&f.registry),
            f.instance_id.clone(),
            services,
            Arc::clone(&f.evidence_source),
        )
        .unwrap()
        .with_poll(Duration::from_millis(20)),
    );
    assert!(
        incomplete_startup_records(&f.env).contains(&f.instance_id),
        "启动扫描必须给出非终态记录的实例"
    );
    assert!(restarted.resume_incomplete(), "启动恢复必须拉起本代驱动器");

    // 收尾：幽灵卸载就地结算（退役补完）→ 关闭收敛。
    wait_record(&restarted, "崩溃收尾完成（Off）", |r| {
        r.phase == SwitchPhase::Off
    })
    .await;
    assert!(!f.plugin_identity_file().exists(), "收尾必须补完身份退役");
    let registered = f.registry.registered(&f.instance_id).unwrap();
    assert!(
        registered.identity_fingerprint.is_empty(),
        "收尾必须清登记锚"
    );
    assert_eq!(
        registered.read_evidence_fingerprint,
        auth::key_digest(&client.keys.spki),
        "只读证据保留"
    );
    let mgmt_ledger: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&mgmt).unwrap()).unwrap();
    assert_eq!(
        mgmt_ledger["jobs"][0]["phase"], "succeeded",
        "完成回执必须对应实际状态"
    );
    // 卸载后历史仍可读（只读证据 + 登记回填 home）。
    let registry = f.readonly_registry();
    assert_eq!(
        registry
            .resolve_readonly_root(&f.instance_id, "session-aaaaaaaa")
            .unwrap(),
        f.canonical_home(),
        "崩溃收尾后历史只读不断链"
    );
}

/// P1-2e 新绑定失败补偿：重绑提交（登记锚 CAS 之后）在账本落盘处失败 →
/// 登记指纹必须回滚为旧值，曾经有效的只读证据不得被失败提交覆盖。
#[tokio::test]
async fn rebind_ledger_write_failure_restores_previous_fingerprint() {
    let _serial = probe_serial().await;
    let f = Fixture::new();
    f.seed_plugin_installed();
    f.service.enable().unwrap();
    wait_intent_armed(&f).await;
    let old =
        HandshakeClient::connect_with_seed(&f.factory, b"p12-rebind-old-seed-000000000000").await;
    let reply = old
        .hello(&f, b"p12-rebind-old-nonce-00000000000")
        .await
        .unwrap();
    assert_eq!(server_state(&reply), "authorized", "首轮意图自动授权");
    wait_view(&f.service, "就绪", |v| v.phase == SwitchPhase::Ready).await;
    let old_fingerprint = auth::key_digest(&old.keys.spki);
    // 身份漂移：直接撤权（墓碑落账、锚不退役）→ 新钥 hello 落待批（重绑入口）。
    // （首轮意图已在首绑时消费，新钥不再被意图烧掉——直接进待批。）
    f.revoke_bound_directly();
    let new = HandshakeClient::connect(&f.factory).await;
    let pending_hello = new
        .hello(&f, b"p12-rebind-retry-nonce-000000000")
        .await
        .unwrap();
    assert_eq!(
        server_state(&pending_hello),
        "pending_approval",
        "新钥落待批: {pending_hello:?}"
    );

    // 注入绑定账本写盘失败（写前即拒 → 磁盘原样）→ 重绑提交在锚 CAS 之后失败。
    let probe_path = f.bindings_ledger_file();
    auth::ledger_write_probe::set_fail_write(Some(Arc::new(move |path: &std::path::Path| {
        path == probe_path
    })));
    let pending = f.auth.pending_binding().expect("待批新钥在场");
    let result = f
        .auth
        .approve_rebind(
            &f.instance_id,
            &pending.binding.binding_id,
            &old_fingerprint,
        )
        .await;
    auth::ledger_write_probe::set_fail_write(None);
    assert!(result.is_err(), "账本写失败必须如实拒绝重绑提交");

    // 补偿：登记指纹回滚为旧值；只读证据保持旧值（不随当代锚回滚）。
    let registered = f.registry.registered(&f.instance_id).unwrap();
    assert_eq!(
        registered.identity_fingerprint, old_fingerprint,
        "失败的新绑定不得覆盖原登记指纹"
    );
    assert_eq!(
        registered.read_evidence_fingerprint, old_fingerprint,
        "只读证据不得被失败提交覆盖"
    );
    // 绑定账本保持旧钥墓碑（提交未生效）。
    let ledger: serde_json::Value =
        serde_json::from_slice(&std::fs::read(f.bindings_ledger_file()).unwrap()).unwrap();
    assert!(
        ledger["bindings"]
            .as_array()
            .is_some_and(|rows| rows.iter().all(|row| row["state"] != "bound")),
        "失败提交不得把新钥 Bound 行留在账本: {ledger:?}"
    );
}

/// P1-2 复审 Important-1：卸载退役后（锚空、只读证据 = 旧钥、账本留旧钥墓碑）
/// 再次首绑，提交在登记锚 CAS 之后失败（账本写盘失败、磁盘确认未变）→ 补偿
/// 必须把**只读证据一并恢复为提交前值**。旧锚为空时回滚只恢复锚（CAS 空 new
/// 不触发证据同步），证据被失败提交推进到新钥 → 只读门拿新钥证据对旧钥墓碑
/// → 历史 403（曾经有效的只读证据被失败提交覆盖）。
#[tokio::test]
async fn first_bind_after_uninstall_write_failure_restores_read_evidence() {
    let _serial = probe_serial().await;
    let f = Fixture::new();
    f.seed_plugin_installed();
    f.seed_history_session("session-aaaaaaaa");
    f.service.enable().unwrap();
    wait_intent_armed(&f).await;
    let old =
        HandshakeClient::connect_with_seed(&f.factory, b"p12-evict-old-seed-0000000000000").await;
    let reply = old
        .hello(&f, b"p12-evict-old-nonce-000000000000")
        .await
        .unwrap();
    assert_eq!(server_state(&reply), "authorized", "首轮意图自动授权");
    wait_view(&f.service, "就绪", |v| v.phase == SwitchPhase::Ready).await;
    let old_digest = auth::key_digest(&old.keys.spki);
    // 关闭 → 卸载完成：锚清空、只读证据保留旧钥、账本留墓碑——历史可读基线。
    f.service.disable().unwrap();
    f.host.active.store(false, Ordering::SeqCst);
    wait_view(&f.service, "关闭完成", |v| v.phase == SwitchPhase::Off).await;
    {
        let registered = f.registry.registered(&f.instance_id).unwrap();
        assert!(registered.identity_fingerprint.is_empty(), "卸载退役清锚");
        assert_eq!(registered.read_evidence_fingerprint, old_digest);
        assert_eq!(
            f.readonly_registry()
                .resolve_readonly_root(&f.instance_id, "session-aaaaaaaa")
                .unwrap(),
            f.canonical_home(),
            "卸载后历史可读（基线）"
        );
    }

    // 再开启（插件重装 + 宿主回归）→ 布防意图 → 全新钥 B 首绑（TOFU，锚空
    // Normal 提交）——注入绑定账本写盘失败（写前即拒 → 磁盘确认未变）→ 提交
    // 在锚 CAS 之后失败。
    f.service.enable().unwrap();
    wait_view(&f.service, "等待重开完成重装", |v| {
        v.phase == SwitchPhase::AwaitingReopen
    })
    .await;
    f.host.active.store(true, Ordering::SeqCst);
    wait_intent_armed(&f).await;
    let new = HandshakeClient::connect(&f.factory).await;
    let probe_path = f.bindings_ledger_file();
    auth::ledger_write_probe::set_fail_write(Some(Arc::new(move |path: &std::path::Path| {
        path == probe_path
    })));
    let reply = new
        .hello(&f, b"p12-evict-new-nonce-000000000000")
        .await
        .unwrap();
    auth::ledger_write_probe::set_fail_write(None);
    assert!(
        server_state(&reply).starts_with("error:"),
        "账本写失败必须拒绝首绑提交: {reply:?}"
    );

    // 补偿后：锚空 + 只读证据恢复为提交前旧值 + 历史仍可读。
    let registered = f.registry.registered(&f.instance_id).unwrap();
    assert!(
        registered.identity_fingerprint.is_empty(),
        "补偿回滚空锚: {registered:?}"
    );
    assert_eq!(
        registered.read_evidence_fingerprint, old_digest,
        "失败的首绑不得覆盖卸载保留的只读证据: {registered:?}"
    );
    assert_eq!(
        f.readonly_registry()
            .resolve_readonly_root(&f.instance_id, "session-aaaaaaaa")
            .unwrap(),
        f.canonical_home(),
        "补偿后历史仍可读（恢复后的证据 == 最新起源行墓碑）"
    );
}

/// P1-2 复审 Important-2：崩溃对账不得「先发布成功回执、再依赖另一作业补齐
/// 事实」——包已移除 + 撤权已收敛的遗留卸载，恢复成功判据必须包含信任物退役
/// （身份目录删除 + 登记锚清空）。否则成功回执落盘后、补完作业完成前再次崩溃
/// 且开关记录未换作业 ID → 下次启动凭成功回执直接 Off，身份目录与登记锚永久
/// 残留（完成回执与实际状态脱钩）。
#[tokio::test]
async fn reconciled_uninstall_success_receipt_implies_identity_retired() {
    let f = Fixture::new();
    f.seed_plugin_installed();
    f.service.enable().unwrap();
    wait_intent_armed(&f).await;
    let client =
        HandshakeClient::connect_with_seed(&f.factory, b"p12-residue-key-seed-00000000000").await;
    let reply = client
        .hello(&f, b"p12-residue-nonce-00000000000000")
        .await
        .unwrap();
    assert_eq!(server_state(&reply), "authorized");
    wait_view(&f.service, "就绪", |v| v.phase == SwitchPhase::Ready).await;
    f.write_plugin_identity(&client.keys.spki);
    // 完整关闭一次：移包 + 撤权 + 身份退役全部完成（Off 终态）。
    f.service.disable().unwrap();
    f.host.active.store(false, Ordering::SeqCst);
    wait_view(&f.service, "关闭完成", |v| v.phase == SwitchPhase::Off).await;

    // —— 手术构造「成功回执落盘后二次崩溃、补完作业未跑」的盘面：身份目录与
    // 登记锚残留、原作业回执回退到非终态 Verifying、开关记录仍指原作业 ——
    // （Off 终态下驱动器已自行退场，无需代次逼退。）
    let old_digest = auth::key_digest(&client.keys.spki);
    f.write_plugin_identity(&client.keys.spki);
    f.registry
        .compare_and_set_fingerprint(&f.instance_id, "", &old_digest)
        .unwrap();
    let mgmt = f.management_ledger_file();
    let job_id = {
        let ledger: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&mgmt).unwrap()).unwrap();
        ledger["jobs"][0]["jobId"]
            .as_str()
            .expect("成功回执已落盘")
            .to_owned()
    };
    let record_path = f.service.record_path.clone();
    let mut record: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&record_path).unwrap()).expect("开关记录可解析");
    record["phase"] = serde_json::json!("waiting_uninstall");
    record["jobId"] = serde_json::json!(job_id);
    record["jobKind"] = serde_json::json!("uninstall");
    std::fs::write(&record_path, serde_json::to_vec(&record).unwrap()).unwrap();
    let mut mgmt_ledger: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&mgmt).unwrap()).unwrap();
    mgmt_ledger["jobs"][0]["phase"] = serde_json::json!("verifying");
    std::fs::write(&mgmt, serde_json::to_vec(&mgmt_ledger).unwrap()).unwrap();

    // 宿主保持**在场**：受理的补完作业停在退出等待门，绝不运行到身份退役段
    // ——这正是「成功回执已落盘、补完作业未完成」的二次崩溃等价面。
    f.host.active.store(true, Ordering::SeqCst);
    let fresh = f.fresh_manager();
    fresh
        .start(ManagementKind::Uninstall)
        .expect("受理补完卸载作业");
    // 轮询原回执落 Succeeded；落地时身份目录与登记锚必须已经退役（成功回执
    // 不得先于事实）。
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        let ledger: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&mgmt).unwrap()).unwrap();
        let settled = ledger["jobs"].as_array().is_some_and(|rows| {
            rows.iter()
                .any(|row| row["jobId"] == serde_json::json!(job_id) && row["phase"] == "succeeded")
        });
        if settled {
            assert!(
                !f.plugin_identity_file().exists(),
                "成功回执落地时身份目录必须已退役（不得先发布成功再依赖另一作业）"
            );
            assert!(
                f.registry
                    .registered(&f.instance_id)
                    .unwrap()
                    .identity_fingerprint
                    .is_empty(),
                "成功回执落地时登记锚必须已清空"
            );
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "等待遗留卸载对账落成功回执超时: {ledger:?}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// 终审 Important-1：Failed(HomeEvidence)（撤权**之前**回填失败——旧 Bound 行、
/// 受信连接、已交付凭据都还活着）未纳入 close_pending 时，enable 直落
/// enabled=true → 执行门立即恢复放行，旧绑定未经撤权/卸载/退役重新获得写权限。
/// 本矩阵断言：三门（enable/repair/直调 lifecycle）一致拒绝 + 执行门旁路面
/// （Failed 态保持拒绝、被拒的 enable 不得让门恢复）+ 重启装载面记录原样 +
/// 真 Off 清理口不回退。
#[tokio::test]
async fn home_evidence_failure_blocks_enable_repair_direct_lifecycle_and_keeps_gate_shut() {
    let f = Fixture::new_legacy();
    f.seed_plugin_installed();
    f.service.enable().unwrap();
    wait_intent_armed(&f).await;
    let client =
        HandshakeClient::connect_with_seed(&f.factory, b"p12-hegate-key-seed-000000000000").await;
    let reply = client
        .hello(&f, b"p12-hegate-nonce-000000000000000")
        .await
        .unwrap();
    assert_eq!(server_state(&reply), "authorized", "首轮意图自动授权");
    wait_view(&f.service, "就绪", |v| v.phase == SwitchPhase::Ready).await;
    // 注入：绑定行入口只剩物理形态 → disable 的撤权前回填失败 → Failed(HomeEvidence)。
    let ledger_path = f.bindings_ledger_file();
    let mut ledger: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&ledger_path).unwrap()).unwrap();
    ledger["bindings"][0]["profileDir"] =
        serde_json::json!(f.profile.to_string_lossy().to_string());
    std::fs::write(&ledger_path, serde_json::to_vec(&ledger).unwrap()).unwrap();

    let view = f.service.disable().unwrap();
    assert_eq!(view.phase, SwitchPhase::Failed);
    assert_eq!(view.failed_step, Some(SwitchStep::HomeEvidence));
    assert!(!view.enabled);
    // 旁路面基线：关闭侧失败态执行门必须保持拒绝（enabled=false 未变）。
    assert_eq!(
        f.service.execution_gate().unwrap_err().code,
        codes::SWITCH_OFF,
        "Failed(HomeEvidence) 态执行门保持拒绝"
    );
    assert_eq!(
        f.service.authorization_gate_check(None).unwrap_err().code,
        codes::SWITCH_OFF,
        "授权提交点同样保持拒绝"
    );
    // 撤权未跑：绑定行仍 Bound（旁路即复活的现场）。
    let ledger: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&ledger_path).unwrap()).unwrap();
    assert_eq!(
        ledger["bindings"][0]["state"], "bound",
        "HomeEvidence 失败未进撤权段（旧绑定活体在场）"
    );
    let record_before = f.service.read().unwrap();

    // ① enable 拒绝（本回归要堵的缺口）。
    let error = f
        .service
        .enable()
        .expect_err("Failed(HomeEvidence) 态 enable 必须拒绝（未撤权旧绑定不得复活）");
    assert_eq!(
        error.code,
        codes::SWITCH_REVOKE_PENDING,
        "关闭侧 Failed 统一走撤权未完成码: {error}"
    );
    assert_eq!(
        f.service.read().unwrap(),
        record_before,
        "被拒的 enable 不得改写关闭侧记录"
    );
    // 执行门不得因被拒的 enable 恢复放行。
    assert_eq!(
        f.service.execution_gate().unwrap_err().code,
        codes::SWITCH_OFF,
        "被拒的 enable 后执行门仍保持拒绝"
    );

    // ② repair 拒绝（作业前置拦截，记录与作业面零副作用）。
    let error = f
        .service
        .repair()
        .expect_err("Failed(HomeEvidence) 态 repair 必须拒绝");
    assert_eq!(error.code, codes::SWITCH_ACTIVE);
    assert_eq!(f.service.read().unwrap(), record_before);
    assert!(
        f.manager.status().unwrap().operation.is_none(),
        "被拒的 repair 不得启动任何管理作业"
    );

    // ③ 直调生命周期命令拒绝。
    let record = f.service.read_raw().unwrap();
    assert!(record.is_some(), "前置：盘上开关记录在场");
    let error = gate_lifecycle(record.as_ref()).expect_err("close_pending 态直调必须拒绝");
    assert_eq!(error.code, codes::SWITCH_ACTIVE);

    // 重启装载面：重新实例化开关服务后记录仍 Failed(HomeEvidence)、门仍拒。
    let service2 = Arc::new(
        RemoteSwitchService::new(
            Arc::clone(&f.env),
            Arc::clone(&f.registry),
            f.instance_id.clone(),
            Arc::clone(&f.services),
            Arc::clone(&f.evidence_source),
        )
        .unwrap(),
    );
    let record2 = service2.read().unwrap();
    assert_eq!(record2.phase, SwitchPhase::Failed);
    assert_eq!(record2.failed_step, Some(SwitchStep::HomeEvidence));
    assert!(record2.close_pending(), "重启装载后判据不漂移");
    let error = gate_lifecycle(Some(&record2)).expect_err("重启后直调仍拒绝");
    assert_eq!(error.code, codes::SWITCH_ACTIVE);

    // 真 Off 清理口不回退：终态关闭记录的直调放行（既有语义，防判据扩大化）。
    let off = SwitchRecord::default_with(SwitchPhase::Off);
    assert!(gate_lifecycle(Some(&off)).is_ok());
}

/// 终审 Important-1 组合链（全程推演无卡死）：回填失败 → 三门拒 → 修复证据
/// （绑定行入口恢复逻辑形态）→ 重试 disable（回填过、撤权收敛、卸载+退役）→
/// 再开启（新钥 TOFU 就绪）→ 旧钥被墓碑拒绝。
#[tokio::test]
async fn home_evidence_close_converges_after_evidence_repair_then_reenable_rejects_old_key() {
    let f = Fixture::new_legacy();
    f.seed_plugin_installed();
    f.service.enable().unwrap();
    wait_intent_armed(&f).await;
    let old =
        HandshakeClient::connect_with_seed(&f.factory, b"p12-hechain-key-seed-00000000000").await;
    let reply = old
        .hello(&f, b"p12-hechain-nonce-00000000000000")
        .await
        .unwrap();
    assert_eq!(server_state(&reply), "authorized", "首轮意图自动授权");
    wait_view(&f.service, "就绪", |v| v.phase == SwitchPhase::Ready).await;
    let old_digest = auth::key_digest(&old.keys.spki);

    // 回填失败 → Failed(HomeEvidence) → 三门拒。
    let ledger_path = f.bindings_ledger_file();
    let mut ledger: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&ledger_path).unwrap()).unwrap();
    ledger["bindings"][0]["profileDir"] =
        serde_json::json!(f.profile.to_string_lossy().to_string());
    std::fs::write(&ledger_path, serde_json::to_vec(&ledger).unwrap()).unwrap();
    let view = f.service.disable().unwrap();
    assert_eq!(view.failed_step, Some(SwitchStep::HomeEvidence));
    assert!(f.service.enable().is_err(), "enable 必须先完成关闭");
    assert!(f.service.repair().is_err(), "repair 必须先完成关闭");
    let record = f.service.read_raw().unwrap();
    assert!(
        gate_lifecycle(record.as_ref()).is_err(),
        "直调必须先完成关闭"
    );

    // 修复证据：绑定行入口恢复逻辑形态（唯一可推导 + 摘要对位）。
    let mut ledger: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&ledger_path).unwrap()).unwrap();
    ledger["bindings"][0]["profileDir"] =
        serde_json::json!(f.home.join("profiles/desktop").to_string_lossy());
    std::fs::write(&ledger_path, serde_json::to_vec(&ledger).unwrap()).unwrap();

    // 重试 disable：回填过 → 撤权收敛 → 等待卸载；宿主退场后卸载 + 退役。
    f.host.active.store(false, Ordering::SeqCst);
    let view = f.service.disable().unwrap();
    assert_eq!(view.phase, SwitchPhase::WaitingUninstall);
    wait_view(&f.service, "关闭完成", |v| v.phase == SwitchPhase::Off).await;
    let registered = f.registry.registered(&f.instance_id).unwrap();
    assert_eq!(
        registered.dsh_home.as_ref(),
        Some(&f.canonical_home()),
        "重试 disable 必须完成撤权前回填"
    );
    assert!(registered.identity_fingerprint.is_empty(), "卸载退役清锚");
    assert_eq!(
        registered.read_evidence_fingerprint, old_digest,
        "只读证据保留"
    );
    let ledger: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&ledger_path).unwrap()).unwrap();
    assert_eq!(
        ledger["bindings"][0]["state"], "revoked",
        "重试 disable 必须把撤权收敛（墓碑落盘）"
    );

    // 再开启：重装 → 等待重开 → 宿主回归 → 新钥 TOFU 就绪。
    f.service.enable().unwrap();
    wait_view(&f.service, "等待重开完成重装", |v| {
        v.phase == SwitchPhase::AwaitingReopen
    })
    .await;
    f.host.active.store(true, Ordering::SeqCst);
    wait_intent_armed(&f).await;
    let new = HandshakeClient::connect(&f.factory).await;
    let reply = new
        .hello(&f, b"p12-hechain-new-nonce-0000000000")
        .await
        .unwrap();
    assert_eq!(server_state(&reply), "authorized", "新钥首绑 TOFU 自动授权");
    wait_view(&f.service, "重开后就绪", |v| {
        v.phase == SwitchPhase::Ready
    })
    .await;
    // 执行门随就绪恢复放行（关闭已收敛的对照面）。
    assert!(f.service.execution_gate().is_ok(), "收敛后执行门恢复放行");

    // 旧钥被拒：撤权墓碑拦截，未经退役的旧钥不得重新获得写权限——服务端以
    // `revoked` 态应答（永不到 authorized，也不给核对码）。
    let old_retry =
        HandshakeClient::connect_with_seed(&f.factory, b"p12-hechain-key-seed-00000000000").await;
    let rejected = old_retry
        .hello(&f, b"p12-hechain-old-nonce-0000000000")
        .await
        .unwrap();
    assert_eq!(
        server_state(&rejected),
        "revoked",
        "旧钥必须被撤权墓碑拦成 revoked 态: {rejected:?}"
    );
}

// F09：失败回执、宿主退出闸门与现场重启恢复。
async fn f09_ready() -> Fixture {
    let f = Fixture::new();
    f.seed_plugin_installed();
    f.seed_bound_binding();
    f.service.enable().unwrap();
    wait_view(&f.service, "就绪", |v| v.phase == SwitchPhase::Ready).await;
    f
}

async fn f09_fail_remove(f: &Fixture) -> SwitchView {
    f.runner.fail_remove.store(true, Ordering::SeqCst);
    f.host.active.store(false, Ordering::SeqCst);
    f.service.disable().unwrap();
    wait_view(&f.service, "卸载失败", |v| {
        v.phase == SwitchPhase::Failed
    })
    .await
}

async fn f09_waiting_to_uninstall(f: &Fixture) -> SwitchView {
    f.service.disable().unwrap();
    wait_view(&f.service, "等待宿主退出", |v| {
        v.job
            .as_ref()
            .is_some_and(|job| job.phase == ManagementPhase::WaitingForDesktopExit)
    })
    .await
}

async fn f09_stays_waiting(f: &Fixture) {
    let deadline = std::time::Instant::now() + Duration::from_millis(400);
    let mut samples = Vec::new();
    loop {
        samples.push((
            f.service.view().unwrap(),
            f.runner.removes.load(Ordering::SeqCst),
        ));
        if std::time::Instant::now() >= deadline {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    for (view, _) in &samples {
        assert_eq!(
            view.phase,
            SwitchPhase::WaitingUninstall,
            "reason: {:?}",
            view.reason_code
        );
    }
    for (view, removes) in samples {
        assert_eq!(
            view.job.as_ref().map(|job| job.phase),
            Some(ManagementPhase::WaitingForDesktopExit)
        );
        assert_eq!(removes, 0);
    }
}

fn f09_extra_host(f: &Fixture, profile: Option<&Path>) {
    let mut cmdline = vec![
        f.host.exe.to_string_lossy().into_owned(),
        "dsh-desktop-host/lib/index.js".into(),
    ];
    if let Some(profile) = profile {
        cmdline.push(profile.to_string_lossy().into_owned());
    }
    let mut extra = f.host.extra.lock().unwrap();
    extra.push(DshProcess {
        pid: HOST_PID + 1,
        exe: f.host.exe.clone(),
        cmdline,
        started_at_ms: HOST_STARTED_AT,
    });
    f.host.active.store(false, Ordering::SeqCst);
}

#[tokio::test]
async fn failed_uninstall_exposes_real_terminal_job_state() {
    let f = f09_ready().await;
    let view = f09_fail_remove(&f).await;
    let job = view.job.expect("必须显示真实失败的卸载作业");
    assert_eq!(
        job.phase,
        ManagementPhase::Failed,
        "终态作业不能伪装成 Queued"
    );
    assert_eq!(job.kind, ManagementKind::Uninstall);
    assert_eq!(Some(job.job_id), f.service.read().unwrap().job_id);
    assert_eq!(job.reason_code.as_deref(), Some(codes::CLI_FAILED));
}

#[tokio::test]
async fn unknown_recorded_job_id_never_fabricates_active_job() {
    let f = Fixture::new();
    std::fs::create_dir_all(f.service.record_path.parent().unwrap()).unwrap();
    std::fs::write(&f.service.record_path, br#"{"enabled":false,"phase":"failed","failedStep":"uninstall","jobId":"mgmt-1760000000000-999","jobKind":"uninstall","generation":1}"#).unwrap();
    assert!(!f.management_ledger_file().exists());
    let view = f.service.view().unwrap();
    assert_eq!(view.phase, SwitchPhase::Failed);
    assert_eq!(view.failed_step, Some(SwitchStep::Uninstall));
    assert!(
        view.job.is_none(),
        "未知作业 ID 不得伪造活跃作业: {:?}",
        view.job
    );
}

#[tokio::test]
async fn active_uninstall_job_reports_real_progress_and_dedupes_retry() {
    let f = f09_ready().await;
    let view = f09_waiting_to_uninstall(&f).await;
    assert!(matches!(
        view.job.unwrap().phase,
        ManagementPhase::Queued
            | ManagementPhase::WaitingForDesktopExit
            | ManagementPhase::Applying
            | ManagementPhase::Verifying
    ));
    for _ in 0..3 {
        assert_ne!(f.service.disable().unwrap().phase, SwitchPhase::Failed);
    }
    f.host.active.store(false, Ordering::SeqCst);
    wait_view(&f.service, "关闭完成", |v| {
        assert_ne!(v.phase, SwitchPhase::Failed, "reason: {:?}", v.reason_code);
        v.phase == SwitchPhase::Off
    })
    .await;
    assert_eq!(f.runner.removes.load(Ordering::SeqCst), 1);
}

#[tokio::test(flavor = "current_thread")]
async fn succeeded_uninstall_anchor_survives_disable_until_off() {
    let f = f09_ready().await;
    let job_id = f09_waiting_to_uninstall(&f).await.job.unwrap().job_id;
    f.host.active.store(false, Ordering::SeqCst);
    wait_view(&f.service, "关闭完成", |v| v.phase == SwitchPhase::Off).await;
    let removes_baseline = f.runner.removes.load(Ordering::SeqCst);
    assert_eq!(removes_baseline, 1);
    assert_eq!(
        f.fresh_manager().job(&job_id).unwrap().phase,
        ManagementPhase::Succeeded
    );
    // 抵达 Off 后驱动器已自行退场；等它从登记表摘除，避免「刚读到旧条目」。
    // （不能等 exit_count 再涨——那一代早已计数过，不会再有第二次退场。）
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while drivers().lock().unwrap().contains_key(&f.instance_id) {
        assert!(
            std::time::Instant::now() < deadline,
            "等待 Off 后驱动器摘除超时"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // 旧代驱动器已退场，直接重建作业已成功、开关尚未提交 Off 的窗口。
    let path = &f.service.record_path;
    let mut record: serde_json::Value =
        serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    record["generation"] = (record["generation"].as_u64().unwrap() + 1).into();
    record["phase"] = "waiting_uninstall".into();
    record["jobId"] = job_id.clone().into();
    record["jobKind"] = "uninstall".into();
    std::fs::write(path, serde_json::to_vec(&record).unwrap()).unwrap();

    // 单线程运行时在下次 await 前不会调度新驱动器，盘上仍应有锚。
    let job = f
        .service
        .disable()
        .unwrap()
        .job
        .expect("必须保留成功作业的锚");
    assert_eq!(job.job_id, job_id);
    assert_eq!(job.phase, ManagementPhase::Succeeded);
    let persisted = f.service.read().unwrap();
    assert_eq!(persisted.job_id.as_deref(), Some(job_id.as_str()));
    assert_eq!(persisted.job_kind, Some(ManagementKind::Uninstall));

    wait_view(&f.service, "复用成功作业后关闭完成", |v| {
        v.phase == SwitchPhase::Off
    })
    .await;
    assert_eq!(f.runner.removes.load(Ordering::SeqCst), removes_baseline);
}

#[test]
fn job_lookup_falls_back_to_ledger_written_after_assembly() {
    let f = Fixture::new();
    let manager = f.fresh_manager();
    let path = f.management_ledger_file();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let job_id = "after-assembly-failed";
    let mut ledger = serde_json::json!({"jobs": [{
        "jobId": job_id,
        "kind": "uninstall",
        "phase": "failed",
        "reasonCode": "DSH_CLI_FAILED",
        "updatedAtMs": 2
    }]});
    std::fs::write(&path, serde_json::to_vec(&ledger).unwrap()).unwrap();
    let job = manager.job(job_id).unwrap();
    assert_eq!(job.phase, ManagementPhase::Failed);
    assert_eq!(job.reason_code.as_deref(), Some("DSH_CLI_FAILED"));

    ledger["jobs"][0]["phase"] = "applying".into();
    ledger["jobs"][0]
        .as_object_mut()
        .unwrap()
        .remove("reasonCode");
    std::fs::write(&path, serde_json::to_vec(&ledger).unwrap()).unwrap();
    assert_eq!(manager.job(job_id).unwrap_err().code, codes::JOB_UNKNOWN);

    let invalid = b"{ not json";
    std::fs::write(&path, invalid).unwrap();
    assert_eq!(manager.job(job_id).unwrap_err().code, codes::JOB_UNKNOWN);
    assert_eq!(std::fs::read(&path).unwrap(), invalid);
}

#[test]
fn packaged_web_cli_does_not_block_desktop_uninstall() {
    use identity::{observe_desktop_exit, DesktopExitObservation};

    struct ProcessTable(Vec<DshProcess>);
    impl DshProcessSource for ProcessTable {
        fn list_processes(&self) -> Vec<DshProcess> {
            self.0.clone()
        }
        fn listening_sockets(&self) -> Vec<DshListener> {
            vec![]
        }
    }

    struct NoCommands;
    impl DshCommandRunner for NoCommands {
        fn run(&self, _: &CommandRequest) -> Result<RunOutcome, DshError> {
            panic!("退出观察不得执行命令");
        }
    }

    let tmp = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(tmp.path()).unwrap();
    let home = root.join("dsh-home");
    let profile = home.join("profiles/desktop");
    let foreign = root.join("foreign-home/profiles/desktop");
    for dir in [&profile, &foreign] {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join("package.json"), b"{}").unwrap();
    }
    let target_id = instances::instance_id_for(HostKind::Desktop, &profile);
    let mut env = (*crate::services::dsh_control::production::production_env()).clone();
    env.home = home;
    env.tuvis_home = root.join("tuvis");
    env.dsh_state_dir = Some(root.join("state"));
    env.files = Arc::new(FixtureFiles(root));
    env.runner = Arc::new(NoCommands);
    env.environments = Arc::new(TestUnobservableEnvironments);

    let exe = r"C:\Program Files\DeepSeek Harness\DeepSeek Harness.exe";
    let mut observe = |cmdline| {
        env.processes = Arc::new(ProcessTable(vec![DshProcess {
            pid: HOST_PID,
            exe: PathBuf::from(exe),
            cmdline,
            started_at_ms: HOST_STARTED_AT,
        }]));
        observe_desktop_exit(&env, &target_id)
    };
    // Windows 实测 argv（windows-report.md §3.2）：web 不紧邻 cli.js。
    let packaged_argv = |command: &str| {
        [
            exe,
            "--expose-internals",
            r"C:\Program Files\DeepSeek Harness\resources\app.asar\dsh\node_modules\@deepseek-ai\dsh-desktop-host\lib\cli.js",
            "--profile",
            "tuvisprobe",
            "--from-default-profile",
            command,
            "--no-open",
        ]
        .map(str::to_owned)
        .to_vec()
    };
    let desktop_argv = |dir: &Path| {
        vec![
            exe.into(),
            "dsh-desktop-host/lib/index.js".into(),
            dir.to_str().unwrap().into(),
        ]
    };
    assert_eq!(observe(packaged_argv("web")), DesktopExitObservation::Gone);
    assert_eq!(
        observe(packaged_argv("desktop")),
        DesktopExitObservation::Unproven
    );
    assert_eq!(
        observe(desktop_argv(&profile)),
        DesktopExitObservation::TargetAlive
    );
    assert_eq!(
        observe(desktop_argv(&foreign)),
        DesktopExitObservation::Gone
    );
}

#[tokio::test]
async fn listener_loss_while_process_alive_keeps_waiting_then_uninstalls_once() {
    let f = f09_ready().await;
    f09_waiting_to_uninstall(&f).await;
    assert_eq!(f.runner.removes.load(Ordering::SeqCst), 0);
    f.host.listener_active.store(false, Ordering::SeqCst);
    f09_stays_waiting(&f).await;
    f.host.active.store(false, Ordering::SeqCst);
    wait_view(&f.service, "关闭完成", |v| v.phase == SwitchPhase::Off).await;
    assert_eq!(f.runner.removes.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn unattributable_desktop_host_blocks_uninstall() {
    let f = f09_ready().await;
    f09_waiting_to_uninstall(&f).await;
    f09_extra_host(&f, None);
    f09_stays_waiting(&f).await;
    f.host.extra.lock().unwrap().clear();
    wait_view(&f.service, "关闭完成", |v| v.phase == SwitchPhase::Off).await;
    assert_eq!(f.runner.removes.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn foreign_instance_host_does_not_block_target_uninstall() {
    let f = f09_ready().await;
    f09_waiting_to_uninstall(&f).await;
    let foreign = f.root.join("foreign-home/profiles/desktop");
    std::fs::create_dir_all(&foreign).unwrap();
    std::fs::write(foreign.join("package.json"), b"{}").unwrap();
    f09_extra_host(&f, Some(&foreign));
    wait_view(&f.service, "关闭完成", |v| {
        assert_ne!(v.phase, SwitchPhase::Failed, "reason: {:?}", v.reason_code);
        v.phase == SwitchPhase::Off
    })
    .await;
    assert_eq!(f.runner.removes.load(Ordering::SeqCst), 1);
    let profiles = f.runner.remove_profiles.lock().unwrap();
    assert_eq!(profiles.len(), 1);
    assert_eq!(
        profiles[0].canonicalize().unwrap(),
        f.profile.canonicalize().unwrap()
    );
}

#[tokio::test]
async fn uninstall_failure_preserves_job_reason_code() {
    let f = f09_ready().await;
    let view = f09_fail_remove(&f).await;
    assert_eq!(view.reason_code.as_deref(), Some(codes::CLI_FAILED));
    assert_ne!(view.reason_code.as_deref(), Some(codes::STATE_IO));
}

#[tokio::test]
async fn uninstall_ledger_failure_reports_state_io() {
    let f = f09_ready().await;
    let ledger = f.management_ledger_file();
    std::fs::create_dir_all(ledger.parent().unwrap()).unwrap();
    std::fs::write(ledger, b"{ not json").unwrap();
    f.host.active.store(false, Ordering::SeqCst);
    f.service.disable().unwrap();
    let view = wait_view(&f.service, "账本损坏导致卸载失败", |v| {
        v.phase == SwitchPhase::Failed
    })
    .await;
    assert_eq!(view.failed_step, Some(SwitchStep::Uninstall));
    assert_eq!(view.reason_code.as_deref(), Some(codes::STATE_IO));
}

#[tokio::test]
async fn legacy_failed_uninstall_record_recovers_through_state_machine() {
    let f = Fixture::new();
    f.seed_plugin_installed();
    f.seed_history_session("session-aaaaaaaa");
    let first =
        HandshakeClient::connect_with_seed(&f.factory, b"f09-first-key-seed-0000000000000").await;
    f.write_plugin_identity(&first.keys.spki);
    f.service.enable().unwrap();
    wait_intent_armed(&f).await;
    let reply = first
        .hello(&f, b"f09-first-nonce-0000000000000000")
        .await
        .unwrap();
    assert_eq!(server_state(&reply), "authorized");
    wait_view(&f.service, "就绪", |v| v.phase == SwitchPhase::Ready).await;
    f09_fail_remove(&f).await;
    let job_id = f
        .service
        .read()
        .unwrap()
        .job_id
        .expect("失败作业 ID 已落盘");
    assert!(f.plugin_identity_file().exists());
    assert!(f.runner.installed.load(Ordering::SeqCst));

    let manager = f.fresh_manager();
    assert_eq!(manager.job(&job_id).unwrap().phase, ManagementPhase::Failed);
    let auth = Arc::clone(&f.auth);
    let services: Arc<SwitchServices> =
        Arc::new(move || Ok((Arc::clone(&manager), Arc::clone(&auth))));
    let restarted = Arc::new(
        RemoteSwitchService::new(
            Arc::clone(&f.env),
            Arc::clone(&f.registry),
            f.instance_id.clone(),
            services,
            Arc::clone(&f.evidence_source),
        )
        .unwrap()
        .with_poll(Duration::from_millis(20)),
    );
    let view = restarted.view().unwrap();
    assert_eq!(view.phase, SwitchPhase::Failed);
    assert_eq!(view.failed_step, Some(SwitchStep::Uninstall));
    let job = view.job.expect("重启后必须显示真实失败的卸载作业");
    assert_eq!(
        job.phase,
        ManagementPhase::Failed,
        "重启后不能把失败作业伪装成 Queued"
    );
    assert_eq!(job.job_id, job_id);
    assert_eq!(view.reason_code.as_deref(), Some(codes::CLI_FAILED));

    let baseline = f.runner.removes.load(Ordering::SeqCst);
    f.runner.fail_remove.store(false, Ordering::SeqCst);
    restarted.disable().unwrap();
    wait_view(&restarted, "重试卸载完成", |v| {
        v.phase == SwitchPhase::Off
    })
    .await;
    assert_eq!(f.runner.removes.load(Ordering::SeqCst), baseline + 1);

    restarted.enable().unwrap();
    wait_view(&restarted, "等待重开", |v| {
        v.phase == SwitchPhase::AwaitingReopen
    })
    .await;
    let second = HandshakeClient::connect(&f.factory).await;
    f.write_plugin_identity(&second.keys.spki);
    f.host.active.store(true, Ordering::SeqCst);
    wait_view(&restarted, "授权中", |v| {
        v.phase == SwitchPhase::Authorizing
    })
    .await;
    wait_intent_armed(&f).await;
    let reply = second
        .hello(&f, b"f09-second-nonce-000000000000000")
        .await
        .unwrap();
    assert_eq!(server_state(&reply), "authorized");
    let view = wait_view(&restarted, "重新就绪", |v| {
        v.phase == SwitchPhase::Ready
    })
    .await;
    assert!(!view.rebind_required);

    restarted.disable().unwrap();
    f.host.active.store(false, Ordering::SeqCst);
    wait_view(&restarted, "再次关闭完成", |v| {
        v.phase == SwitchPhase::Off
    })
    .await;
    let foreign_home = f.root.join("foreign-home");
    let foreign_profile = foreign_home.join("profiles/desktop");
    std::fs::create_dir_all(&foreign_profile).unwrap();
    std::fs::write(foreign_profile.join("package.json"), b"{}").unwrap();
    f.registry
        .register_candidate_with_home(HostKind::Desktop, &foreign_profile, Some(&foreign_home))
        .unwrap();
    let foreign_session = foreign_home.join("sessions/project/session-zzzzzzzz");
    std::fs::create_dir_all(&foreign_session).unwrap();
    std::fs::write(
        foreign_session.join("session.jsonl"),
        b"{\"id\":\"session-zzzzzzzz\"}\n",
    )
    .unwrap();
    let readonly = f.readonly_registry();
    assert_eq!(
        readonly
            .resolve_readonly_root(&f.instance_id, "session-aaaaaaaa")
            .unwrap(),
        f.canonical_home()
    );
    assert_eq!(
        readonly
            .resolve_readonly_root(&f.instance_id, "session-zzzzzzzz")
            .unwrap_err()
            .code,
        codes::SESSION_OWNERSHIP_UNPROVEN
    );
}
