//! OpenCode 服务插件桥（B1 可信初始绑定）。
//!
//! 设计边界（2026-11-02 决策）：
//! * 用户在 GUI 明确选择固定的 **service + session**；桥只证明“这个 session 由这个
//!   已登记的服务进程提供”，不跟随桌面前台、不要求每次导航都撤销授权。
//! * 服务内一次性安装的插件通过 Unix socket 连接本进程。兔维斯用内核 peer 事实
//!   （PID/UID/启动时间/可执行路径）核对连接确实来自登记文件里的 service PID，
//!   插件再用自己的 `ctx.session` 复核 session/directory；两侧一致才建立绑定。
//! * 插件回执不能阻止同进程恶意插件的先行副作用；本轮产品边界是可信宿主 + 明确
//!   安装的桥代码，不把“抵御 OpenCode 进程内任意恶意代码”加入基础收发的无限门槛。
//! * 断链/服务重启/插件重载后旧绑定失效；已 claim 的动作只读核对，不自动重放。
//!
//! 该模块只提供证明与确认，不直接执行会话写；写仍走现有 `NativeHub` 动作账本和
//! HTTP 一次派发，接收侧在 reserve/claim 前调用这里的 `confirm_session`。

use std::collections::HashMap;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{oneshot, Mutex};

use crate::services::opencode_native::discovery::discover;
use crate::services::opencode_native::types::{
    codes, DiscoveryInput, NativeError, RuntimeKey, ServiceDescriptor,
};

const MAX_FRAME_BYTES: usize = 64 * 1024;
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
const CONFIRM_TIMEOUT: Duration = Duration::from_secs(3);

/// 桥的文件布局。生产用 `~/.tuvis/opencode-bridge`；测试显式注入临时根。
#[derive(Clone, Debug)]
pub struct BridgePaths {
    pub dir: PathBuf,
    pub socket: PathBuf,
    pub config: PathBuf,
    /// OpenCode `plugins` 里登记的目录（含 `index.js`）；loader 接受目录形式。
    pub plugin_dir: PathBuf,
    /// 目录内的实际入口文件。
    pub plugin_file: PathBuf,
}

impl BridgePaths {
    pub fn production(home: &Path) -> Self {
        let dir = home.join(".tuvis").join("opencode-bridge");
        Self::in_dir(dir)
    }

    pub fn in_dir(dir: PathBuf) -> Self {
        let plugin_dir = dir.join("plugin");
        Self {
            socket: dir.join("bridge.sock"),
            config: dir.join("bridge.json"),
            plugin_file: plugin_dir.join("index.js"),
            plugin_dir,
            dir,
        }
    }
}

/// 已由插件确认的固定目标。
#[derive(Clone, Debug)]
pub struct ConfirmedTarget {
    pub runtime: RuntimeKey,
    pub incarnation: String,
    pub location: PathBuf,
}

/// 归属证明接口。生产是 [`BridgeRuntime`]；测试可注入静态证明，但生产构造默认无证明。
pub trait OwnershipProof: Send + Sync {
    fn attested(&self) -> bool;
    fn runtime(&self) -> Option<RuntimeKey>;
    fn confirm<'a>(
        &'a self,
        session_id: &'a str,
        directory: &'a Path,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<ConfirmedTarget, NativeError>> + Send + 'a>,
    >;
}

struct BridgeInner {
    paths: BridgePaths,
    token: String,
    input: DiscoveryInput,
    process_fact: Arc<dyn Fn(u32) -> Option<i64> + Send + Sync>,
    state: std::sync::Mutex<BridgeState>,
    listener: Mutex<Option<tokio::task::JoinHandle<()>>>,
    stopped: AtomicBool,
    #[cfg(test)]
    test_peer_bypass: bool,
}

#[derive(Default)]
struct BridgeState {
    connection: Option<Arc<Connection>>,
    last_error: Option<String>,
}

struct Connection {
    id: String,
    runtime: RuntimeKey,
    incarnation: String,
    location: PathBuf,
    peer_pid: u32,
    writer: Arc<Mutex<OwnedWriteHalf>>,
    pending: Arc<Mutex<HashMap<String, oneshot::Sender<Value>>>>,
    /// 停止/坏连接关闭信号：reader 的 read_frame 可能永久阻塞，必须能被打断。
    closed: tokio::sync::Notify,
}

/// 生产服务插件桥。`start` 只监听本地 Unix socket；安装/卸载插件由
/// [`super::bridge_install`] 的 GUI 流程负责，本类型不改 OpenCode 配置。
pub struct BridgeRuntime {
    inner: Arc<BridgeInner>,
}

impl BridgeRuntime {
    pub async fn start(
        paths: BridgePaths,
        input: DiscoveryInput,
        process_fact: Arc<dyn Fn(u32) -> Option<i64> + Send + Sync>,
    ) -> Result<Arc<Self>, String> {
        Self::start_inner(paths, input, process_fact, false).await
    }

    #[cfg(test)]
    pub(crate) async fn start_for_test(
        paths: BridgePaths,
        input: DiscoveryInput,
        process_fact: Arc<dyn Fn(u32) -> Option<i64> + Send + Sync>,
    ) -> Result<Arc<Self>, String> {
        Self::start_inner(paths, input, process_fact, true).await
    }

    async fn start_inner(
        paths: BridgePaths,
        input: DiscoveryInput,
        process_fact: Arc<dyn Fn(u32) -> Option<i64> + Send + Sync>,
        test_peer_bypass: bool,
    ) -> Result<Arc<Self>, String> {
        #[cfg(not(test))]
        let _ = test_peer_bypass;
        std::fs::create_dir_all(&paths.dir).map_err(|e| format!("bridge dir: {e}"))?;
        set_private_dir(&paths.dir)?;
        if paths.socket.exists() {
            std::fs::remove_file(&paths.socket).map_err(|e| format!("remove stale socket: {e}"))?;
        }
        // 复用上次安装的 token：Tuvis 重启后服务进程里的插件仍持有旧配置；
        // 每次随机会让插件的 hello 永久被拒，直到用户重启 OpenCode 服务。
        let token = std::fs::read(&paths.config)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
            .and_then(|value| {
                value
                    .get("token")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            })
            .filter(|token| token.len() >= 32)
            .unwrap_or_else(random_token);
        write_private(
            &paths.config,
            serde_json::to_vec(&json!({
                "socket": paths.socket.to_string_lossy(),
                "token": token,
            }))
            .map_err(|e| format!("bridge config: {e}"))?,
        )?;
        let listener =
            UnixListener::bind(&paths.socket).map_err(|e| format!("bind bridge socket: {e}"))?;
        set_private_socket(&paths.socket)?;

        let inner = Arc::new(BridgeInner {
            paths,
            token,
            input,
            process_fact,
            state: std::sync::Mutex::new(BridgeState::default()),
            listener: Mutex::new(None),
            stopped: AtomicBool::new(false),
            #[cfg(test)]
            test_peer_bypass,
        });
        let runtime = Arc::new(Self {
            inner: Arc::clone(&inner),
        });
        let accept_inner = Arc::clone(&inner);
        let task = tokio::spawn(async move {
            loop {
                if accept_inner.stopped.load(Ordering::SeqCst) {
                    break;
                }
                match listener.accept().await {
                    Ok((stream, _)) => {
                        let inner = Arc::clone(&accept_inner);
                        tokio::spawn(async move {
                            #[cfg(test)]
                            let test_peer_bypass = inner.test_peer_bypass;
                            #[cfg(not(test))]
                            let test_peer_bypass = false;
                            if let Err(error) =
                                handle_connection(inner, stream, test_peer_bypass).await
                            {
                                log::warn!("opencode bridge handshake rejected: {error}");
                            }
                        });
                    }
                    Err(error) => {
                        if accept_inner.stopped.load(Ordering::SeqCst) {
                            break;
                        }
                        log::warn!("opencode bridge accept failed: {error}");
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }
                }
            }
        });
        *runtime.inner.listener.lock().await = Some(task);
        Ok(runtime)
    }

