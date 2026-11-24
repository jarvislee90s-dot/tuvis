//! R01 复审：公开发现/授权入口的 Web home 行为合同。
//!
//! 独立 Node 子进程各持有真实 TCP；PID/启动时刻/exe/监听属主取自生产 OS 适配器，
//! 仅按本测试的子 PID 过滤，绝不读写真实用户 home。环境证据由夹具显式注入，
//! 不验证 OS 的环境读取完整性。绑定用例额外要求 macOS 内核进程代次。
//! 插件 BindingSession 负责 Ed25519 签名，服务端走
//! 生产 UDS、发现和审批。没有 verified/指纹/Bound 行预置。
//!
//! 这是隔离测试宿主，不宣称官方 DSH 的配置优先级或 Windows 已验收；覆盖两种
//! 注入证据：Complete + Unset + HOME/.dsh，以及绝对自定义 DSH_HOME。
//! RED 必须由本文件真正运行后认定；构建/夹具/OS 前置条件错误不算行为 RED。
#![cfg(unix)]

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

use multi_agents_manager_lib::services::dsh_control::auth::{self, DshAuthService};
use multi_agents_manager_lib::services::dsh_control::error::DshError;
use multi_agents_manager_lib::services::dsh_control::identity;
use multi_agents_manager_lib::services::dsh_control::instances::{instance_id_for, HostKind};
use multi_agents_manager_lib::services::dsh_control::model::{
    AuthorizationState, CommandRequest, DshCommandRunner, DshEnvironment, DshListener, DshProcess,
    DshProcessSource, ProcessEnvironmentEvidence, ProcessEnvironmentReader, RunOutcome,
};
use multi_agents_manager_lib::services::dsh_control::process_environment::{
    AllowlistedEnvironment, EnvironmentField, ObservedProcessIdentity, ReadCompleteness,
};
use multi_agents_manager_lib::services::dsh_control::production::{
    self, FsFileSource, SysinfoProcessSource, SystemClock,
};

const WAIT: Duration = Duration::from_secs(10);

/// 不编造进程或监听，只限制测试可见 PID。每次重建源，避免生产 TTL 掩盖换代。
struct OwnedKernelProcesses(BTreeSet<u32>);
impl DshProcessSource for OwnedKernelProcesses {
    fn list_processes(&self) -> Vec<DshProcess> {
        SysinfoProcessSource::new()
            .list_processes()
            .into_iter()
            .filter(|p| self.0.contains(&p.pid))
            .collect()
    }

    fn listening_sockets(&self) -> Vec<DshListener> {
        SysinfoProcessSource::new()
            .listening_sockets()
            .into_iter()
            .filter(|l| self.0.contains(&l.owner_pid))
            .collect()
    }
}

/// 只为本测试拥有的子进程提供显式启动环境证据，不读随机 PID。
struct OwnedEnvironmentEvidence(std::collections::HashMap<u32, ProcessEnvironmentEvidence>);
impl ProcessEnvironmentReader for OwnedEnvironmentEvidence {
    fn observed_environment(&self, pid: u32) -> Option<ProcessEnvironmentEvidence> {
        self.0.get(&pid).cloned()
    }
}

struct NoCommands;
impl DshCommandRunner for NoCommands {
    fn run(&self, _: &CommandRequest) -> Result<RunOutcome, DshError> {
        panic!("R01 归属测试不得运行宿主 CLI 或模型")
    }
}

struct Web {
    child: Child,
    input: ChildStdin,
    output: Lines<BufReader<ChildStdout>>,
    pid: u32,
    authority: String,
    home: PathBuf,
    custom_home: bool,
}

