//! 本机授权 IPC 传输层（T06；§2.5「Unix domain socket / Windows named pipe，
//! Rust 持有服务端，限定当前用户并拒绝远程 pipe」）。
//!
//! 三件套（对 §2.1 文件布局的**文档化扩展**：本文件承载 auth 域的本机传输面，
//! 与后续 T08 的 `transport.rs`「连接宿主 loopback authority」是两回事）：
//!
//! * [`IpcListener`] / [`IpcClientConnection`]：可注入抽象（dyn 兼容，BoxFuture
//!   签名；**不引 async-trait 依赖**）。测试用 [`mem`] 内存对（零真实套接字、
//!   确定性、可观测关闭语义），生产用 [`uds::UdsIpcListener`]；
//! * 生产面（cfg(unix)）：UDS 落在 DSH 状态目录（`<app_data_home>/.tuvis/dsh/`，
//!   覆盖变量 `TUVIS_DSH_STATE_DIR`），目录 0700、套接字 0600
//!   （**当前用户限定靠文件系统权限**）；对端 OS 身份 = `peer_cred()` 的 PID/UID
//!   投影（macOS/Linux getpeereid 等价面），UID 缺席时由 auth 层以「PID 必须映射
//!   到运行中宿主进程」补核验；
//! * Windows 面（cfg(windows)）：命名管道已接生产工厂（DACL 限当前用户 + 拒远程
//!   + 内核 PID）。
//!
//! 当前环境无法实测 DACL/跨账户/远程/取消/EOF/背压——**不得据此声称 Windows
//! 已验收**，实机证据补齐前按未验证申报。

use std::pin::Pin;
use std::sync::{Arc, Mutex};

use super::error::{codes, DshError};

/// 帧上限（NDJSON 单行；握手帧极小，1 MiB 已是宽裕护栏）。
pub const MAX_FRAME_BYTES: usize = 1024 * 1024;

// ============================================================
// Unix 套接字路径长度策略（Mac 补探测 §5 P3 / §6 第 7 条）
// ============================================================

/// AF_UNIX `sun_path` 上限（macOS 实测 103 字节可 bind，104+ 报
/// `path must be shorter than SUN_LEN`；Linux 108 ——取更严的那个作统一上界）。
pub const UNIX_SOCKET_PATH_MAX: usize = 103;

/// 本端套接字文件名字节的固定长度（`ipc-<16hex>.sock` = 4 + 16 + 5 = 25）。
pub const SOCKET_NAME_BYTES: usize = "ipc-0123456789abcdef.sock".len();

/// 状态目录可用预算：`<state_dir>/<socket 名>` ≤ 103 ⇒ state_dir ≤ 77。
///
/// 注：探测报告的行文写作「state_dir ≤ 78」，那是 `103 - 25` **未计分隔符**；
/// `bind` 校验的是整条路径（含 `/`），故实现取更严的 77 —— 78 字节的目录加
/// 分隔符再加套接字名会是 104 字节，实测即失败。
pub const STATE_DIR_BYTES_MAX: usize = UNIX_SOCKET_PATH_MAX - 1 - SOCKET_NAME_BYTES;

/// 用户可换的短目录变量名（group 2b 引入的生产旋钮；此处的文案先以名字引用）。
pub const STATE_DIR_ENV: &str = "TUVIS_DSH_STATE_DIR";

/// 校验状态目录长度（按**字节**，非字符数：非 ASCII 路径按 UTF-8 实际占用）。
///
/// 为什么要在 bind 之前拦（而不是让 `bind` 报错）：裸 OS 错误
/// （`path must be shorter than SUN_LEN`）只给出失败，不指出用户可操作的旋钮，
/// 深路径场景（影子 `TUVIS_HOME`/长用户名）在 UI 上会被当成「IPC 坏了」——
/// 用户的修复出口就是把 `TUVIS_DSH_STATE_DIR` 指到更短的目录。
pub fn check_state_dir_length(state_dir: &std::path::Path) -> Result<(), DshError> {
    check_socket_path_length(state_dir, SOCKET_NAME_BYTES)
}

/// 按实际套接字名长度的预算校验（H05：实例作用域套接字名是
/// `ipc-<32hex>.sock` = 41 字节，比随机名长 16 字节——预算按**实际**名长收紧，
/// 默认根在常规安装下仍可容纳；超限文案同样指向 `TUVIS_DSH_STATE_DIR`）。
pub fn check_socket_path_length(
    state_dir: &std::path::Path,
    socket_name_bytes: usize,
) -> Result<(), DshError> {
    #[cfg(windows)]
    {
        let _ = (state_dir, socket_name_bytes);
        Ok(())
    }
    #[cfg(not(windows))]
    {
        let bytes = state_dir.as_os_str().as_encoded_bytes().len();
        let budget = UNIX_SOCKET_PATH_MAX.saturating_sub(1 + socket_name_bytes);
        if bytes > budget {
            return Err(DshError::new(
                codes::STATE_PATH_TOO_LONG,
                format!(
                    "dsh state dir is {} bytes but must be <= {budget} bytes (unix socket path limit {} includes the {socket_name_bytes} byte socket name); set {} to a shorter directory",
                    bytes, UNIX_SOCKET_PATH_MAX, STATE_DIR_ENV
                ),
            ));
        }
        Ok(())
    }
}