    pub async fn shutdown(&self) {
        self.inner.stopped.store(true, Ordering::SeqCst);
        if let Some(task) = self.inner.listener.lock().await.take() {
            task.abort();
            let _ = task.await;
        }
        let active = self
            .inner
            .state
            .lock()
            .unwrap()
            .connection
            .clone()
            .map(|connection| connection.id.clone());
        if let Some(id) = active {
            // 关闭写半端并唤醒 reader，让插件收到断开；不能只清内存状态。
            self.close_connection(&id, "bridge stopped").await;
        }
        let _ = std::fs::remove_file(&self.inner.paths.socket);
    }

    pub fn is_stopped(&self) -> bool {
        self.inner.stopped.load(Ordering::SeqCst)
    }

    pub fn config_path(&self) -> &Path {
        &self.inner.paths.config
    }

    pub fn socket_path(&self) -> &Path {
        &self.inner.paths.socket
    }

    pub fn plugin_path(&self) -> &Path {
        &self.inner.paths.plugin_file
    }

    pub fn plugin_dir(&self) -> &Path {
        &self.inner.paths.plugin_dir
    }

    pub fn dir(&self) -> &Path {
        &self.inner.paths.dir
    }

    pub async fn status(&self) -> BridgeStatus {
        let state = self.inner.state.lock().unwrap();
        BridgeStatus {
            attested: state.connection.is_some(),
            runtime: state.connection.as_ref().map(|c| c.runtime.clone()),
            incarnation: state.connection.as_ref().map(|c| c.incarnation.clone()),
            location: state
                .connection
                .as_ref()
                .map(|c| c.location.to_string_lossy().into_owned()),
            peer_pid: state.connection.as_ref().map(|c| c.peer_pid),
            last_error: state.last_error.clone(),
        }
    }
}

#[derive(Clone, Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BridgeStatus {
    pub attested: bool,
    pub runtime: Option<RuntimeKey>,
    pub incarnation: Option<String>,
    pub location: Option<String>,
    pub peer_pid: Option<u32>,
    pub last_error: Option<String>,
}

impl OwnershipProof for BridgeRuntime {
    fn attested(&self) -> bool {
        self.inner
            .state
            .lock()
            .map(|state| state.connection.is_some())
            .unwrap_or(false)
    }

    fn runtime(&self) -> Option<RuntimeKey> {
        self.inner
            .state
            .lock()
            .ok()
            .and_then(|state| state.connection.as_ref().map(|c| c.runtime.clone()))
    }

    fn confirm<'a>(
        &'a self,
        session_id: &'a str,
        directory: &'a Path,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<ConfirmedTarget, NativeError>> + Send + 'a>,
    > {
        Box::pin(async move { self.confirm_session(session_id, directory).await })
    }
}

impl BridgeRuntime {
    async fn clear_connection(&self, connection_id: &str, reason: &str) {
        let mut state = self.inner.state.lock().unwrap();
        if state
            .connection
            .as_ref()
            .is_some_and(|c| c.id == connection_id)
        {
            state.connection = None;
            state.last_error = Some(reason.to_string());
        }
    }

    /// 断掉坏连接：只清状态而不关 stream 会让插件永远等在这个 socket 上，
    /// 不会重连（confirm 超时后的死状态）。先 shutdown 写半端，读循环随 EOF 退出。
    async fn close_connection(&self, connection_id: &str, reason: &str) {
        let connection = {
            let state = self.inner.state.lock().unwrap();
            state.connection.clone()
        };
        if let Some(connection) = connection {
            if connection.id == connection_id {
                connection.closed.notify_waiters();
                let mut writer = connection.writer.lock().await;
                let _ = writer.shutdown().await;
                drop(writer);
            }
        }
        self.clear_connection(connection_id, reason).await;
    }

    async fn confirm_session(
        &self,
        session_id: &str,
        directory: &Path,
    ) -> Result<ConfirmedTarget, NativeError> {
        let connection = {
            let state = self.inner.state.lock().unwrap();
            state.connection.clone()
        }
        .ok_or_else(bridge_unavailable)?;
        let request_id = random_token();
        let (tx, rx) = oneshot::channel();
        connection
            .pending
            .lock()
            .await
            .insert(request_id.clone(), tx);
        let frame = json!({
            "type": "confirm_session",
            "requestId": request_id,
            "sessionID": session_id,
            "directory": directory.to_string_lossy(),
        });
        let mut line = serde_json::to_vec(&frame)
            .map_err(|error| NativeError::new(codes::OC_RESPONSE_INVALID, &error.to_string()))?;
        line.push(b'\n');
        {
            let mut writer = connection.writer.lock().await;
            if let Err(error) = tokio::time::timeout(CONFIRM_TIMEOUT, writer.write_all(&line)).await
            {
                drop(writer);
                connection.pending.lock().await.remove(&request_id);
                self.close_connection(&connection.id, "bridge write timeout")
                    .await;
                let _ = error;
                return Err(bridge_unavailable());
            }
        }
        let response = match tokio::time::timeout(CONFIRM_TIMEOUT, rx).await {
            Ok(Ok(value)) => value,
            _ => {
                connection.pending.lock().await.remove(&request_id);
                self.close_connection(&connection.id, "bridge confirm timeout")
                    .await;
                return Err(bridge_unavailable());
            }
        };
        if response.get("ok").and_then(Value::as_bool) != Some(true) {
            let reason = response
                .get("reason")
                .and_then(Value::as_str)
                .unwrap_or("target rejected");
            return Err(NativeError::new(
                codes::OC_INSTANCE_OWNERSHIP_UNVERIFIED,
                &format!("bridge rejected target: {reason}"),
            ));
        }
        let target = response.get("session").ok_or_else(bridge_unavailable)?;
        let id = target
            .get("id")
            .and_then(Value::as_str)
            .ok_or_else(bridge_unavailable)?;
        if id != session_id {
            return Err(NativeError::new(
                codes::OC_SESSION_MISMATCH,
                "bridge confirmed a different session",
            ));
        }
        let reported = target
            .get("directory")
            .and_then(Value::as_str)
            .ok_or_else(bridge_unavailable)?;
        let expected = canonical(directory).map_err(|_| bridge_unavailable())?;
        let actual = canonical(Path::new(reported)).map_err(|_| bridge_unavailable())?;
        if expected != actual {
            return Err(NativeError::new(
                codes::OC_DIRECTORY_UNRESOLVED,
                "bridge confirmed a different directory",
            ));
        }
        Ok(ConfirmedTarget {
            runtime: connection.runtime.clone(),
            incarnation: connection.incarnation.clone(),
            location: actual,
        })
    }
}