impl Web {
    async fn spawn(user_home: &Path, custom_home: Option<&Path>) -> Self {
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/r01-web/dsh");
        let plugin =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../packages/dsh-remote-access/src");
        std::fs::create_dir_all(user_home).unwrap();
        let mut command = Command::new("node");
        command
            .arg(fixture)
            .arg("web")
            .arg(plugin)
            .env_remove("DSH_HOME")
            .env_remove("NODE_OPTIONS")
            .env_remove("NODE_PATH")
            .env("HOME", user_home)
            .env("USERPROFILE", user_home)
            .current_dir(user_home)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::inherit())
            .kill_on_drop(true);
        if let Some(home) = custom_home {
            command.env("DSH_HOME", home);
        }
        let mut child = command
            .spawn()
            .expect("夹具前置条件：支持 module hooks 的 Node");
        let input = child.stdin.take().unwrap();
        let mut output = BufReader::new(child.stdout.take().unwrap()).lines();
        let line = tokio::time::timeout(WAIT, output.next_line())
            .await
            .expect("夹具前置条件：Node 就绪有界")
            .unwrap()
            .expect("夹具前置条件：Node 不得提前退出");
        let ready: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(ready["event"], "ready", "夹具前置条件：{ready}");
        let pid = child.id().unwrap();
        assert_eq!(ready["pid"], pid);
        assert_eq!(ready["dshHomeSet"], custom_home.is_some());
        let home = std::fs::canonicalize(
            custom_home
                .map(Path::to_path_buf)
                .unwrap_or_else(|| user_home.join(".dsh")),
        )
        .unwrap();
        assert_eq!(Path::new(ready["home"].as_str().unwrap()), home);
        Self {
            child,
            input,
            output,
            pid,
            authority: ready["authority"].as_str().unwrap().into(),
            home,
            custom_home: custom_home.is_some(),
        }
    }

    async fn send(&mut self, value: Value) {
        self.input
            .write_all(format!("{value}\n").as_bytes())
            .await
            .unwrap();
        self.input.flush().await.unwrap();
    }

    async fn event(&mut self, kind: &str, attempt: &str) -> Value {
        tokio::time::timeout(WAIT, async {
            loop {
                let line = self
                    .output
                    .next_line()
                    .await
                    .unwrap()
                    .unwrap_or_else(|| panic!("夹具提前退出：等待 {kind}/{attempt}"));
                let value: Value = serde_json::from_str(&line).unwrap();
                if value["event"] == kind && value["attempt"] == attempt {
                    return value;
                }
            }
        })
        .await
        .unwrap_or_else(|_| panic!("等待 {kind}/{attempt} 超时，不能记为预期行为 RED"))
    }

    async fn hello(&mut self, rv: &Value, attempt: &str, changes: Value) -> (Value, Value) {
        let mut command = json!({ "op": "begin", "attempt": attempt, "rendezvous": rv });
        command
            .as_object_mut()
            .unwrap()
            .extend(changes.as_object().unwrap().clone());
        self.send(command).await;
        let sent = self.event("sent", attempt).await;
        assert_eq!(sent["frame"]["type"], "hello");
        let reply = self.event("reply", attempt).await;
        (sent["frame"].clone(), reply["frame"].clone())
    }

    async fn stop(&mut self, attempt: &str) {
        self.send(json!({ "op": "stop", "attempt": attempt })).await;
        self.event("stopped", attempt).await;
    }
}

impl Drop for Web {
    fn drop(&mut self) {
        let _ = self.child.start_kill();
    }
}

struct Bench {
    env: DshEnvironment,
    webs: Vec<Web>,
    // 声明在子进程之后，析构时先关子进程再删隔离根。
    _dir: tempfile::TempDir,
}