/// 未来别名（dyn 兼容签名的手写 BoxFuture；等价 async-trait 的展开形态）。
pub type IpcFut<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

use std::future::Future;

/// OS 层对端身份投影（UDS peercred / 命名管道 sid 的最小面）。
///
/// `uid` 是「当前用户限定」的直接证据（有则必核）；缺席（个别平台 peercred
/// 不出 uid）时由 auth 层退化为「PID 必须在进程表中且是宿主」的强核验。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerOsIdentity {
    /// 对端真实 OS PID（不可自报，只能来自内核）。
    pub pid: u32,
    /// 对端 UID（可得则必核 = 当前用户）。
    pub uid: Option<u32>,
}

/// 单个客户端连接（服务端视角；一行一帧 NDJSON）。
pub trait IpcClientConnection: Send + Sync {
    /// OS 层对端身份（连接建立即固定）。
    fn peer_os(&self) -> PeerOsIdentity;
    /// 读一帧（原始字节，含尾部 '\n' 由实现剥离；对端关闭/IO 错误 → Err）。
    fn recv(&self) -> IpcFut<'_, Result<String, DshError>>;
    /// 写一帧（调用方给不带 '\n' 的单行；实现负责补行界）。
    fn send(&self, frame: &str) -> IpcFut<'_, Result<(), DshError>>;
    /// 立即关闭（撤权关受信 Peer 的落点；后续 send/recv → Err）。
    /// `Arc<Self>` 接收者：实现需要把异步资源（流半区）挪进销毁任务。
    fn close(self: Arc<Self>);
    /// 关闭态（撤权测试的观测面：关闭后 write 不可用）。
    fn is_closed(&self) -> bool;
}

/// 监听端（服务端）：绑定随机私有地址 → 逐个 accept。
pub trait IpcListener: Send + Sync {
    /// 绑定随机私有地址并返回端点串（UDS 路径 / 管道名；rendezvous 的 address 字段）。
    fn bind(&self) -> IpcFut<'_, Result<String, DshError>>;
    /// 接受一个连接（含 OS 对端身份）。
    fn accept(&self) -> IpcFut<'_, Result<Box<dyn IpcClientConnection>, DshError>>;
    /// 停止监听（不再 accept；已建连接不受影响——撤权单独关）。
    fn stop(&self);
    /// 端点串（bind 后可用；未 bind → None）。
    fn endpoint(&self) -> Option<String>;
}

// ============================================================
// 内存传输（测试注入面；pub 以便集成测试 dsh_auth_test 使用）
// ============================================================

pub mod mem {
    use super::*;
    use tokio::sync::mpsc;

    /// 内存连接的通道容量（帧极小；1 足够，满时自然背压）。
    const CHAN: usize = 16;

    struct MemInner {
        to_server: mpsc::Sender<String>,
        /// 接收端要跨 await 持锁 → tokio Mutex（std guard 不 Send）。
        from_server: tokio::sync::Mutex<mpsc::Receiver<String>>,
        server_to: mpsc::Sender<String>,
        server_from: tokio::sync::Mutex<mpsc::Receiver<String>>,
        closed: Mutex<bool>,
        peer_os: PeerOsIdentity,
    }

    /// 客户端侧句柄（测试持有；观测服务端写来的帧与关闭态）。
    pub struct MemClient {
        inner: Arc<MemInner>,
    }

    /// 服务端侧连接（auth 服务循环经 accept 拿到）。
    pub struct MemServerConn {
        inner: Arc<MemInner>,
    }

    /// 一对工厂：`(listener, client_factory)`。每次 `factory.connect()` 产出一对
    /// 已互连的（客户端句柄，服务端连接），服务端观测到的 peer_os = 给定身份。
    pub struct MemClientFactory {
        accept_tx: mpsc::Sender<MemServerConn>,
        peer_os: PeerOsIdentity,
    }

    /// 建一对内存监听端 + 客户端工厂（测试的注入点；地址只是占位串）。
    pub fn mem_listener(peer_os: PeerOsIdentity) -> (MemIpcListener, Arc<MemClientFactory>) {
        let (accept_tx, accept_rx) = mpsc::channel(CHAN);
        (
            MemIpcListener {
                accept_rx: tokio::sync::Mutex::new(accept_rx),
                endpoint: Mutex::new(None),
            },
            Arc::new(MemClientFactory { accept_tx, peer_os }),
        )
    }