/// 注册已认证连接。停止检查必须与注册在同一个状态锁临界区内完成，否则
/// shutdown 可在“快速检查”和赋值之间清空状态并删 socket，留下活连接。
fn register_connection(inner: &BridgeInner, connection: &Arc<Connection>) -> Result<(), String> {
    let mut state = inner.state.lock().unwrap();
    if inner.stopped.load(Ordering::SeqCst) {
        return Err("bridge stopped before registration".into());
    }
    if state.connection.is_some() {
        return Err("bridge already has an attested connection".into());
    }
    state.connection = Some(Arc::clone(connection));
    state.last_error = None;
    Ok(())
}

async fn handle_connection(
    inner: Arc<BridgeInner>,
    stream: UnixStream,
    test_peer_bypass: bool,
) -> Result<(), String> {
    let fd = stream.as_raw_fd();
    let (read_half, write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);
    let hello_line = read_frame(&mut reader, Some(HANDSHAKE_TIMEOUT)).await?;
    let hello: Value =
        serde_json::from_slice(&hello_line).map_err(|e| format!("bridge hello invalid: {e}"))?;
    if hello.get("type").and_then(Value::as_str) != Some("hello") {
        return Err("bridge first frame is not hello".into());
    }
    let token = hello
        .get("token")
        .and_then(Value::as_str)
        .ok_or("bridge token missing")?;
    if token != inner.token {
        return Err("bridge token mismatch".into());
    }
    let incarnation = hello
        .get("incarnation")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or("bridge incarnation missing")?
        .to_string();
    let location = hello
        .get("location")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or("bridge location missing")?
        .to_string();

    let descriptor = discover(
        &inner.input,
        &|path| {
            std::fs::read(path).map_err(|_| {
                NativeError::new(
                    codes::OC_SERVICE_NOT_REGISTERED,
                    "opencode background service is not registered",
                )
            })
        },
        &*inner.process_fact,
    )
    .map_err(|error| format!("service discovery: {}", error.message))?;
    let peer = peer::inspect(fd, &*inner.process_fact)?;
    if !test_peer_bypass {
        verify_peer(&peer, &descriptor)?;
    }

    // 停止中的桥不得再登记新连接：accept 任务可能在 abort 前已进入握手，
    // 只靠 listener abort 会留下“已停止但仍 attested”的竞态。
    if inner.stopped.load(Ordering::SeqCst) {
        return Err("bridge stopped before registration".into());
    }
    let writer = Arc::new(Mutex::new(write_half));
    let pending = Arc::new(Mutex::new(HashMap::new()));
    let connection = Arc::new(Connection {
        id: random_token(),
        runtime: descriptor.runtime.clone(),
        incarnation: incarnation.clone(),
        location: PathBuf::from(&location),
        peer_pid: peer.pid,
        writer: Arc::clone(&writer),
        pending: Arc::clone(&pending),
        closed: tokio::sync::Notify::new(),
    });
    register_connection(&inner, &connection)?;
    // 回执只是让插件知道握手完成；插件不依赖它做安全决策。
    let ack = serde_json::to_vec(&json!({
        "type": "registered",
        "runtime": descriptor.runtime,
        "location": location,
    }))
    .map_err(|e| e.to_string())?;
    {
        let mut writer = writer.lock().await;
        let mut frame = ack;
        frame.push(b'\n');
        let _ = tokio::time::timeout(HANDSHAKE_TIMEOUT, writer.write_all(&frame)).await;
    }
    let inner_for_reader = Arc::clone(&inner);
    let connection_for_reader = Arc::clone(&connection);
    tokio::spawn(async move {
        let mut closed = Box::pin(connection_for_reader.closed.notified());
        loop {
            let frame = tokio::select! {
                frame = read_frame(&mut reader, None) => match frame {
                    Ok(frame) => frame,
                    Err(_) => break,
                },
                _ = &mut closed => break,
            };
            let Ok(value) = serde_json::from_slice::<Value>(&frame) else {
                continue;
            };
            if value.get("type").and_then(Value::as_str) != Some("confirm_session_result") {
                continue;
            }
            let Some(request_id) = value.get("requestId").and_then(Value::as_str) else {
                continue;
            };
            let sender = connection_for_reader
                .pending
                .lock()
                .await
                .remove(request_id);
            if let Some(sender) = sender {
                let _ = sender.send(value);
            }
        }
        // 只有仍是本连接 id 时才清；同 incarnation 的新连接不能被旧 reader 迟到 EOF 清掉。
        let mut state = inner_for_reader.state.lock().unwrap();
        if state
            .connection
            .as_ref()
            .is_some_and(|c| c.id == connection_for_reader.id)
        {
            state.connection = None;
            state.last_error = Some("bridge disconnected".into());
        }
    });
    Ok(())
}

async fn read_frame(
    reader: &mut BufReader<OwnedReadHalf>,
    timeout: Option<Duration>,
) -> Result<Vec<u8>, String> {
    let mut line = Vec::new();
    let read = match timeout {
        Some(timeout) => tokio::time::timeout(timeout, reader.read_until(b'\n', &mut line))
            .await
            .map_err(|_| "bridge frame timeout".to_string())?
            .map_err(|error| format!("bridge read: {error}"))?,
        None => reader
            .read_until(b'\n', &mut line)
            .await
            .map_err(|error| format!("bridge read: {error}"))?,
    };
    if read == 0 {
        return Err("bridge closed".into());
    }
    if line.len() > MAX_FRAME_BYTES {
        return Err("bridge frame too large".into());
    }
    if line.last() == Some(&b'\n') {
        line.pop();
    }
    Ok(line)
}

#[derive(Debug)]
struct PeerIdentity {
    pid: u32,
    uid: u32,
    started_at_ms: i64,
    executable: PathBuf,
}

fn verify_peer(peer: &PeerIdentity, descriptor: &ServiceDescriptor) -> Result<(), String> {
    if peer.pid != descriptor.runtime.pid {
        return Err(format!(
            "bridge peer pid {} does not match registered service pid {}",
            peer.pid, descriptor.runtime.pid
        ));
    }
    if peer.started_at_ms != descriptor.runtime.started_at_ms {
        return Err("bridge peer start time does not match registration".into());
    }
    if peer.uid != current_uid() {
        return Err("bridge peer uid is not the current user".into());
    }
    let name = peer
        .executable
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or_default();
    if name != "opencode" {
        return Err(format!(
            "bridge peer executable is not the opencode binary: {}",
            peer.executable.display()
        ));
    }
    Ok(())
}

#[cfg(target_os = "macos")]
mod peer {
    use std::ffi::CStr;
    use std::os::fd::RawFd;
    use std::path::PathBuf;