impl Bench {
    /// mode: default / custom / pair。env.home 可刻意指向有标记但无宿主的残留目录。
    async fn new(mode: &str, decoy_home: bool) -> Self {
        let dir = tempfile::Builder::new()
            .prefix("r01w-")
            .tempdir_in("/tmp")
            .unwrap();
        let mut webs = Vec::new();
        if mode != "custom" {
            webs.push(Web::spawn(&dir.path().join("默认用户 home"), None).await);
        }
        if mode != "default" {
            webs.push(
                Web::spawn(
                    &dir.path().join("自定义用户 home"),
                    Some(&dir.path().join("自定义 实际 home")),
                )
                .await,
            );
        }
        let home = if decoy_home {
            let decoy = dir.path().join("无宿主的残留 home");
            std::fs::create_dir_all(decoy.join("profiles/web")).unwrap();
            std::fs::write(decoy.join("profiles/web/package.json"), b"{}").unwrap();
            decoy
        } else {
            webs[0].home.clone()
        };
        let processes = Arc::new(OwnedKernelProcesses(webs.iter().map(|w| w.pid).collect()));
        let snapshot = processes.list_processes();
        let mut evidence = std::collections::HashMap::new();
        for web in &webs {
            let process = snapshot
                .iter()
                .find(|p| p.pid == web.pid)
                .expect("专属子进程必须可观测");
            #[cfg(target_os = "macos")]
            let native_generation = {
                use multi_agents_manager_lib::services::dsh_control::process_environment::{
                    MacOsProcessEnvironmentReader, ProcessEnvironmentReader as RawReader,
                };
                let actual = MacOsProcessEnvironmentReader::default()
                    .identity_now(web.pid)
                    .expect("专属子进程身份应可读取")
                    .expect("专属子进程仍存活");
                assert_eq!(actual.started_at_ms, process.started_at_ms);
                assert_eq!(actual.executable, process.exe);
                actual.native_generation
            };
            #[cfg(not(target_os = "macos"))]
            let native_generation = None;
            let identity = ObservedProcessIdentity {
                pid: process.pid,
                started_at_ms: process.started_at_ms,
                executable: process.exe.clone(),
                native_generation,
            };
            let environment = if web.custom_home {
                AllowlistedEnvironment {
                    dsh_home: EnvironmentField::Value(web.home.to_string_lossy().into_owned()),
                    home: EnvironmentField::Unset,
                }
            } else {
                AllowlistedEnvironment {
                    dsh_home: EnvironmentField::Unset,
                    home: EnvironmentField::Value(
                        web.home.parent().unwrap().to_string_lossy().into_owned(),
                    ),
                }
            };
            evidence.insert(
                web.pid,
                ProcessEnvironmentEvidence {
                    identity_before: identity.clone(),
                    identity_after: identity,
                    completeness: ReadCompleteness::Complete,
                    environment,
                },
            );
        }
        let env = DshEnvironment {
            // macOS /tmp 为符号链接；比较生产规范路径，避免 no-guess 断言假通过。
            home: std::fs::canonicalize(home).unwrap(),
            tuvis_home: dir.path().join("tuvis"),
            // 短路径避免 macOS sun_path 上限，auth 状态仍只落本测试临时根。
            dsh_state_dir: Some(dir.path().join("s")),
            processes,
            files: Arc::new(FsFileSource),
            runner: Arc::new(NoCommands),
            clock: Arc::new(SystemClock),
            // 完整性和字段来自启动夹具，不能作为系统环境读取能力的验收证据。
            environments: Arc::new(OwnedEnvironmentEvidence(evidence)),
        };
        // 独立前置条件：确保失败来自生产决策，而非宿主未启动/监听属主缺失。
        let processes = env.processes.list_processes();
        let listeners = env.processes.listening_sockets();
        for web in &webs {
            let process = processes
                .iter()
                .find(|p| p.pid == web.pid)
                .expect("OS 前置条件：子 PID 可观测");
            assert!(process.started_at_ms > 0, "OS 前置条件：启动代际可读");
            assert!(!process.exe.as_os_str().is_empty(), "OS 前置条件：exe 可读");
            assert!(process.cmdline.iter().any(|a| a.ends_with("/dsh")));
            assert!(process.cmdline.iter().any(|a| a == "web"));
            assert!(
                listeners
                    .iter()
                    .any(|l| l.owner_pid == web.pid && l.authority == web.authority),
                "OS 前置条件：真实独立监听必须归子 PID 所有；检查 lsof 可用性"
            );
        }
        Self {
            env,
            webs,
            _dir: dir,
        }
    }

    async fn service(&self, index: usize) -> (Arc<DshAuthService>, Value) {
        let web = &self.webs[index];
        let id = instance_id_for(HostKind::Web, &web.home.join("profiles/web"));
        let svc = Arc::new(production::init_instance_auth(&self.env, &id).unwrap());
        assert_eq!(svc.authorization_state(), AuthorizationState::Unbound);
        let ledger = auth::instance_state_dir(&self.env, &id)
            .unwrap()
            .join("bindings.json");
        assert!(!ledger.exists(), "必须从无绑定账本开始，不能预置可信状态");
        let result = svc.serve().await;
        assert!(
            result.is_ok(),
            "R01：有真实 PID/监听/profile 的未受信 Web 必须可发布受限首握手入口；{result:?}"
        );
        let state = auth::instance_state_dir(&self.env, &id).unwrap();
        let rv: Value =
            serde_json::from_slice(&std::fs::read(state.join("rendezvous.json")).unwrap()).unwrap();
        assert_eq!(rv["processIdentity"]["pid"], web.pid);
        let process = self
            .env
            .processes
            .list_processes()
            .into_iter()
            .find(|p| p.pid == web.pid)
            .unwrap();
        assert_eq!(rv["processIdentity"]["startedAtMs"], process.started_at_ms);
        assert_eq!(
            rv["processIdentity"]["executable"],
            process.exe.to_string_lossy().as_ref()
        );
        (svc, rv)
    }
}