    impl MemClientFactory {
        /// 客户端主动连接：产出（客户端句柄，服务端连接已入 accept 队列）。
        pub async fn connect(&self) -> MemClient {
            self.connect_as(self.peer_os.clone()).await
        }

        /// 以另一 OS 对端身份连接（同一条监听端 accept 队列）：宿主世代重启回归用
        /// ——同一监听端先后出现不同宿主代际（不同 PID）的客户端连接。
        pub async fn connect_as(&self, peer_os: PeerOsIdentity) -> MemClient {
            let (c2s_tx, c2s_rx) = mpsc::channel(CHAN);
            let (s2c_tx, s2c_rx) = mpsc::channel(CHAN);
            let inner = Arc::new(MemInner {
                to_server: c2s_tx,
                from_server: tokio::sync::Mutex::new(s2c_rx),
                server_to: s2c_tx,
                server_from: tokio::sync::Mutex::new(c2s_rx),
                closed: Mutex::new(false),
                peer_os,
            });
            let _ = self
                .accept_tx
                .send(MemServerConn {
                    inner: inner.clone(),
                })
                .await;
            MemClient { inner }
        }
    }

    impl MemClient {
        /// 读一帧服务端来帧（服务端关闭 → Err）。
        pub async fn recv(&self) -> Result<String, DshError> {
            let mut guard = self.inner.from_server.lock().await;
            guard.recv().await.ok_or_else(|| {
                DshError::new(codes::IPC_STATE, "mem ipc: server closed the connection")
            })
        }

        /// 写一帧到服务端（关闭后 → Err；撤权测试的 write 观测面）。
        pub async fn send(&self, frame: &str) -> Result<(), DshError> {
            if *self.inner.closed.lock().unwrap() {
                return Err(DshError::new(
                    codes::IPC_STATE,
                    "mem ipc: connection closed",
                ));
            }
            self.inner
                .to_server
                .send(frame.to_string())
                .await
                .map_err(|_| {
                    DshError::new(codes::IPC_STATE, "mem ipc: server dropped the connection")
                })
        }

        /// 非阻塞收帧（排空服务端主动推送的 granted/revoked 通知）。
        pub fn try_recv(&self) -> Option<String> {
            let mut guard = self.inner.from_server.try_lock().ok()?;
            guard.try_recv().ok()
        }

        /// 撤权后服务端关闭 → 客户端写不可用（brief 的 write_available 否定面）。
        pub fn write_available(&self) -> bool {
            !*self.inner.closed.lock().unwrap()
        }
    }

    impl IpcClientConnection for MemServerConn {
        fn peer_os(&self) -> PeerOsIdentity {
            self.inner.peer_os.clone()
        }

        fn recv(&self) -> IpcFut<'_, Result<String, DshError>> {
            let inner = self.inner.clone();
            Box::pin(async move {
                let mut guard = inner.server_from.lock().await;
                guard.recv().await.ok_or_else(|| {
                    DshError::new(codes::IPC_STATE, "mem ipc: client closed the connection")
                })
            })
        }

        fn send(&self, frame: &str) -> IpcFut<'_, Result<(), DshError>> {
            let inner = self.inner.clone();
            let frame = frame.to_string();
            Box::pin(async move {
                if *inner.closed.lock().unwrap() {
                    return Err(DshError::new(
                        codes::IPC_STATE,
                        "mem ipc: connection closed",
                    ));
                }
                inner.server_to.send(frame).await.map_err(|_| {
                    DshError::new(codes::IPC_STATE, "mem ipc: client dropped the connection")
                })
            })
        }

        fn close(self: Arc<Self>) {
            *self.inner.closed.lock().unwrap() = true;
        }

        fn is_closed(&self) -> bool {
            *self.inner.closed.lock().unwrap()
        }
    }

    /// 内存监听端（accept 队列 + 占位端点）。
    pub struct MemIpcListener {
        accept_rx: tokio::sync::Mutex<mpsc::Receiver<MemServerConn>>,
        endpoint: Mutex<Option<String>>,
    }

    impl IpcListener for MemIpcListener {
        fn bind(&self) -> IpcFut<'_, Result<String, DshError>> {
            Box::pin(async {
                let addr = "mem://tuvis-dsh-auth".to_string();
                *self.endpoint.lock().unwrap() = Some(addr.clone());
                Ok(addr)
            })
        }

        fn accept(&self) -> IpcFut<'_, Result<Box<dyn IpcClientConnection>, DshError>> {
            Box::pin(async {
                let mut guard = self.accept_rx.lock().await;
                guard
                    .recv()
                    .await
                    .map(|c| Box::new(c) as Box<dyn IpcClientConnection>)
                    .ok_or_else(|| DshError::new(codes::IPC_STATE, "mem ipc: listener closed"))
            })
        }

        fn stop(&self) {}

        fn endpoint(&self) -> Option<String> {
            self.endpoint.lock().unwrap().clone()
        }
    }
}

// ============================================================
// 生产面：Unix domain socket（macOS / Linux）
// ============================================================