    use super::PeerIdentity;

    const LOCAL_PEERPID: libc::c_int = 0x002;

    pub(super) fn inspect(
        fd: RawFd,
        process_fact: &dyn Fn(u32) -> Option<i64>,
    ) -> Result<PeerIdentity, String> {
        let mut pid: libc::pid_t = 0;
        let mut len = std::mem::size_of::<libc::pid_t>() as libc::socklen_t;
        let rc = unsafe {
            libc::getsockopt(
                fd,
                libc::SOL_LOCAL,
                LOCAL_PEERPID,
                &mut pid as *mut _ as *mut libc::c_void,
                &mut len,
            )
        };
        if rc != 0 || pid <= 0 {
            return Err("bridge peer pid unavailable".into());
        }
        let mut uid: libc::uid_t = 0;
        let mut gid: libc::gid_t = 0;
        if unsafe { libc::getpeereid(fd, &mut uid, &mut gid) } != 0 {
            return Err("bridge peer credentials unavailable".into());
        }
        let started_at_ms = process_fact(pid as u32)
            .ok_or_else(|| "bridge peer process start time unavailable".to_string())?;
        let mut buf = vec![0i8; 4096];
        let path_len =
            unsafe { libc::proc_pidpath(pid, buf.as_mut_ptr() as *mut libc::c_void, 4096) };
        if path_len <= 0 {
            return Err("bridge peer executable unavailable".into());
        }
        let executable = unsafe { CStr::from_ptr(buf.as_ptr()) }
            .to_string_lossy()
            .into_owned();
        Ok(PeerIdentity {
            pid: pid as u32,
            uid,
            started_at_ms,
            executable: PathBuf::from(executable),
        })
    }
}

#[cfg(not(target_os = "macos"))]
mod peer {
    use std::os::fd::RawFd;
    use std::path::PathBuf;

    use super::PeerIdentity;

    pub(super) fn inspect(
        _fd: RawFd,
        _process_fact: &dyn Fn(u32) -> Option<i64>,
    ) -> Result<PeerIdentity, String> {
        Err("openCode bridge peer identity is only implemented on macOS".into())
    }

    #[allow(dead_code)]
    fn _keep_path_type() -> Option<PathBuf> {
        None
    }
}

#[cfg(unix)]
fn current_uid() -> u32 {
    unsafe { libc::geteuid() }
}

#[cfg(not(unix))]
fn current_uid() -> u32 {
    0
}

fn bridge_unavailable() -> NativeError {
    NativeError::new(
        codes::OC_INSTANCE_OWNERSHIP_UNVERIFIED,
        "OpenCode bridge is not attested for this session",
    )
}

fn canonical(path: &Path) -> std::io::Result<PathBuf> {
    path.canonicalize()
}

fn random_token() -> String {
    let mut bytes = [0u8; 24];
    rand::fill(&mut bytes);
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn set_private_dir(path: &Path) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
            .map_err(|e| format!("bridge dir permissions: {e}"))?;
    }
    Ok(())
}

fn set_private_socket(path: &Path) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .map_err(|e| format!("bridge socket permissions: {e}"))?;
    }
    Ok(())
}