/// 与 TS 实现独立的逐字段消息构造；真实公钥/签名取自实际发送帧。
fn signature_valid(hello: &Value, rv: &Value) -> bool {
    let client_spki = auth::b64url_decode(hello["publicKeySpki"].as_str().unwrap()).unwrap();
    let rust_spki = auth::b64url_decode(rv["publicKeySpki"].as_str().unwrap()).unwrap();
    let h = &hello["homeBinding"];
    // json! 的 preserve_order Map 插入顺序可能按宏展开变化，显式数组拼接固定 wire 序。
    let message = format!(
        "{{\"bindingProtocol\":2,\"keyDigests\":[{},{}],\"nonce\":{},\"hostId\":{},\"epoch\":{},\"expiry\":{},\"homeBinding\":{{\"homeRoot\":{},\"hostPid\":{},\"hostStartedAtMs\":{}}}}}",
        json!(auth::key_digest(&rust_spki)), json!(auth::key_digest(&client_spki)),
        hello["nonce"], hello["hostId"], hello["epoch"], hello["expiry"],
        h["homeRoot"], h["hostPid"], h["hostStartedAtMs"],
    );
    let key = VerifyingKey::from_bytes(&auth::spki_unwrap(&client_spki).unwrap()).unwrap();
    let bytes = auth::b64url_decode(hello["signature"].as_str().unwrap()).unwrap();
    key.verify(message.as_bytes(), &Signature::from_slice(&bytes).unwrap())
        .is_ok()
}

async fn bind(web: &mut Web, svc: &Arc<DshAuthService>, rv: &Value) -> String {
    assert_eq!(svc.current_generation_key_digest().unwrap(), None);
    let (hello, reply) = web.hello(rv, "valid", json!({})).await;
    assert!(
        signature_valid(&hello, rv),
        "前置条件：插件实际发出的签名有效"
    );
    assert_eq!(hello["pid"], web.pid);
    assert_eq!(hello["homeBinding"]["hostPid"], web.pid);
    assert_eq!(
        hello["homeBinding"]["homeRoot"],
        web.home.to_string_lossy().as_ref()
    );
    assert_eq!(
        hello["homeBinding"]["hostStartedAtMs"],
        rv["processIdentity"]["startedAtMs"]
    );
    assert_eq!(
        reply["type"], "server_hello",
        "R01：有效 Web v2 首握手必须可达待批，不能用拒绝全部 Web 代替归属证明：{reply}"
    );
    assert_eq!(reply["state"], "pending_approval");
    assert_eq!(reply["homeBinding"], hello["homeBinding"]);
    assert_eq!(svc.authorization_state(), AuthorizationState::Unbound);
    assert_eq!(
        svc.current_generation_key_digest().unwrap(),
        None,
        "待批不能成为受信证明"
    );
    let binding = reply["bindingId"].as_str().unwrap();
    svc.approve(binding)
        .await
        .expect("有效归属经公开审批入口绑定");
    let granted = web.event("reply", "valid").await;
    assert_eq!(granted["frame"]["type"], "granted");
    assert_eq!(svc.authorization_state(), AuthorizationState::Bound);
    let digest =
        auth::key_digest(&auth::b64url_decode(hello["publicKeySpki"].as_str().unwrap()).unwrap());
    assert_eq!(
        svc.current_generation_key_digest().unwrap(),
        Some(digest.clone())
    );
    digest
}

/// R01 晋升：批准后**该实例的 Web 归属必须已核验**，使凭据交付不再被
/// `INSTANCE_UNVERIFIED` 挡住；撤权后证明作废（重新绑定需再证明）。
#[cfg(target_os = "macos")]
#[tokio::test]
async fn approval_promotes_the_observed_web_host_for_this_generation() {
    let mut b = Bench::new("default", false).await;
    let (svc, rv) = b.service(0).await;
    let id = rv["hostId"].as_str().unwrap().to_owned();
    let _digest = bind(&mut b.webs[0], &svc, &rv).await;

    // 晋升的核心断言：同一代际重复解析时 verified 必须为 true。
    let promoted = svc.web_home_verified_for_test(&id).expect("晋升查询失败");
    assert!(
        promoted,
        "R01：批准后该代 Web 归属必须被视为已核验（否则凭据交付永远被拒）"
    );
}