#[cfg(unix)]
pub mod uds {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
    use tokio::net::UnixListener;

    /// H05 审查 C2-r2：bind 排他窗口的进程内串行门（窗口零 await，std 锁可跨整个
    /// poll 持有；并发 bind 无论运行时形态都由它串行，绝不交错探测/删除）。
    static BIND_GATE: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// 跨进程排他锁文件（`<端点>.lock`；flock；bind 返回即释放——此后端点由探活
    /// 保护：其他进程探到活端点即拒绝接管）。锁文件是常规文件，不占 sun_path 预算。
    fn takeover_lock(endpoint_path: &std::path::Path) -> Result<std::fs::File, DshError> {
        let lock_path = endpoint_path.with_extension("lock");
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)
            .map_err(|e| {
                DshError::new(
                    codes::IPC_STATE,
                    format!("open takeover lock {}: {e}", lock_path.display()),
                )
            })?;
        use fs2::FileExt;
        file.lock_exclusive().map_err(|e| {
            DshError::new(
                codes::IPC_STATE,
                format!("flock takeover lock {}: {e}", lock_path.display()),
            )
        })?;
        Ok(file)
    }

    /// 探活失败分类（C2-r2）：只有**可确认残留**（端点已消失 / 无人监听）才允许
    /// unlink 接管；其它本地错误（fd 耗尽 / 权限 / 无法判定）一律保守拒绝——
    /// 绝不因一次探活失败删掉一个可能仍活着的端点。
    pub(crate) fn stale_takeover_allowed(err: &std::io::Error) -> bool {
        matches!(
            err.kind(),
            std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
        )
    }

    /// UDS 监听端：路径随机（或按实例指定的确定名）、0600；对端身份 = peer_cred（PID/UID）。
    pub struct UdsIpcListener {
        /// 权限根（解析后的 DSH 状态目录；0700）。
        state_dir: PathBuf,
        /// 固定套接字名（实例作用域：`ipc-<instance_id>.sock`）；None = 随机名。
        socket_name: Option<String>,
        bound: Mutex<Option<Arc<UnixListener>>>,
        endpoint: Mutex<Option<String>>,
    }

    impl UdsIpcListener {
        pub fn new(state_dir: PathBuf) -> Self {
            Self {
                state_dir,
                socket_name: None,
                bound: Mutex::new(None),
                endpoint: Mutex::new(None),
            }
        }

        /// 实例作用域监听端（H05）：套接字名 = `ipc-<instance_id>.sock`（确定性；
        /// 各实例端点互不可见，重启后地址稳定）。预算按实际名长校验。
        pub fn new_with_name(state_dir: PathBuf, socket_name: String) -> Self {
            Self {
                state_dir,
                socket_name: Some(socket_name),
                bound: Mutex::new(None),
                endpoint: Mutex::new(None),
            }
        }
    }

    impl UdsIpcListener {
        /// 排他窗口内的实际 bind 序列（全部同步；成功把监听端与端点写入自身槽位）。
        fn bind_under_gate(&self, path: &std::path::Path) -> Result<(), DshError> {
            // 先探活再清理：能连上 = 有存活监听端在服务（绝不删除活端点）；
            // 可确认残留（ENOENT/ECONNREFUSED）→ 清掉后 bind；其余错误保守拒绝
            // 接管（绝不 unlink）。
            if path.exists() {
                match std::os::unix::net::UnixStream::connect(path) {
                    Ok(_) => {
                        return Err(DshError::new(
                            codes::IPC_STATE,
                            format!(
                                "dsh ipc endpoint is already served by a live listener: {}",
                                path.display()
                            ),
                        ));
                    }
                    Err(e) if stale_takeover_allowed(&e) => {
                        let _ = std::fs::remove_file(path);
                    }
                    Err(e) => {
                        return Err(DshError::new(
                            codes::IPC_STATE,
                            format!(
                                "dsh ipc endpoint probe failed ({}); refusing to take over {}",
                                e,
                                path.display()
                            ),
                        ));
                    }
                }
            }
            let listener = UnixListener::bind(path).map_err(|e| {
                DshError::new(codes::IPC_STATE, format!("bind {}: {e}", path.display()))
            })?;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).map_err(
                |e| {
                    DshError::new(
                        codes::IPC_STATE,
                        format!("chmod 0600 {}: {e}", path.display()),
                    )
                },
            )?;
            let endpoint = path.to_string_lossy().to_string();
            *self.bound.lock().unwrap() = Some(Arc::new(listener));
            *self.endpoint.lock().unwrap() = Some(endpoint);
            Ok(())
        }
    }

    impl IpcListener for UdsIpcListener {
        fn bind(&self) -> IpcFut<'_, Result<String, DshError>> {
            Box::pin(async move {
                // 路径长度先于任何文件系统动作（AF_UNIX 上限是硬约束，绑定必然
                // 失败——给出可诊断的定位错误，而不是裸的 SUN_LEN 报错）。
                // 预算按**实际**套接字名长度收紧（实例名 41 字节 > 随机名 25 字节）。
                let name = match &self.socket_name {
                    Some(name) => name.clone(),
                    None => format!(
                        "ipc-{:016x}.sock",
                        crate::services::dsh_control::auth::random_u64()
                    ),
                };
                check_socket_path_length(&self.state_dir, name.len())?;
                // 目录 0700：非当前用户连目录都进不来（当前用户限定的第一道闸）。
                std::fs::create_dir_all(&self.state_dir).map_err(|e| {
                    DshError::new(
                        codes::IPC_STATE,
                        format!("create_dir_all {}: {e}", self.state_dir.display()),
                    )
                })?;
                std::fs::set_permissions(&self.state_dir, std::fs::Permissions::from_mode(0o700))
                    .map_err(|e| {
                    DshError::new(
                        codes::IPC_STATE,
                        format!("chmod 0700 {}: {e}", self.state_dir.display()),
                    )
                })?;
                let path = self.state_dir.join(&name);
                // H05 审查 C2-r2：「探测→清理→bind」必须是一个**排他窗口**（进程内
                // BIND_GATE 串行 + 跨进程 flock 锁文件），且只在**可确认残留**时接管：
                // 探活失败 ≠ 崩溃残留（fd 耗尽/权限等本地错误下端点可能仍活），只有
                // ENOENT（端点已消失）/ ECONNREFUSED（无人监听）两类才允许 unlink。
                // 整个窗口零 await（std 连接探活 + tokio UnixListener::bind 是同步
                // 构造），gate/flock 守卫不会跨 await。
                let bind_gate = BIND_GATE.lock().unwrap();
                let lock_file = takeover_lock(&path)?;
                let bind_result = self.bind_under_gate(&path);
                // bind 返回即释放 flock：端点此后由探活保护（活端点拒绝接管）。
                drop(lock_file);
                drop(bind_gate);
                bind_result?;
                self.endpoint.lock().unwrap().clone().ok_or_else(|| {
                    DshError::new(codes::IPC_STATE, "uds bind finished without an endpoint")
                })
            })
        }

        fn accept(&self) -> IpcFut<'_, Result<Box<dyn IpcClientConnection>, DshError>> {
            Box::pin(async move {
                let listener =
                    self.bound.lock().unwrap().clone().ok_or_else(|| {
                        DshError::new(codes::IPC_STATE, "uds listener is not bound")
                    })?;
                let (stream, _addr) = listener
                    .accept()
                    .await
                    .map_err(|e| DshError::new(codes::IPC_STATE, format!("uds accept: {e}")))?;
                let peer = stream.peer_cred().ok().map(|c| PeerOsIdentity {
                    pid: c.pid().map(|p| p as u32).unwrap_or(0),
                    // unix peercred 恒有 uid；当前用户限定另由 0700 目录/0600 套接字兜底。
                    uid: Some(c.uid()),
                });
                let peer = peer.ok_or_else(|| {
                    // 拿不到对端身份 = 无法核验 = 拒绝（不猜）。
                    DshError::new(codes::PEER_MISMATCH, "uds peer credentials unavailable")
                })?;
                // 整条 socket 的**同步**独立句柄（先于 into_split 取）：close 时对它
                // 调 `std::net::Shutdown::Both`（= shutdown(SHUT_RDWR)），才能同时
                // 「解除本端在飞读」并「让对端收到 EOF」（Mac 补探测 §5 S4/S5）。
                // 用 std 句柄而非 libc 原始 fd：`shutdown` 是 `&self` 同步方法，
                // 拿锁/await 都不需要（正是「清理路径不得与在飞读共用互斥锁」的要求）；
                // `into_split()` 之后两半都拿不到整条 socket，故这是必要形状。
                let std_stream = stream
                    .into_std()
                    .map_err(|e| DshError::new(codes::IPC_STATE, format!("uds into_std: {e}")))?;
                let shutdown_stream = Arc::new(std_stream.try_clone().map_err(|e| {
                    DshError::new(codes::IPC_STATE, format!("uds clone handle: {e}"))
                })?);
                let stream = tokio::net::UnixStream::from_std(std_stream)
                    .map_err(|e| DshError::new(codes::IPC_STATE, format!("uds from_std: {e}")))?;
                let (reader, writer) = stream.into_split();
                Ok(Box::new(UdsConn {
                    reader: tokio::sync::Mutex::new(Some(BufReader::new(reader))),
                    writer: tokio::sync::Mutex::new(writer),
                    peer,
                    closed: Mutex::new(false),
                    shutdown_stream,
                }) as Box<dyn IpcClientConnection>)
            })
        }

        fn stop(&self) {
            // drop 掉 UnixListener 即停 accept；套接字文件随进程退出由下次 bind 清理。
            *self.bound.lock().unwrap() = None;
        }

        fn endpoint(&self) -> Option<String> {
            self.endpoint.lock().unwrap().clone()
        }
    }

    pub struct UdsConn {
        /// 读半 + 行缓冲（recv 逐次取一行；guard 跨 await → tokio Mutex）。
        reader: tokio::sync::Mutex<Option<BufReader<tokio::net::unix::OwnedReadHalf>>>,
        /// 写半（send 串行化；guard 跨 await → tokio Mutex）。
        writer: tokio::sync::Mutex<tokio::net::unix::OwnedWriteHalf>,
        peer: PeerOsIdentity,
        closed: Mutex<bool>,
        /// 整条 socket 的独立 std 句柄（`close()` 的唯一武器）。
        ///
        /// **为什么单独持有**：`into_split()` 后的两半各自只代表半条 socket，
        /// 对任一半 shutdown 都只解一侧（S3）；`std::net::UnixStream::shutdown`
        /// 是 `&self` 同步调用、不碰 reader/writer 的 tokio 锁——这正是 R07 的核心：
        /// 清理路径必须能在「recv 正持 reader 锁卡在 read_line」时独立推进。
        shutdown_stream: Arc<std::os::unix::net::UnixStream>,
    }

    impl IpcClientConnection for UdsConn {
        fn peer_os(&self) -> PeerOsIdentity {
            self.peer.clone()
        }

        fn recv(&self) -> IpcFut<'_, Result<String, DshError>> {
            let slot = &self.reader;
            let closed = &self.closed;
            Box::pin(async move {
                if *closed.lock().unwrap() {
                    return Err(DshError::new(codes::IPC_STATE, "uds connection closed"));
                }
                let mut guard = slot.lock().await;
                let Some(reader) = guard.as_mut() else {
                    return Err(DshError::new(codes::IPC_STATE, "uds connection closed"));
                };
                // M-6 修复：`read_line` 是无界的——对端（本机当前用户的任意进程都能
                // 连上这条 UDS）写一行 4 GiB 就能把兔维斯撑到 OOM，声称的 1 MiB 硬
                // 上限并不存在。这里用 `take(MAX_FRAME_BYTES + 1)` 把**单次读取**本身
                // 限死：多读那 1 字节只用于区分「恰好到界」与「越界」，不必先收进内存
                // 再判断（反模式：先聚合后校验）。
                //
                // M-9（复审 off-by-one）：上限的语义是**内容长度 ≤ MAX_FRAME_BYTES**
                // （行尾换行不算内容）。旧判定拿「读出的字节数（含换行）」与 MAX 比，
                // 恰好 MAX 字节内容的帧（+1 换行）被误拒——比声称的 1 MiB 少 1 字节。
                // 现在按**去掉行尾 CR/LF 后的长度**判定：≤ MAX 交付，> MAX 拒绝。
                let mut line = String::new();
                let capped = &mut reader.take((MAX_FRAME_BYTES + 1) as u64);
                let n = capped
                    .read_line(&mut line)
                    .await
                    .map_err(|e| DshError::new(codes::IPC_STATE, format!("uds read_line: {e}")))?;
                // read_line 返回后先看关闭旗标：shutdown 会让它读到 0 字节（或抹掉
                // 半行），两种形态都按「已关闭」如实上报，不把残留字节当有效帧。
                if *closed.lock().unwrap() {
                    return Err(DshError::new(codes::IPC_STATE, "uds connection closed"));
                }
                if n == 0 {
                    return Err(DshError::new(
                        codes::IPC_STATE,
                        "uds peer closed the connection",
                    ));
                }
                let trimmed = line.trim_end_matches(['\n', '\r']);
                // 越界（含「1 MiB + 1 但无行界」这种半行形态）→ 明确拒绝并把连接
                // 标成已关（与其它 IPC_STATE 失败同款语义：坏帧不再续读）。
                if trimmed.len() > MAX_FRAME_BYTES {
                    *closed.lock().unwrap() = true;
                    return Err(DshError::new(
                        codes::IPC_STATE,
                        format!(
                            "uds frame exceeds the {MAX_FRAME_BYTES} byte limit (read {n} bytes)"
                        ),
                    ));
                }
                Ok(trimmed.to_string())
            })
        }

        fn send(&self, frame: &str) -> IpcFut<'_, Result<(), DshError>> {
            let payload = format!("{frame}\n");
            Box::pin(async move {
                if *self.closed.lock().unwrap() {
                    return Err(DshError::new(codes::IPC_STATE, "uds connection closed"));
                }
                let mut guard = self.writer.lock().await;
                guard
                    .write_all(payload.as_bytes())
                    .await
                    .map_err(|e| DshError::new(codes::IPC_STATE, format!("uds write: {e}")))?;
                guard
                    .flush()
                    .await
                    .map_err(|e| DshError::new(codes::IPC_STATE, format!("uds flush: {e}")))
            })
        }

        fn close(self: Arc<Self>) {
            // R07 修复：置关闭旗标 + 对**整条 socket** 同步 shutdown(SHUT_RDWR)。
            //
            // 旧实现 spawn 一个任务先 `self.reader.lock().await` 再 shutdown 写半——
            // 在飞 recv 正持那把 reader 锁卡在 read_line，teardown 永远拿不到锁，
            // 于是既解不了本端读、也不给对端 EOF（探针 S2 实测）。
            //
            // 现在：`std::net::UnixStream::shutdown` 是同步 `&self` 方法，不经过任何
            // tokio 锁、不 await、不起任务——立即对整条 socket 生效：
            //  * 本端在飞的 `read_line` 立刻被唤醒并以 0 字节/错误返回（置旗标后
            //    由 recv 的 closed 分支如实报错）；
            //  * 对端读到 EOF（S4/S5 的面向对端效应），即使对端自己保持写半区。
            //
            // 幂等：重复 shutdown 只是再做一次同状态调用（std 层返回 Ok）。
            *self.closed.lock().unwrap() = true;
            let _ = self.shutdown_stream.shutdown(std::net::Shutdown::Both);
        }

        fn is_closed(&self) -> bool {
            *self.closed.lock().unwrap()
        }
    }
}