fn write_private(path: &Path, bytes: Vec<u8>) -> Result<(), String> {
    let temp = path.with_extension("tmp");
    std::fs::write(&temp, &bytes).map_err(|e| format!("bridge config write: {e}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&temp, std::fs::Permissions::from_mode(0o600))
            .map_err(|e| format!("bridge config permissions: {e}"))?;
    }
    std::fs::rename(&temp, path).map_err(|e| format!("bridge config rename: {e}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    fn registration(pid: u32) -> Vec<u8> {
        serde_json::to_vec(&json!({
            "id": "svc-test",
            "version": "2.2.5",
            "url": "http://127.0.0.1:1",
            "pid": pid,
            "password": "pw",
        }))
        .unwrap()
    }

    #[tokio::test]
    #[cfg_attr(
        not(target_os = "macos"),
        ignore = "requires macOS OpenCode ownership or process semantics"
    )]
    async fn bridge_confirms_session_only_after_hello_and_rejects_after_disconnect() {
        let temp = tempfile::tempdir().unwrap();
        let project = temp.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let project = project.canonicalize().unwrap();
        let state_file = temp.path().join("service.json");
        std::fs::write(&state_file, registration(std::process::id())).unwrap();
        let paths = BridgePaths::in_dir(temp.path().join("bridge"));
        let runtime = BridgeRuntime::start_for_test(
            paths.clone(),
            DiscoveryInput {
                state_file,
                home: temp.path().to_path_buf(),
            },
            Arc::new(|_pid| Some(12_000)),
        )
        .await
        .unwrap();
        assert!(!runtime.attested());
        let config: Value = serde_json::from_slice(&std::fs::read(&paths.config).unwrap()).unwrap();
        let token = config["token"].as_str().unwrap().to_string();
        let socket = runtime.socket_path().to_path_buf();
        let expected_project = project.clone();
        let client = tokio::spawn(async move {
            let stream = UnixStream::connect(&socket).await.unwrap();
            let (read, mut write) = stream.into_split();
            let mut reader = BufReader::new(read);
            write
                .write_all(
                    format!(
                        "{}\n",
                        json!({
                            "type": "hello",
                            "token": token,
                            "incarnation": "inc-1",
                            "location": expected_project,
                        })
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
            let mut line = String::new();
            reader.read_line(&mut line).await.unwrap();
            assert!(line.contains("registered"), "{line}");
            line.clear();
            reader.read_line(&mut line).await.unwrap();
            let request: Value = serde_json::from_str(line.trim()).unwrap();
            assert_eq!(request["type"], "confirm_session");
            let request_id = request["requestId"].as_str().unwrap();
            let reply = json!({
                "type": "confirm_session_result",
                "requestId": request_id,
                "ok": true,
                "session": { "id": "ses-1", "directory": expected_project },
            });
            write
                .write_all(format!("{reply}\n").as_bytes())
                .await
                .unwrap();
            // Keep the connection alive until the assertion path drops it.
            tokio::time::sleep(Duration::from_millis(200)).await;
        });
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(runtime.attested());
        let confirmed = runtime
            .confirm_session("ses-1", &project)
            .await
            .expect("confirm");
        assert_eq!(confirmed.incarnation, "inc-1");
        assert_eq!(confirmed.runtime.service_id, "svc-test");
        client.await.unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(!runtime.attested());
        assert!(runtime.confirm_session("ses-1", &project).await.is_err());
    }

    async fn connect_attested(
        runtime: &BridgeRuntime,
        token: &str,
        project: &Path,
        incarnation: &str,
    ) -> (tokio::task::JoinHandle<()>, Arc<Connection>) {
        let socket = runtime.socket_path().to_path_buf();
        let project = project.to_path_buf();
        let token = token.to_string();
        let incarnation = incarnation.to_string();
        let client = tokio::spawn(async move {
            let stream = UnixStream::connect(&socket).await.unwrap();
            let (read, mut write) = stream.into_split();
            let mut reader = BufReader::new(read);
            write
                .write_all(
                    format!(
                        "{}\n",
                        json!({
                            "type": "hello",
                            "token": token,
                            "incarnation": incarnation,
                            "location": project,
                        })
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
            let mut line = String::new();
            reader.read_line(&mut line).await.unwrap();
            assert!(line.contains("registered"), "{line}");
            // 保持连接并等待测试释放；释放时服务端 reader 会看到 EOF。
            let _ = reader.read_line(&mut line).await;
        });
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(runtime.attested());
        let connection = runtime
            .inner
            .state
            .lock()
            .unwrap()
            .connection
            .clone()
            .expect("attested connection");
        (client, connection)
    }

    #[tokio::test]
    #[cfg_attr(
        not(target_os = "macos"),
        ignore = "requires macOS OpenCode ownership or process semantics"
    )]
    async fn shutdown_closes_authenticated_connection() {
        let temp = tempfile::tempdir().unwrap();
        let project = temp.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let project = project.canonicalize().unwrap();
        let state_file = temp.path().join("service.json");
        std::fs::write(&state_file, registration(std::process::id())).unwrap();
        let paths = BridgePaths::in_dir(temp.path().join("bridge"));
        let runtime = BridgeRuntime::start_for_test(
            paths.clone(),
            DiscoveryInput {
                state_file,
                home: temp.path().to_path_buf(),
            },
            Arc::new(|_pid| Some(12_000)),
        )
        .await
        .unwrap();
        let token = serde_json::from_slice::<Value>(&std::fs::read(&paths.config).unwrap())
            .unwrap()["token"]
            .as_str()
            .unwrap()
            .to_string();
        let socket = runtime.socket_path().to_path_buf();
        let stream = UnixStream::connect(&socket).await.unwrap();
        let (read, mut write) = stream.into_split();
        let mut reader = BufReader::new(read);
        write
            .write_all(
                format!(
                    "{}\n",
                    json!({
                        "type": "hello",
                        "token": token,
                        "incarnation": "inc-shutdown",
                        "location": project,
                    })
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();
        assert!(line.contains("registered"), "{line}");
        runtime.shutdown().await;
        line.clear();
        let closed =
            tokio::time::timeout(Duration::from_millis(800), reader.read_line(&mut line)).await;
        assert!(
            closed.is_ok(),
            "client must observe bridge shutdown (EOF/close), not hang"
        );
        assert_eq!(closed.unwrap().unwrap(), 0, "expected EOF after shutdown");
    }

    #[tokio::test]
    #[cfg_attr(
        not(target_os = "macos"),
        ignore = "requires macOS OpenCode ownership or process semantics"
    )]
    async fn stale_reader_eof_does_not_clear_new_connection() {
        let temp = tempfile::tempdir().unwrap();
        let project = temp.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let project = project.canonicalize().unwrap();
        let state_file = temp.path().join("service.json");
        std::fs::write(&state_file, registration(std::process::id())).unwrap();
        let paths = BridgePaths::in_dir(temp.path().join("bridge"));
        let runtime = BridgeRuntime::start_for_test(
            paths.clone(),
            DiscoveryInput {
                state_file,
                home: temp.path().to_path_buf(),
            },
            Arc::new(|_pid| Some(12_000)),
        )
        .await
        .unwrap();
        let token = serde_json::from_slice::<Value>(&std::fs::read(&paths.config).unwrap())
            .unwrap()["token"]
            .as_str()
            .unwrap()
            .to_string();
        let (old_client, old_connection) =
            connect_attested(&runtime, &token, &project, "same-incarnation").await;
        // 模拟“旧连接已从状态摘除但 reader 仍活着”：只清状态，不关闭旧连接。
        runtime
            .clear_connection(&old_connection.id, "simulate stale state")
            .await;
        assert!(!runtime.attested());
        let (_new_client, _new_connection) =
            connect_attested(&runtime, &token, &project, "same-incarnation").await;
        assert!(runtime.attested());
        // 旧连接现在 EOF；旧 reader 不得清掉新连接。
        old_connection.writer.lock().await.shutdown().await.unwrap();
        old_client.await.unwrap();
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(
            runtime.attested(),
            "stale reader must not clear the newer connection with the same incarnation"
        );
        runtime.shutdown().await;
    }

    #[tokio::test]
    #[cfg_attr(
        not(target_os = "macos"),
        ignore = "requires macOS OpenCode ownership or process semantics"
    )]
    async fn shutdown_closes_connection_and_same_process_restart_reaccepts_plugin() {
        let temp = tempfile::tempdir().unwrap();
        let project = temp.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let project = project.canonicalize().unwrap();
        let state_file = temp.path().join("service.json");
        std::fs::write(&state_file, registration(std::process::id())).unwrap();
        let paths = BridgePaths::in_dir(temp.path().join("bridge"));
        let input = DiscoveryInput {
            state_file,
            home: temp.path().to_path_buf(),
        };
        let fact: Arc<dyn Fn(u32) -> Option<i64> + Send + Sync> = Arc::new(|_pid| Some(12_000));
        let first = BridgeRuntime::start_for_test(paths.clone(), input.clone(), Arc::clone(&fact))
            .await
            .unwrap();
        let token = serde_json::from_slice::<Value>(&std::fs::read(&paths.config).unwrap())
            .unwrap()["token"]
            .as_str()
            .unwrap()
            .to_string();
        let (client, _) = connect_attested(&first, &token, &project, "inc-restart").await;
        first.shutdown().await;
        client.await.unwrap();
        // 同进程重开：新监听必须接受同一 token/incarnation 的插件重连。
        let second = BridgeRuntime::start_for_test(paths, input, fact)
            .await
            .unwrap();
        let (client2, _) = connect_attested(&second, &token, &project, "inc-restart").await;
        second.shutdown().await;
        client2.await.unwrap();
    }

    #[tokio::test]
    async fn stopped_bridge_rejects_registration_inside_state_lock() {
        let temp = tempfile::tempdir().unwrap();
        let state_file = temp.path().join("service.json");
        std::fs::write(&state_file, registration(std::process::id())).unwrap();
        let paths = BridgePaths::in_dir(temp.path().join("bridge"));
        let runtime = BridgeRuntime::start_for_test(
            paths,
            DiscoveryInput {
                state_file,
                home: temp.path().to_path_buf(),
            },
            Arc::new(|_pid| Some(12_000)),
        )
        .await
        .unwrap();
        let (left, right) = UnixStream::pair().unwrap();
        let (_read, write) = left.into_split();
        drop(right);
        let connection = Arc::new(Connection {
            id: "conn-stopped".into(),
            runtime: RuntimeKey {
                service_id: "svc-test".into(),
                pid: std::process::id(),
                started_at_ms: 12_000,
            },
            incarnation: "inc-stopped".into(),
            location: temp.path().to_path_buf(),
            peer_pid: std::process::id(),
            writer: Arc::new(Mutex::new(write)),
            pending: Arc::new(Mutex::new(HashMap::new())),
            closed: tokio::sync::Notify::new(),
        });
        runtime.inner.stopped.store(true, Ordering::SeqCst);
        let error = register_connection(&runtime.inner, &connection).unwrap_err();
        assert!(error.contains("stopped"));
        assert!(!runtime.attested());
        assert!(runtime.inner.state.lock().unwrap().connection.is_none());
    }

    #[tokio::test]
    async fn bridge_restart_reuses_the_installed_token() {
        let temp = tempfile::tempdir().unwrap();
        let state_file = temp.path().join("service.json");
        std::fs::write(&state_file, registration(std::process::id())).unwrap();
        let paths = BridgePaths::in_dir(temp.path().join("bridge"));
        let input = DiscoveryInput {
            state_file,
            home: temp.path().to_path_buf(),
        };
        let fact: Arc<dyn Fn(u32) -> Option<i64> + Send + Sync> = Arc::new(|_pid| Some(12_000));
        let first = BridgeRuntime::start_for_test(paths.clone(), input.clone(), Arc::clone(&fact))
            .await
            .unwrap();
        let token1: Value = serde_json::from_slice(&std::fs::read(&paths.config).unwrap()).unwrap();
        let token1 = token1["token"].as_str().unwrap().to_string();
        first.shutdown().await;
        let second = BridgeRuntime::start_for_test(paths.clone(), input, Arc::clone(&fact))
            .await
            .unwrap();
        let token2: Value = serde_json::from_slice(&std::fs::read(&paths.config).unwrap()).unwrap();
        assert_eq!(token1, token2["token"].as_str().unwrap());
        second.shutdown().await;
    }

    #[tokio::test]
    #[cfg_attr(
        not(target_os = "macos"),
        ignore = "requires macOS OpenCode ownership or process semantics"
    )]
    async fn bridge_rejects_wrong_directory_and_unknown_session() {
        let temp = tempfile::tempdir().unwrap();
        let project = temp.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let project = project.canonicalize().unwrap();
        let other = temp.path().join("other");
        std::fs::create_dir_all(&other).unwrap();
        let state_file = temp.path().join("service.json");
        std::fs::write(&state_file, registration(std::process::id())).unwrap();
        let paths = BridgePaths::in_dir(temp.path().join("bridge"));
        let runtime = BridgeRuntime::start_for_test(
            paths.clone(),
            DiscoveryInput {
                state_file,
                home: temp.path().to_path_buf(),
            },
            Arc::new(|_pid| Some(12_000)),
        )
        .await
        .unwrap();
        let config: Value = serde_json::from_slice(&std::fs::read(&paths.config).unwrap()).unwrap();
        let token = config["token"].as_str().unwrap().to_string();
        let socket = runtime.socket_path().to_path_buf();
        let client = tokio::spawn(async move {
            let stream = UnixStream::connect(&socket).await.unwrap();
            let (read, mut write) = stream.into_split();
            let mut reader = BufReader::new(read);
            write
                .write_all(
                    format!(
                        "{}\n",
                        json!({
                            "type": "hello",
                            "token": token,
                            "incarnation": "inc-2",
                            "location": project,
                        })
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
            let mut line = String::new();
            reader.read_line(&mut line).await.unwrap();
            line.clear();
            reader.read_line(&mut line).await.unwrap();
            let request: Value = serde_json::from_str(line.trim()).unwrap();
            let request_id = request["requestId"].as_str().unwrap();
            let reply = json!({
                "type": "confirm_session_result",
                "requestId": request_id,
                "ok": false,
                "reason": "directory-mismatch",
            });
            write
                .write_all(format!("{reply}\n").as_bytes())
                .await
                .unwrap();
            tokio::time::sleep(Duration::from_millis(100)).await;
        });
        tokio::time::sleep(Duration::from_millis(100)).await;
        let error = runtime.confirm_session("ses-1", &other).await.unwrap_err();
        assert_eq!(error.code, codes::OC_INSTANCE_OWNERSHIP_UNVERIFIED);
        client.await.unwrap();
    }
}

#[cfg(test)]
mod live_tests {
    use super::*;
    use crate::remote::pairing::DeviceStore;
    use crate::services::opencode_native::{BridgeInstall, BridgePaths, NativeHub};
    use serde_json::Value;
    use std::process::Command;
    use std::time::Instant;
    use sysinfo::{Pid, System};

    fn process_start_ms(pid: u32) -> Option<i64> {
        let system = System::new_all();
        system
            .process(Pid::from_u32(pid))
            .map(|process| (process.start_time() as i64) * 1000)
    }

    fn opencode_bin() -> std::path::PathBuf {
        std::env::var_os("TUVIS_OPENCODE_BIN")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| {
                std::path::PathBuf::from(std::env::var("HOME").unwrap_or_default())
                    .join(".opencode/bin/opencode")
            })
    }

    /// 真实隔离服务插件桥的工程证据；默认 ignored，手动运行：
    /// `cargo test --lib opencode_native::bridge::live_tests -- --ignored --nocapture`
    #[tokio::test]
    #[ignore = "requires a local opencode binary; run explicitly for live bridge evidence"]
    async fn live_bridge_attests_registered_service_and_confirms_session() {
        let bin = opencode_bin();
        assert!(bin.exists(), "opencode binary missing: {}", bin.display());
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().to_path_buf();
        for dir in [
            "data", "config", "state", "cache", "tmp", "project", "bridge",
        ] {
            std::fs::create_dir_all(root.join(dir)).unwrap();
        }
        let project = root.join("project").canonicalize().unwrap();
        let config_path = root.join("config/opencode/opencode.json");
        let paths = BridgePaths::in_dir(root.join("bridge"));
        let installer = BridgeInstall::new(paths.clone(), config_path.clone());
        installer.install().unwrap();
        let input = DiscoveryInput {
            state_file: root.join("state/opencode/service.json"),
            home: root.clone(),
        };
        let fact: Arc<dyn Fn(u32) -> Option<i64> + Send + Sync> = Arc::new(process_start_ms);
        let hub = NativeHub::with_sources(
            DeviceStore::memory(),
            input.clone(),
            Arc::clone(&fact),
            None,
            None,
        );
        hub.store()
            .with(|conn| {
                crate::remote::pairing::persist_device(
                    conn,
                    &crate::remote::pairing::NewDevice {
                        id: "dev-live".into(),
                        name: "live-test".into(),
                        ua: "ua-live".into(),
                        origin_ip: "127.0.0.1#live".into(),
                        via: "lan".into(),
                        paired_at: chrono::Utc::now().timestamp_millis(),
                    },
                )
            })
            .map_err(|error| panic!("seed device: {error}"))
            .unwrap();
        hub.start_bridge_with(paths.clone(), input.clone(), Arc::clone(&fact))
            .await
            .unwrap();
        let port = {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            listener.local_addr().unwrap().port()
        };
        let set_port = Command::new(&bin)
            .args(["service", "set", "port", &port.to_string()])
            .env("HOME", std::env::var("HOME").unwrap_or_default())
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .env("TERM", "xterm-256color")
            .env("OPENCODE_DISABLE_AUTOUPDATE", "1")
            .env(
                "TUVIS_OPENCODE_BRIDGE_CONFIG",
                paths.config.to_string_lossy().to_string(),
            )
            .env("XDG_DATA_HOME", root.join("data"))
            .env("XDG_CONFIG_HOME", root.join("config"))
            .env("XDG_STATE_HOME", root.join("state"))
            .env("XDG_CACHE_HOME", root.join("cache"))
            .env("TMPDIR", root.join("tmp"))
            .current_dir(&project)
            .status()
            .unwrap();
        assert!(
            set_port.success(),
            "opencode service set port failed: {set_port:?}"
        );
        let out = std::fs::File::create(root.join("service.out")).unwrap();
        let err = std::fs::File::create(root.join("service.err")).unwrap();
        let mut service = Command::new(&bin)
            .args(["serve", "--service"])
            .env("HOME", std::env::var("HOME").unwrap_or_default())
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .env("TERM", "xterm-256color")
            .env("OPENCODE_DISABLE_AUTOUPDATE", "1")
            .env(
                "TUVIS_OPENCODE_BRIDGE_CONFIG",
                paths.config.to_string_lossy().to_string(),
            )
            .env("XDG_DATA_HOME", root.join("data"))
            .env("XDG_CONFIG_HOME", root.join("config"))
            .env("XDG_STATE_HOME", root.join("state"))
            .env("XDG_CACHE_HOME", root.join("cache"))
            .env("TMPDIR", root.join("tmp"))
            .current_dir(&project)
            .stdout(out)
            .stderr(err)
            .spawn()
            .unwrap();
        let started = Instant::now();
        let mut registration: Option<Value> = None;
        while started.elapsed() < Duration::from_secs(20) {
            if let Ok(bytes) = std::fs::read(&input.state_file) {
                if let Ok(value) = serde_json::from_slice::<Value>(&bytes) {
                    registration = Some(value);
                    break;
                }
            }
            if let Ok(Some(status)) = service.try_wait() {
                panic!("opencode service exited before registration: {status:?}");
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        let registration = registration.expect("service registration not written");
        let pid = registration["pid"].as_u64().unwrap() as u32;
        let password = registration["password"].as_str().unwrap().to_string();
        let url = registration["url"].as_str().unwrap().to_string();
        let service_id = registration["id"].as_str().unwrap().to_string();
        let started_at_ms = process_start_ms(pid).expect("service start time");
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let created = client
            .post(format!("{url}/api/session"))
            .basic_auth("opencode", Some(&password))
            .json(&json!({
                "title": "tuvis-bridge-live",
                "location": {"directory": project},
            }))
            .send()
            .await
            .unwrap();
        assert!(
            created.status().is_success(),
            "create session: {}",
            created.status()
        );
        let body: Value = created.json().await.unwrap();
        let session_id = body["data"]["id"].as_str().unwrap().to_string();
        // OpenCode 的服务插件在配置 revalidation 时加载；安装后显式 reload 是产品
        // GUI 的一次性步骤（隔离环境用同一命令触发）。
        let reload = Command::new(&bin)
            .args(["plugin", "list"])
            .env("HOME", std::env::var("HOME").unwrap_or_default())
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .env("TERM", "xterm-256color")
            .env("OPENCODE_DISABLE_AUTOUPDATE", "1")
            .env(
                "TUVIS_OPENCODE_BRIDGE_CONFIG",
                paths.config.to_string_lossy().to_string(),
            )
            .env("XDG_DATA_HOME", root.join("data"))
            .env("XDG_CONFIG_HOME", root.join("config"))
            .env("XDG_STATE_HOME", root.join("state"))
            .env("XDG_CACHE_HOME", root.join("cache"))
            .env("TMPDIR", root.join("tmp"))
            .current_dir(&project)
            .status()
            .unwrap();
        assert!(reload.success(), "opencode reload failed: {reload:?}");
        // OpenCode loads/setups service plugins lazily at the first location/session.
        let mut status = hub.bridge_status().await.unwrap();
        let wait = Instant::now();
        while !status.attested && wait.elapsed() < Duration::from_secs(15) {
            tokio::time::sleep(Duration::from_millis(200)).await;
            status = hub.bridge_status().await.unwrap();
        }
        if !status.attested {
            let out = std::fs::read_to_string(root.join("service.out")).unwrap_or_default();
            let err = std::fs::read_to_string(root.join("service.err")).unwrap_or_default();
            let config = std::fs::read_to_string(&config_path).unwrap_or_default();
            let log = std::fs::read_to_string(root.join("data/opencode/log/opencode.log"))
                .unwrap_or_default();
            let tail: String = log
                .lines()
                .rev()
                .take(80)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect::<Vec<_>>()
                .join("\n");
            panic!("bridge not attested: {status:?}\n--- service.out ---\n{out}\n--- service.err ---\n{err}\n--- config ---\n{config}\n--- log tail ---\n{tail}");
        }
        let target = crate::services::opencode_native::types::NativeTarget {
            runtime: RuntimeKey {
                service_id,
                pid,
                started_at_ms,
            },
            session_id: session_id.clone(),
            directory: project.clone(),
        };
        // R2 起桥只是状态/诊断源：写门由服务身份核验（证据链）统一判定。
        hub.verify_target_identity(&target)
            .await
            .expect("verified target identity");
        let _ = pid;
        let _ = project;
        let session = crate::session::Session {
            id: session_id.clone(),
            agent_type: crate::session::AgentType::OpenCode,
            project_name: "bridge-live".into(),
            project_path: project.to_string_lossy().into_owned(),
            title: Some("bridge-live".into()),
            git_branch: None,
            github_url: None,
            status: crate::session::SessionStatus::Waiting,
            last_message: None,
            last_message_role: None,
            last_message_subagent_report: false,
            flap_from_subagent_activity: false,
            last_activity_at: "2026-11-02T00:00:00Z".into(),
            pid,
            cpu_usage: 0.0,
            active_subagent_count: 0,
            form: crate::session::ProcessForm::Cli,
            jump_supported: false,
            unread: false,
            unread_turned_green_at_ms: None,
        };
        let (view, claim) = hub
            .view_and_claim(&session, true)
            .await
            .expect("view and claim");
        assert!(claim.owned, "route must be claimed");
        let action_id = format!("act_live_{}", std::process::id());
        let command = crate::services::opencode_native::NativeCommand {
            action_id: action_id.clone(),
            session_id: session_id.clone(),
            binding_token: view.binding_token.clone(),
            operation: crate::services::opencode_native::NativeOperation::Send(
                crate::services::opencode_native::SendPayload {
                    text: "Reply with exactly OK".into(),
                    attachment_paths: Vec::new(),
                    delivery: crate::services::opencode_native::DeliveryKind::Queue,
                    client_message_id: format!("msg_{action_id}"),
                },
            ),
        };
        let actor = crate::services::opencode_native::NativeActor {
            device_id: "dev-live".into(),
            device_name: "live-test".into(),
            signature_enabled: false,
        };
        let result = hub
            .execute(&actor, &session, command)
            .await
            .expect("live send");
        println!("LIVE_WRITE_ACTION {:?}", result.state);
        if result.state == crate::services::opencode_native::ActionState::Queued {
            hub.flush_outbox(&session).await.expect("flush outbox");
            let after = hub.action("dev-live", &action_id).await.unwrap();
            println!("LIVE_WRITE_AFTER_FLUSH {:?}", after);
        }
        // Confirm the real model answered and the message is in the native session.
        let mut assistant = None;
        for _ in 0..40 {
            tokio::time::sleep(Duration::from_millis(500)).await;
            let messages = client
                .get(format!("{url}/api/session/{session_id}/message"))
                .basic_auth("opencode", Some(&password))
                .send()
                .await
                .unwrap();
            if let Ok(body) = messages.json::<Value>().await {
                if let Some(items) = body["data"].as_array() {
                    assistant = items.iter().find_map(|item| {
                        if item["type"] == "assistant" {
                            item["content"].as_array().and_then(|parts| {
                                parts.iter().find_map(|part| {
                                    (part["type"] == "text")
                                        .then(|| {
                                            part["text"].as_str().unwrap_or_default().to_string()
                                        })
                                        .filter(|text| !text.trim().is_empty())
                                })
                            })
                        } else {
                            None
                        }
                    });
                    if assistant.is_some() {
                        break;
                    }
                }
            }
        }
        let assistant = assistant.expect("real model assistant reply");
        assert!(assistant.contains("OK"), "unexpected reply: {assistant}");
        println!("LIVE_WRITE_OK session={session_id} pid={pid} reply={assistant:?}");

        let agents = hub.agent_view(&session).await.expect("agent view");
        println!(
            "LIVE_AGENTS current={:?} options={:?}",
            agents.current, agents.options
        );
        assert!(agents.options.iter().any(|option| option.id == "plan"));
        let action_id = format!("act_live_agent_{}", std::process::id());
        let command = crate::services::opencode_native::NativeCommand {
            action_id: action_id.clone(),
            session_id: session_id.clone(),
            binding_token: view.binding_token.clone(),
            operation: crate::services::opencode_native::NativeOperation::SetAgent(
                crate::services::opencode_native::SetAgentPayload {
                    agent: "plan".into(),
                },
            ),
        };
        let switched = hub
            .execute(&actor, &session, command)
            .await
            .expect("switch agent");
        println!("LIVE_AGENT_ACTION {:?}", switched);
        assert_eq!(
            switched.state,
            crate::services::opencode_native::ActionState::Settled
        );
        assert_eq!(switched.reason_code.as_deref(), Some("agent_switched"));
        let after_switch = hub
            .agent_view(&session)
            .await
            .expect("agent view after switch");
        assert_eq!(after_switch.current.as_deref(), Some("plan"));
        println!("LIVE_AGENT_OK current={:?}", after_switch.current);

        // 真实附件效果：受控附件目录放一个含秘密词的文件，让模型只回该词。
        let attachment_dir = crate::remote::attachments::attachment_dir(&project, &session_id);
        std::fs::create_dir_all(&attachment_dir).unwrap();
        let secret = attachment_dir.join("tuvis-live-secret.txt");
        std::fs::write(&secret, "BLUEBIRD").unwrap();
        let action_id = format!("act_live_attach_{}", std::process::id());
        let command = crate::services::opencode_native::NativeCommand {
            action_id: action_id.clone(),
            session_id: session_id.clone(),
            binding_token: view.binding_token.clone(),
            operation: crate::services::opencode_native::NativeOperation::Send(
                crate::services::opencode_native::SendPayload {
                    text: "The attached file contains a secret word. Reply with exactly that word."
                        .into(),
                    attachment_paths: vec![secret.to_string_lossy().into_owned()],
                    delivery: crate::services::opencode_native::DeliveryKind::Queue,
                    client_message_id: format!("msg_{action_id}"),
                },
            ),
        };
        let attached = hub
            .execute(&actor, &session, command)
            .await
            .expect("send attachment");
        println!("LIVE_ATTACH_ACTION {:?}", attached.state);
        if attached.state == crate::services::opencode_native::ActionState::Queued {
            hub.flush_outbox(&session).await.expect("flush attachment");
        }
        let mut attachment_reply = None;
        for _ in 0..40 {
            tokio::time::sleep(Duration::from_millis(500)).await;
            let messages = client
                .get(format!("{url}/api/session/{session_id}/message"))
                .basic_auth("opencode", Some(&password))
                .send()
                .await
                .unwrap();
            if let Ok(body) = messages.json::<Value>().await {
                if let Some(items) = body["data"].as_array() {
                    attachment_reply = items.iter().find_map(|item| {
                        if item["type"] != "assistant" {
                            return None;
                        }
                        let text: String = item["content"]
                            .as_array()?
                            .iter()
                            .filter(|part| part["type"] == "text")
                            .filter_map(|part| part["text"].as_str())
                            .collect();
                        text.contains("BLUEBIRD").then_some(text)
                    });
                    if attachment_reply.is_some() {
                        break;
                    }
                }
            }
        }
        let attachment_reply = attachment_reply.expect("real model attachment reply");
        assert!(attachment_reply.contains("BLUEBIRD"));
        println!("LIVE_ATTACH_OK reply={attachment_reply:?}");

        // T7 未完成前 always 产品能力关闭；权限正向效果不再在本 live 测试中冒充通过。
        // 历史 isolated 取证保留在证据文档，产品回归在 actions 测试中应断言零写拒绝。

        let fresh_view = hub.view(&session).await.expect("view before stop");
        // 真实活动回合停止：先发一条长回答请求，不等回执就 interrupt；服务应答
        // interrupted=true 表示停到了活动回合，false 表示此刻已空闲，两者都是确定结论。
        let long = client
            .post(format!("{url}/api/session/{session_id}/prompt"))
            .basic_auth("opencode", Some(&password))
            .json(&json!({
                "id": format!("msg_live_long_{}", std::process::id()),
                "text": "Count slowly from 1 to 500, one number per line. Do not stop early.",
                "delivery": "queue",
            }))
            .send()
            .await
            .unwrap();
        assert!(
            long.status().is_success(),
            "long prompt status: {}",
            long.status()
        );
        tokio::time::sleep(Duration::from_millis(150)).await;

        let action_id = format!("act_live_stop_{}", std::process::id());
        let command = crate::services::opencode_native::NativeCommand {
            action_id: action_id.clone(),
            session_id: session_id.clone(),
            binding_token: fresh_view.binding_token.clone(),
            operation: crate::services::opencode_native::NativeOperation::StopSession(
                crate::services::opencode_native::StopSessionPayload {},
            ),
        };
        let stopped = hub
            .execute(&actor, &session, command)
            .await
            .expect("stop session");
        assert_eq!(
            stopped.state,
            crate::services::opencode_native::ActionState::Settled
        );
        assert!(matches!(
            stopped.reason_code.as_deref(),
            Some("interrupted") | Some("already_idle")
        ));
        println!("LIVE_STOP_OK reason={:?}", stopped.reason_code);
        let _ = Command::new("kill")
            .arg("-TERM")
            .arg(pid.to_string())
            .status();
        let _ = service.wait();
        hub.stop_bridge().await;
    }
}