/// 撤权必须作废 Web 归属证明：重新绑定要重新走环境证明，不能沿用旧结论。
#[cfg(target_os = "macos")]
#[tokio::test]
async fn revocation_clears_the_promoted_web_proof() {
    let mut b = Bench::new("default", false).await;
    let (svc, rv) = b.service(0).await;
    let id = rv["hostId"].as_str().unwrap().to_owned();
    let digest = bind(&mut b.webs[0], &svc, &rv).await;
    // 找到实际 binding id 并撤权。
    let binding = svc.active_binding_for_test(&id).expect("绑定存在");
    svc.revoke(&binding).await.expect("撤权受理");
    assert_eq!(svc.authorization_state(), AuthorizationState::Revoked);
    assert!(
        !svc.web_home_verified_for_test(&id).unwrap_or(false),
        "撤权后必须作废旧代际的 Web 归属证明"
    );
    let _ = digest;
}

#[tokio::test]
async fn fixture_exposes_two_distinct_kernel_owned_web_listeners() {
    let b = Bench::new("pair", false).await;
    assert_ne!(b.webs[0].pid, b.webs[1].pid);
    assert_ne!(b.webs[0].authority, b.webs[1].authority);
    assert_ne!(b.webs[0].home, b.webs[1].home);
}

#[tokio::test]
async fn discover_detailed_keeps_both_raw_web_processes_with_independent_listeners() {
    let b = Bench::new("pair", true).await;
    let discovery = identity::discover_detailed(&b.env).unwrap();
    // P1-1b 之后的口径：真实 Web 进程**要么**晋升为带 home 的候选（启动环境可
    // 观测 → 归属被正面证明），**要么**留在 raw 里。两者合起来必须恰好覆盖每个
    // 真实 PID，不重复、不丢失——这才是「不猜但也不丢事实」的真正不变量。
    let promoted: BTreeSet<u32> = discovery.candidates.iter().map(|c| c.host.pid).collect();
    let raw: BTreeSet<u32> = discovery
        .raw_web_hosts
        .iter()
        .map(|r| r.process.pid)
        .collect();
    assert!(
        promoted.is_disjoint(&raw),
        "同一进程不得既晋升为候选又留在 raw：promoted={promoted:?} raw={raw:?}"
    );
    let covered: BTreeSet<u32> = promoted.union(&raw).copied().collect();
    assert_eq!(
        covered,
        b.webs.iter().map(|w| w.pid).collect::<BTreeSet<_>>(),
        "两个真实 Web 必须恰好覆盖一次（不截断、不重复）"
    );
    assert_eq!(promoted.len() + raw.len(), 2, "两个 Web 不得截断或重复");
    // 两个不同 home 的 Web 各自独立：晋升时必须是两个不同实例 ID。
    let instance_ids: BTreeSet<_> = discovery
        .candidates
        .iter()
        .map(|c| c.instance_id.clone())
        .collect();
    assert_eq!(
        instance_ids.len(),
        discovery.candidates.len(),
        "不同 home 的 Web 必须派生不同实例 ID"
    );
    let processes = b.env.processes.list_processes();
    let listeners = b.env.processes.listening_sockets();
    for web in &b.webs {
        let process = processes.iter().find(|p| p.pid == web.pid).unwrap();
        // 不论走哪条路，进程事实（启动代际/exe）与监听属主都必须如实。
        if let Some(raw) = discovery
            .raw_web_hosts
            .iter()
            .find(|raw| raw.process.pid == web.pid)
        {
            assert_eq!(raw.process.started_at_ms, process.started_at_ms);
            assert_eq!(raw.process.exe, process.exe);
            let actual_listeners: BTreeSet<_> = raw
                .listeners
                .iter()
                .map(|l| (l.owner_pid, l.authority.clone()))
                .collect();
            assert!(
                actual_listeners.contains(&(web.pid, web.authority.clone())),
                "raw 事实里的监听必须归对应内核 PID"
            );
        } else {
            let candidate = discovery
                .candidates
                .iter()
                .find(|c| c.host.pid == web.pid)
                .expect("每个真实 Web 必在候选或 raw 中出现一次");
            assert_eq!(candidate.host.started_at_ms, process.started_at_ms);
            assert_eq!(candidate.host.executable, process.exe);
            assert_eq!(candidate.kind, HostKind::Web, "Web 候选的 kind 必须是 Web");
            assert!(candidate.verified, "晋升出来的 Web 候选必须是已核验归属");
            assert_eq!(
                candidate.host.authority, web.authority,
                "候选 authority 必须就是该进程的真实监听"
            );
        }
        let owned_listeners: BTreeSet<_> = listeners
            .iter()
            .filter(|l| l.owner_pid == web.pid)
            .map(|l| (l.owner_pid, l.authority.clone()))
            .collect();
        assert!(
            owned_listeners.contains(&(web.pid, web.authority.clone())),
            "OS 前置条件：真实独立监听必须归子 PID 所有"
        );
    }
}