// ============================================================
// Windows 面：命名管道（DACL 限当前用户 + 拒远程 + 内核 PID）
// ============================================================

#[cfg(windows)]
#[path = "local_ipc/winpipe.rs"]
mod winpipe_impl;

#[cfg(windows)]
pub use winpipe_impl::WinPipeListener;

/// 生产监听端工厂（按平台选择；测试不经此处）。
pub fn production_listener(state_dir: std::path::PathBuf) -> Box<dyn IpcListener> {
    production_listener_named(state_dir, None)
}

/// 生产监听端工厂·指定套接字名（H05 实例作用域：`ipc-<instance_id>.sock`——
/// 各实例端点互不可见且跨重启稳定；None = 旧随机名行为逐字保留）。
pub fn production_listener_named(
    state_dir: std::path::PathBuf,
    socket_name: Option<&str>,
) -> Box<dyn IpcListener> {
    #[cfg(unix)]
    {
        match socket_name {
            Some(name) => Box::new(uds::UdsIpcListener::new_with_name(
                state_dir,
                name.to_owned(),
            )),
            None => Box::new(uds::UdsIpcListener::new(state_dir)),
        }
    }
    #[cfg(windows)]
    {
        // 实例作用域套接字名直接作为管道后缀；端点按 state_dir + 名做哈希，
        // 各实例互不可见且跨重启稳定。
        Box::new(winpipe_impl::WinPipeListener::new(
            state_dir,
            socket_name.map(str::to_owned),
        ))
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = state_dir;
        let _ = socket_name;
        panic!("unsupported platform for local ipc")
    }
}

#[cfg(all(test, unix))]
mod path_policy_tests {
    use super::*;
    use std::path::PathBuf;

    /// 预算常量自洽：103 = 1（分隔符）+ 25（套接字名）+ 77... 逐步核算。
    #[test]
    fn budget_constants_are_consistent() {
        assert_eq!(UNIX_SOCKET_PATH_MAX, 103);
        assert_eq!(SOCKET_NAME_BYTES, 25);
        assert_eq!(STATE_DIR_BYTES_MAX, 77, "103 - 1（分隔符）- 25（套接字名）");
        // 上限边界：77 字节恰好通过，78 字节必须被拒（78+1+25 = 104 > 103）。
        let ok = PathBuf::from("a".repeat(STATE_DIR_BYTES_MAX));
        check_state_dir_length(&ok).expect("恰在预算内必须通过");
        let too_long = PathBuf::from("a".repeat(STATE_DIR_BYTES_MAX + 1));
        assert_eq!(
            check_state_dir_length(&too_long).unwrap_err().code,
            codes::STATE_PATH_TOO_LONG
        );
    }

    /// 非 ASCII 路径按**字节**计量（中文路径一个字符 3 字节，字符数达标不等于
    /// 字节数达标——这正是把 `sun_path` 报错伪装成「IPC 坏了」的典型坑）。
    #[test]
    fn non_ascii_state_dir_is_measured_in_bytes() {
        // 25 个汉字 = 75 字节（通过），26 个 = 78 字节（必须被拒）。
        let ok = PathBuf::from("深".repeat(25));
        assert_eq!(ok.as_os_str().as_encoded_bytes().len(), 75);
        check_state_dir_length(&ok).expect("75 字节必须通过");
        let too_long = PathBuf::from("深".repeat(26));
        assert_eq!(
            check_state_dir_length(&too_long).unwrap_err().code,
            codes::STATE_PATH_TOO_LONG
        );
    }