/// 核心 no-guess 不变量（P1-1b 之后重写）：
/// 残留的 `env.home/profiles/web` 标记**绝不能**被当成归属证据。
///
/// 旧断言是「候选必须恒为空」——那是「Web 永不晋升」时代的产物，直接把功能缺口
/// 写成期望。现在的判据更强也更准确：如果这个 Web 晋升了，它派生的实例 ID 必须
/// **来自为该进程注入的 home 证据**，而不是 `env.home` 里那个带标记的残留目录。
#[tokio::test]
async fn discovery_does_not_assign_custom_web_to_an_unrelated_marked_env_home() {
    let b = Bench::new("custom", true).await;
    let d = identity::discover_detailed(&b.env).unwrap();
    let web = &b.webs[0];
    let process = b
        .env
        .processes
        .list_processes()
        .into_iter()
        .find(|p| p.pid == web.pid)
        .unwrap();

    // 真实进程事实必须在两处之一出现（不丢事实）。
    let raw = d.raw_web_hosts.iter().find(|r| r.process.pid == web.pid);
    let candidate = d.candidates.iter().find(|c| c.host.pid == web.pid);
    assert!(
        raw.is_some() ^ candidate.is_some(),
        "真实 Web 进程必须恰好在候选或 raw 中出现一次"
    );
    if let Some(raw) = raw {
        assert_eq!(raw.process.started_at_ms, process.started_at_ms);
        assert_eq!(raw.process.exe, process.exe);
        assert!(!raw.listeners.is_empty(), "原始进程应保留已知真实监听");
        assert!(raw.listeners.iter().all(|l| l.owner_pid == web.pid));
        assert!(raw.listeners.iter().any(|l| l.authority == web.authority));
    }

    // no-guess 的真正判据：任何晋升出来的 Web 候选都不能落在 env.home 的标记目录上。
    let env_home_instance = instance_id_for(HostKind::Web, &b.env.home.join("profiles/web"));
    for c in d.candidates.iter().filter(|c| c.kind == HostKind::Web) {
        assert_ne!(
            c.instance_id, env_home_instance,
            "残留 env.home 的 profiles/web 标记不得生成猜测归属候选"
        );
        // profile 布局恒为 `<home>/profiles/web` → 上两级即该候选声明的 home。
        let derived_home = c
            .profile_dir
            .parent()
            .and_then(|profiles| profiles.parent())
            .expect("候选 profile 必须在 <home>/profiles/web 布局下");
        assert_eq!(
            derived_home,
            b.webs
                .iter()
                .find(|w| w.pid == c.host.pid)
                .map(|w| w.home.as_path())
                .expect("候选必须对应一个真实 Web 进程"),
            "候选 home 必须等于该进程注入证据中的启动 home，不能是 env.home 推导值"
        );
    }
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn unset_dsh_home_can_bind_the_default_profile_using_real_peer_and_plugin_signature() {
    let mut b = Bench::new("default", false).await;
    let (svc, rv) = b.service(0).await;
    bind(&mut b.webs[0], &svc, &rv).await;
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn custom_home_can_enter_first_binding_when_tuvis_home_points_elsewhere() {
    let mut b = Bench::new("custom", true).await;
    let (svc, rv) = b.service(0).await;
    bind(&mut b.webs[0], &svc, &rv).await;
}

async fn other_home_refused(from: usize, to: usize) {
    let mut b = Bench::new("pair", true).await;
    let (a, rv_a) = b.service(0).await;
    let (z, rv_z) = b.service(1).await;
    let key_a = bind(&mut b.webs[0], &a, &rv_a).await;
    let key_z = bind(&mut b.webs[1], &z, &rv_z).await;
    assert_ne!(key_a, key_z, "两个真实插件实例使用独立身份");
    let services = [a, z];
    let records = [rv_a, rv_z];
    let roots: Vec<_> = b
        .webs
        .iter()
        .map(|w| w.home.to_string_lossy().into_owned())
        .collect();
    let paths: Vec<_> = records
        .iter()
        .map(|rv| {
            auth::instance_state_dir(&b.env, rv["hostId"].as_str().unwrap())
                .unwrap()
                .join("bindings.json")
        })
        .collect();
    let before: Vec<_> = paths.iter().map(|p| std::fs::read(p).unwrap()).collect();
    let (hello, reply) = b.webs[from]
        .hello(
            &records[from],
            "cross-home",
            json!({ "claimHome": roots[to] }),
        )
        .await;
    assert!(
        signature_valid(&hello, &records[from]),
        "互报反例必须携带真实有效插件签名"
    );
    assert_eq!(
        reply["type"], "error",
        "R01：有效签名不能把 A PID 的实际 home 换成 B：{reply}"
    );
    assert!(services[from].pending_binding().is_none());
    for (path, bytes) in paths.iter().zip(before) {
        assert_eq!(
            std::fs::read(path).unwrap(),
            bytes,
            "互报拒绝不能改写任一实例账本"
        );
    }
    assert_eq!(
        services[0].current_generation_key_digest().unwrap(),
        Some(key_a)
    );
    assert_eq!(
        services[1].current_generation_key_digest().unwrap(),
        Some(key_z)
    );
    b.webs[from].stop("cross-home").await;
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn default_web_cannot_claim_custom_web_home_even_with_a_valid_plugin_signature() {
    other_home_refused(0, 1).await;
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn custom_web_cannot_claim_default_web_home_even_with_a_valid_plugin_signature() {
    other_home_refused(1, 0).await;
}

async fn rejected_after_valid_binding(changes: Value, expect_valid_signature: bool) {
    let mut b = Bench::new("default", false).await;
    let (svc, rv) = b.service(0).await;
    let digest = bind(&mut b.webs[0], &svc, &rv).await;
    let path = auth::instance_state_dir(&b.env, rv["hostId"].as_str().unwrap())
        .unwrap()
        .join("bindings.json");
    let before = std::fs::read(&path).unwrap();
    // 同一进程的新插件实例：清本地身份/信任锚，确保这次真的重新发 hello
    // （保留已绑定身份会直接进读循环，测试只会看到超时）。
    let mut proof = changes.as_object().cloned().unwrap_or_default();
    proof.insert("freshIdentity".into(), json!(true));
    let (hello, reply) = b.webs[0]
        .hello(&rv, "invalid-proof", Value::Object(proof))
        .await;
    assert_eq!(signature_valid(&hello, &rv), expect_valid_signature);
    assert_eq!(
        reply["type"], "error",
        "R01：非法归属证明不得再进入绑定分支：{reply}"
    );
    assert!(svc.pending_binding().is_none());
    assert_eq!(std::fs::read(path).unwrap(), before);
    assert_eq!(svc.current_generation_key_digest().unwrap(), Some(digest));
    b.webs[0].stop("invalid-proof").await;
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn signed_home_binding_with_another_pid_is_refused_after_a_valid_binding() {
    // 自报 PID 整份伪造（rv + homeBinding 同步），才能到达服务端核验点。
    rejected_after_valid_binding(
        json!({ "claimBindingPid": std::process::id(), "claimPeerPid": std::process::id(), "forgeRendezvousIdentity": true }),
        true,
    )
    .await;
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn signed_home_binding_with_another_start_generation_is_refused_after_a_valid_binding() {
    rejected_after_valid_binding(
        json!({ "claimStartedAtMs": 1, "forgeRendezvousIdentity": true }),
        true,
    )
    .await;
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn hello_cannot_replace_the_kernel_peer_pid_with_a_signed_claim() {
    rejected_after_valid_binding(
        json!({
            "claimPeerPid": std::process::id(),
            "claimBindingPid": std::process::id(),
            "forgeRendezvousIdentity": true
        }),
        true,
    )
    .await;
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn correct_home_and_kernel_identity_cannot_bind_with_an_invalid_plugin_signature() {
    rejected_after_valid_binding(json!({ "breakSignature": true }), false).await;
}