    /// 错误文案必须指出用户可操作的两个事实：上限数值与可换的变量名。
    #[test]
    fn diagnostic_mentions_limit_and_env_var() {
        let err = check_state_dir_length(&PathBuf::from("x".repeat(200))).unwrap_err();
        assert!(err.detail.contains("103"), "{}", err.detail);
        assert!(err.detail.contains(STATE_DIR_ENV), "{}", err.detail);
        assert!(err.detail.contains("77"), "{}", err.detail);
        assert!(err.detail.contains("200"), "{}", err.detail);
    }

    /// H05 审查 C2：先探活再清理——活端点绝不删除（第二个同端点 bind 被拒且
    /// 原端点原样存活）；宿主放弃后（连接不通）残留才可被接管。
    #[tokio::test]
    async fn live_endpoint_is_never_deleted_by_a_second_bind() {
        let dir = std::env::temp_dir().join(format!("dsh-uds-probe-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let a = uds::UdsIpcListener::new_with_name(dir.clone(), "ipc-probe-test.sock".into());
        let addr = a.bind().await.expect("首个 bind 成功");
        // 第二个同端点 bind：拒绝（活端点在服务）
        let b = uds::UdsIpcListener::new_with_name(dir.clone(), "ipc-probe-test.sock".into());
        let err = b.bind().await.expect_err("活端点必须被拒绝");
        assert!(
            err.detail.contains("already served"),
            "拒绝文案必须指明端点已被服务：{err}"
        );
        // 原端点原样存活（endpoint 未被动过）
        assert_eq!(a.endpoint().as_deref(), Some(addr.as_str()));
        // 宿主放弃（drop → 无人服务）→ 残留可被接管
        drop(a);
        let c = uds::UdsIpcListener::new_with_name(dir.clone(), "ipc-probe-test.sock".into());
        c.bind().await.expect("崩溃残留（无人服务）可接管");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// H05 审查 C2-r2：并发双 bind——两个任务同时对同一端点 bind，排他窗口下
    /// 恰一个成功；败者的探活发生在胜者 bind **之后**（窗口串行）→ 活端点文件
    /// 绝不被删除（第三次 bind 仍被拒、端点文件仍在）。
    #[tokio::test]
    async fn concurrent_binds_never_delete_a_live_endpoint() {
        let dir = std::env::temp_dir().join(format!("dsh-uds-conc-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let name = "ipc-conc-test.sock".to_string();
        let a = uds::UdsIpcListener::new_with_name(dir.clone(), name.clone());
        let b = uds::UdsIpcListener::new_with_name(dir.clone(), name.clone());
        let (ra, rb) = tokio::join!(a.bind(), b.bind());
        let endpoint_path = dir.join(&name);
        match (&ra, &rb) {
            (Ok(_), Err(e)) | (Err(e), Ok(_)) => {
                assert!(
                    e.detail.contains("already served"),
                    "败者必须以「端点已被服务」拒绝：{e}"
                );
            }
            _ => panic!("并发双 bind 必须恰一个成功：{ra:?} / {rb:?}"),
        }
        // 胜者端点原样存活：文件未被败者删除；再 bind 仍被拒。
        assert!(endpoint_path.exists(), "活端点文件不得被并发败者删除");
        let third = uds::UdsIpcListener::new_with_name(dir.clone(), name.clone());
        let err = third.bind().await.expect_err("活端点必须继续被拒");
        assert!(err.detail.contains("already served"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// H05 审查 C2-r2：探活失败分类——只有可确认残留（NotFound/ECONNREFUSED）
    /// 允许接管；其它错误保守拒绝（绝不 unlink 一个可能仍活着的端点）。
    #[test]
    fn stale_takeover_only_for_confirmable_remnants() {
        use std::io::ErrorKind;
        assert!(uds::stale_takeover_allowed(&std::io::Error::from(
            ErrorKind::NotFound
        )));
        assert!(uds::stale_takeover_allowed(&std::io::Error::from(
            ErrorKind::ConnectionRefused
        )));
        // 本地错误（fd 耗尽/权限/未知）保守拒绝——端点可能仍活。
        assert!(!uds::stale_takeover_allowed(&std::io::Error::from(
            ErrorKind::PermissionDenied
        )));
        assert!(!uds::stale_takeover_allowed(&std::io::Error::from(
            ErrorKind::Other
        )));
        assert!(!uds::stale_takeover_allowed(&std::io::Error::other(
            "emfile-like"
        )));
    }

    /// bind 时同样拦截（不止 serve 早退路径）：深目录不得先落盘目录再失败。
    #[tokio::test]
    async fn uds_bind_rejects_deep_state_dir_before_touching_disk() {
        let deep = std::env::temp_dir().join("d".repeat(90));
        let listener = uds::UdsIpcListener::new(deep.clone());
        let err = listener.bind().await.unwrap_err();
        assert_eq!(err.code, codes::STATE_PATH_TOO_LONG);
        assert!(
            !deep.exists(),
            "路径策略必须先于 create_dir_all（不得留下半个深目录）"
        );
    }
}
