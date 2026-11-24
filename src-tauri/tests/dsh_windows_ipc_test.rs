//! H06：生产 Windows named pipe 与秘密文件 ACL 的待运行行为测试。
//!
//! 运行前在 Windows 安装 Node，并构建 packages/dsh-remote-access 的 lib 产物。
//! 本文件不使用 mem IPC、不注入 PeerOsIdentity，也不替生产文件设置合格 ACL。
//! Everyone ACL 只用于真实的宽权限父目录和远端连通性对照。
//!
//! macOS 上 cfg 会排除整份测试；零测试通过不构成 RED/GREEN。交叉编译只能检查
//! Windows 分支类型/链接，不能证明内核 PID、DACL、远端拒绝、EOF 或取消行为。
//! 远端用例默认 ignore，须提供第二台 Windows 的 PowerShell Remoting 环境；
//! 同一远端连接 Everyone 对照成功后，才接受生产管道的明确 AccessDenied。
//! 原 13 项保留，另补 accept 取消/stop 重绑/队列积压/双向背压/EOF 队列与
//! authorize_grant 的真实进程旧启动时刻拒绝。全部异步用例经 Job 隔离 worker，
//! 父进程设置总期限，并核对 worker 确实启动了选定测试，防零用例假通过。
//! 尚未覆盖：异用户令牌实际读写/连接、真实 Desktop 完整握手与启动代际拒绝、
//! ACL 设置失败时关闭能力、文件替换瞬间权限、非系统盘实际安装及 G1-Win。

#![cfg(windows)]

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures::stream::FuturesUnordered;
use futures::StreamExt;
use multi_agents_manager_lib::services::dsh_control::auth::{
    b64url_decode, b64url_encode, random_u64, DshAuthService, RustSigningKey,
};
use multi_agents_manager_lib::services::dsh_control::error::{codes, DshError};
use multi_agents_manager_lib::services::dsh_control::local_ipc::{
    check_socket_path_length, check_state_dir_length, production_listener, IpcClientConnection,
    IpcFut, IpcListener,
};
use multi_agents_manager_lib::services::dsh_control::model::{
    ClientIdentity, DshEnvironment, DshProcess, DshProcessSource, LocalHost,
};
use multi_agents_manager_lib::services::dsh_control::production::{
    build_env, checked_state_dir, DshEnvSnapshot, SysinfoProcessSource,
};
use serde_json::{json, Value};
use tempfile::TempDir;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::time::timeout;
use windows::core::PCWSTR;
use windows::Win32::Foundation::{CloseHandle, FILETIME, HANDLE};
use windows::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
    SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
};
use windows::Win32::System::Threading::{
    GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
};

const IO_WAIT: Duration = Duration::from_secs(10);
const CLOSE_WAIT: Duration = Duration::from_secs(3);
const INSTANCE: &str = "0123456789abcdef0123456789abcdef";
const WORKER_ENV: &str = "TUVIS_DSH_H06_TEST_WORKER";
const WORKER_ACK_ENV: &str = "TUVIS_DSH_H06_TEST_WORKER_ACK";
const FLOW_FRAMES: usize = 512;
const WRITE_REQUESTS: usize = 96;

// 同步 stop/close 若阻塞，Tokio timeout 无法抢占。父测试进程因此监控独立 worker，
// 并先把 worker 加入 KILL_ON_JOB_CLOSE Job，再经 stdin 放行；避免 Node 在入 Job
// 前被派生。worker 的所有本机后代均继承 Job，超时/断言失败也会清理整棵树。
struct ProcessJob(HANDLE);

impl ProcessJob {
    fn new() -> Self {
        // SAFETY: 无名称、无可继承属性；返回句柄只由本 RAII 守卫持有。
        let job = Self(unsafe { CreateJobObjectW(None, PCWSTR::null()) }.expect("create test Job"));
        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        // SAFETY: 描述符指针和大小对应有效的 JOBOBJECT_EXTENDED_LIMIT_INFORMATION。
        unsafe {
            SetInformationJobObject(
                job.0,
                JobObjectExtendedLimitInformation,
                (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                std::mem::size_of_val(&limits) as u32,
            )
        }
        .expect("enable descendant cleanup; a Job setup error is a prerequisite failure");
        job
    }

    fn assign(&self, child: &Child) {
        let raw = child.raw_handle().expect("live worker process handle");
        // SAFETY: 只借用 Tokio 拥有的进程句柄，不关闭/转移该句柄。
        unsafe { AssignProcessToJobObject(self.0, HANDLE(raw)) }
            .expect("assign test worker to cleanup Job; do not silently run without cleanup");
    }
}

impl Drop for ProcessJob {
    fn drop(&mut self) {
        // SAFETY: 这是本守卫唯一拥有的 Job 句柄；关闭时内核终止仍存活的后代。
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

// true = 父测试已验证 worker 退出，可以返回；false = 当前是执行真实断言的 worker。
async fn isolated_worker(name: &str) -> bool {
    if std::env::var(WORKER_ENV).ok().as_deref() == Some(name) {
        let mut gate = String::new();
        timeout(
            IO_WAIT,
            BufReader::new(tokio::io::stdin()).read_line(&mut gate),
        )
        .await
        .expect("parent did not attach cleanup Job before the worker deadline")
        .expect("read worker start gate");
        assert_eq!(gate, "run\n", "worker may only start after Job assignment");
        let ack = std::env::var_os(WORKER_ACK_ENV).expect("worker acknowledgement path");
        std::fs::write(ack, name).expect("acknowledge the exact behavior test actually started");
        return false;
    }
    let acknowledgement = tempfile::tempdir().expect("worker acknowledgement directory");
    let ack = acknowledgement.path().join("started");
    let job = ProcessJob::new();
    let mut child = Command::new(std::env::current_exe().expect("current test executable"))
        .args(["--exact", name, "--nocapture", "--include-ignored"])
        .env(WORKER_ENV, name)
        .env(WORKER_ACK_ENV, &ack)
        .stdin(Stdio::piped())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn isolated Windows test worker");
    job.assign(&child);
    let mut gate = child.stdin.take().unwrap();
    timeout(IO_WAIT, gate.write_all(b"run\n"))
        .await
        .unwrap()
        .unwrap();
    drop(gate);
    let budget = if name.starts_with("remote_client_") {
        75
    } else {
        60
    };
    match timeout(Duration::from_secs(budget), child.wait()).await {
        Ok(result) => assert!(
            result.expect("wait test worker").success(),
            "Windows behavior worker failed: {name}"
        ),
        Err(_) => {
            // 先关 Job 杀掉 worker 和 Node/PowerShell，再在独立短期限内收尸。
            drop(job);
            let _ = timeout(CLOSE_WAIT, child.wait()).await;
            panic!("Windows worker exceeded {budget}s; process tree terminated: {name}");
        }
    }
    // 成功退出也关闭 Job，清除 worker 未回收的本机后代。
    drop(job);
    assert_eq!(
        std::fs::read_to_string(&ack).expect("zero selected tests is not a passing worker"),
        name,
        "the exact requested test must have executed"
    );
    true
}

// 所有子进程均有期限、kill_on_drop 和独立脚本目录；不改全局 HOME/DSH_HOME。
struct ScriptPeer {
    child: Child,
    input: ChildStdin,
    output: Lines<BufReader<ChildStdout>>,
    _scripts: TempDir,
}

impl ScriptPeer {
    fn spawn(program: &str, extension: &str, source: &str, args: &[&OsStr]) -> Self {
        let scripts = tempfile::tempdir().expect("create script directory");
        let script = scripts.path().join(format!("probe.{extension}"));
        // Windows PowerShell 5.1 依赖 BOM 来识别脚本 UTF-8。
        let prefix = if extension == "ps1" { "\u{feff}" } else { "" };
        std::fs::write(&script, format!("{prefix}{source}")).expect("write test script");
        let mut command = Command::new(program);
        if extension == "ps1" {
            command.args(["-NoLogo", "-NoProfile", "-NonInteractive", "-File"]);
        }
        let mut child = command
            .arg(&script)
            .args(args)
            .current_dir(scripts.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .expect("test prerequisite: Node / Windows PowerShell must be available");
        let input = child.stdin.take().expect("child stdin");
        let output = BufReader::new(child.stdout.take().expect("child stdout")).lines();
        Self {
            child,
            input,
            output,
            _scripts: scripts,
        }
    }

    async fn line(&mut self) -> String {
        timeout(IO_WAIT, self.output.next_line())
            .await
            .expect("child output timed out; this is not evidence of access denial")
            .expect("read child output")
            .expect("child exited before producing the expected observation")
    }

    async fn command(&mut self, line: &str) {
        timeout(
            IO_WAIT,
            self.input.write_all(format!("{line}\n").as_bytes()),
        )
        .await
        .expect("child command timed out")
        .expect("write child command");
    }

    async fn success(&mut self) {
        let status = timeout(IO_WAIT, self.child.wait())
            .await
            .expect("child exit timed out")
            .expect("wait child");
        assert!(status.success(), "test helper failed: {status}");
    }

    async fn terminate(&mut self) {
        // 压力客户端刻意不自行配合退出；测试断言后显式终止并在短期限内收尸。
        let _ = self.child.start_kill();
        timeout(CLOSE_WAIT, self.child.wait())
            .await
            .expect("test peer did not exit after termination")
            .expect("reap terminated test peer");
    }
}

struct BoundPipe {
    listener: Box<dyn IpcListener>,
    address: String,
    _root: TempDir,
}

impl BoundPipe {
    async fn new() -> Self {
        let root = tempfile::tempdir().expect("create isolated pipe state root");
        Self::at(root, PathBuf::from("state")).await
    }

    async fn at(root: TempDir, relative: PathBuf) -> Self {
        let listener = production_listener(root.path().join(relative));
        let address = timeout(IO_WAIT, listener.bind())
            .await
            .expect("production bind timed out")
            .expect("H06 requires a production pipe; DSH_UNSUPPORTED is not success");
        assert!(
            address.starts_with(r"\\.\pipe\"),
            "production must return a Windows named pipe: {address}"
        );
        assert_eq!(listener.endpoint().as_deref(), Some(address.as_str()));
        Self {
            listener,
            address,
            _root: root,
        }
    }

    async fn accept(&self) -> Arc<dyn IpcClientConnection> {
        Arc::from(
            timeout(IO_WAIT, self.listener.accept())
                .await
                .expect("production accept timed out")
                .expect("production accept must obtain an OS peer"),
        )
    }

    fn node(&self, mode: &str) -> ScriptPeer {
        ScriptPeer::spawn(
            "node",
            "mjs",
            NODE_PIPE_CLIENT,
            &[
                OsStr::new(&self.address),
                OsStr::new(mode),
                OsStr::new(&std::process::id().to_string()),
            ],
        )
    }
}

impl Drop for BoundPipe {
    fn drop(&mut self) {
        self.listener.stop();
    }
}

const NODE_PIPE_CLIENT: &str = r#"
import net from 'node:net';
import readline from 'node:readline';
const [address, mode, claimedPid] = process.argv.slice(2);
const guard = setTimeout(() => { console.error('pipe helper deadline'); process.exit(2); }, 20000);
const input = readline.createInterface({ input: process.stdin });
const socket = net.connect(address, () => {
  console.log('connected');
  if (mode === 'frames') {
    const frame = Buffer.from('{"text":"中文 pipe"}\nsecond\n');
    const split = frame.indexOf(Buffer.from('中')) + 1;
    socket.write(frame.subarray(0, split));
    socket.write(frame.subarray(split));
  }
  if (mode === 'identity') {
    socket.write(JSON.stringify({ pid: Number(claimedPid), startedAtMs: 1 }) + '\n');
  }
});
let received = '';
socket.setEncoding('utf8');
socket.on('data', chunk => {
  received += chunk;
  for (;;) {
    const n = received.indexOf('\n');
    if (n < 0) break;
    console.log(JSON.stringify({ received: received.slice(0, n) }));
    received = received.slice(n + 1);
  }
});
input.on('line', command => {
  if (command === 'end') socket.end();
});
socket.on('end', () => console.log('eof'));
socket.on('error', error => { console.error(error.code); process.exitCode = 2; });
socket.on('close', () => { clearTimeout(guard); input.close(); process.stdin.destroy(); });
"#;

#[tokio::test]
async fn production_pipe_exchanges_ndjson_with_a_real_node_client() {
    if isolated_worker("production_pipe_exchanges_ndjson_with_a_real_node_client").await {
        return;
    }
    let pipe = BoundPipe::new().await;
    let mut node = pipe.node("frames");
    let connection = pipe.accept().await;
    assert_eq!(node.line().await, "connected");
    assert_eq!(
        timeout(IO_WAIT, connection.recv()).await.unwrap().unwrap(),
        r#"{"text":"中文 pipe"}"#
    );
    assert_eq!(
        timeout(IO_WAIT, connection.recv()).await.unwrap().unwrap(),
        "second"
    );
    timeout(IO_WAIT, connection.send(r#"{"reply":"收到"}"#))
        .await
        .unwrap()
        .unwrap();
    let observed: Value = serde_json::from_str(&node.line().await).unwrap();
    assert_eq!(observed, json!({ "received": r#"{"reply":"收到"}"# }));
    connection.close();
    node.success().await;
}

#[tokio::test]
async fn production_pipe_peer_pid_ignores_the_pid_claimed_in_node_payload() {
    if isolated_worker("production_pipe_peer_pid_ignores_the_pid_claimed_in_node_payload").await {
        return;
    }
    let pipe = BoundPipe::new().await;
    let mut node = pipe.node("identity");
    let node_pid = node.child.id().expect("live Node PID from CreateProcess");
    let connection = pipe.accept().await;
    assert_eq!(node.line().await, "connected");
    let frame = timeout(IO_WAIT, connection.recv()).await.unwrap().unwrap();
    let claimed: Value = serde_json::from_str(&frame).unwrap();
    assert_eq!(claimed["pid"], std::process::id());
    assert_ne!(node_pid, std::process::id());
    assert_eq!(
        connection.peer_os().pid,
        node_pid,
        "PID must come from the kernel"
    );
    connection.close();
    node.success().await;
}

// 独立用 GetProcessTimes 取证，避免用 SysinfoProcessSource 自己算期望。
fn kernel_start_ms(pid: u32) -> i64 {
    // SAFETY: OpenProcess 返回本测试拥有的句柄；FILETIME 指针均指向有效可写对象。
    // GetProcessTimes 后无论成功与否都先关闭句柄。
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid)
            .expect("query live Node process");
        let mut created = FILETIME::default();
        let mut exited = FILETIME::default();
        let mut kernel = FILETIME::default();
        let mut user = FILETIME::default();
        let result = GetProcessTimes(handle, &mut created, &mut exited, &mut kernel, &mut user);
        CloseHandle(handle).expect("close process query handle");
        result.expect("kernel process creation time");
        let ticks = ((created.dwHighDateTime as u64) << 32) | created.dwLowDateTime as u64;
        // FILETIME = 自 1601 起的 100ns；Unix epoch 差为固定 11644473600 秒。
        (ticks / 10_000) as i64 - 11_644_473_600_000
    }
}

#[tokio::test]
async fn production_process_source_uses_kernel_start_time_instead_of_node_claim() {
    if isolated_worker("production_process_source_uses_kernel_start_time_instead_of_node_claim")
        .await
    {
        return;
    }
    let pipe = BoundPipe::new().await;
    let mut node = pipe.node("identity");
    let node_pid = node.child.id().unwrap();
    let connection = pipe.accept().await;
    assert_eq!(node.line().await, "connected");
    let frame = timeout(IO_WAIT, connection.recv()).await.unwrap().unwrap();
    let claimed: Value = serde_json::from_str(&frame).unwrap();
    assert_eq!(claimed["startedAtMs"], 1);
    let expected_ms = kernel_start_ms(node_pid);
    let process = tokio::task::spawn_blocking(move || {
        SysinfoProcessSource::new()
            .list_processes()
            .into_iter()
            .find(|p| p.pid == node_pid)
            .expect("production process table must contain the live Node peer")
    })
    .await
    .unwrap();
    // 生产 sysinfo 投影只保留整秒；不要求它伪装成毫秒精度。
    assert_eq!(process.started_at_ms, expected_ms / 1000 * 1000);
    assert_ne!(process.started_at_ms, 1);
    connection.close();
    node.success().await;
}

#[tokio::test]
async fn close_unblocks_an_already_pending_production_pipe_read() {
    if isolated_worker("close_unblocks_an_already_pending_production_pipe_read").await {
        return;
    }
    let pipe = BoundPipe::new().await;
    let mut node = pipe.node("idle");
    let connection = pipe.accept().await;
    assert_eq!(node.line().await, "connected");
    let pending = connection.recv();
    tokio::pin!(pending);
    // 真正 poll 同一个 recv，确认它在没有帧时挂起；随后不取消该 future。
    tokio::select! {
        result = &mut pending => panic!("idle recv completed before close: {result:?}"),
        _ = tokio::time::sleep(Duration::from_millis(100)) => {}
    }
    connection.clone().close();
    assert!(timeout(CLOSE_WAIT, &mut pending)
        .await
        .expect("close must release the pending read without taking its lock")
        .is_err());
    assert!(connection.is_closed());
    assert!(timeout(CLOSE_WAIT, connection.send("after close"))
        .await
        .unwrap()
        .is_err());
    assert_eq!(node.line().await, "eof", "the peer must also observe EOF");
    node.success().await;
}

#[tokio::test]
async fn node_eof_is_reported_as_an_error_instead_of_an_empty_frame() {
    if isolated_worker("node_eof_is_reported_as_an_error_instead_of_an_empty_frame").await {
        return;
    }
    let pipe = BoundPipe::new().await;
    let mut node = pipe.node("idle");
    let connection = pipe.accept().await;
    assert_eq!(node.line().await, "connected");
    node.command("end").await;
    assert!(timeout(CLOSE_WAIT, connection.recv())
        .await
        .unwrap()
        .is_err());
    connection.close();
    node.success().await;
}

// 真实客户端支持暂停读、受写回调约束的入站灌流、半关闭和启动身份声明。
// 只打印事件/计数，不把大帧或密钥私有部分写到 stdout。
const NODE_FLOW_CLIENT: &str = r#"
import net from 'node:net';
import readline from 'node:readline';
import { generateKeyPairSync } from 'node:crypto';
const [address, mode, claimedStart] = process.argv.slice(2);
const guard = setTimeout(() => { console.error('flow helper deadline'); process.exit(2); }, 45000);
const input = readline.createInterface({ input: process.stdin });
let completed = 0;
let pumping = false;
let received = '';
const seen = new Set();
const socket = net.connect({ path: address, allowHalfOpen: true }, () => {
  if (mode === 'paused') socket.pause();
  console.log('connected');
  if (mode === 'eof') {
    socket.end(Array.from({ length: 8 }, (_, i) => `queued:${i}\n`).join(''), () => console.log('sent-eof'));
  }
  if (mode === 'identity') {
    const { publicKey } = generateKeyPairSync('ed25519');
    socket.write(JSON.stringify({
      pid: process.pid, startedAtMs: Number(claimedStart), executable: process.execPath,
      publicKeySpki: publicKey.export({ type: 'spki', format: 'der' }).toString('base64url'),
    }) + '\n');
  }
});
socket.setEncoding('utf8');
socket.on('data', chunk => {
  received += chunk;
  for (;;) {
    const n = received.indexOf('\n');
    if (n < 0) break;
    const line = received.slice(0, n);
    received = received.slice(n + 1);
    if (!/^\d{3}:w{65500}$/.test(line)) throw new Error('invalid outbound frame');
    const index = Number(line.slice(0, 3));
    if (index >= 96 || seen.has(index)) throw new Error('duplicate/out-of-range outbound frame');
    seen.add(index);
    if (seen.size === 96) console.log('all-received');
  }
});
async function pump() {
  if (pumping) throw new Error('pump started twice');
  pumping = true;
  const pad = 'r'.repeat(65500);
  for (let i = 0; i < 512; i++) {
    const frame = `in:${String(i).padStart(3, '0')}:${pad}\n`;
    await new Promise((resolve, reject) => socket.write(frame, error => error ? reject(error) : resolve()));
    completed++;
  }
  pumping = false;
  console.log('pump-complete');
}
input.on('line', command => {
  if (command === 'pump') pump().catch(error => console.log(`pump-error:${error.code}`));
  if (command === 'status') console.log(JSON.stringify({ completed, pumping }));
  if (command === 'probe') socket.write('read-probe\n', () => console.log('probe-sent'));
  if (command === 'resume') { socket.resume(); console.log('resumed'); }
});
// 保持本端写半区，避免靠客户端自动退出才让服务端 close 看起来成功。
socket.on('end', () => console.log('eof'));
socket.on('error', error => {
  console.log(`error:${error.code}`);
  if (!['EPIPE', 'ECONNRESET'].includes(error.code)) process.exitCode = 2;
});
socket.on('close', () => { clearTimeout(guard); console.log('closed'); input.close(); process.stdin.destroy(); });
"#;

fn flow_node(pipe: &BoundPipe, mode: &str, claimed_start: i64) -> ScriptPeer {
    ScriptPeer::spawn(
        "node",
        "mjs",
        NODE_FLOW_CLIENT,
        &[
            OsStr::new(&pipe.address),
            OsStr::new(mode),
            OsStr::new(&claimed_start.to_string()),
        ],
    )
}

async fn wait_closed(connection: &dyn IpcClientConnection) {
    timeout(CLOSE_WAIT, async {
        while !connection.is_closed() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("production must observe peer EOF within the close deadline");
}

async fn wait_node_event(node: &mut ScriptPeer, expected: &str) {
    timeout(CLOSE_WAIT, async {
        loop {
            let line = node
                .output
                .next_line()
                .await
                .unwrap()
                .expect("peer exited before close event");
            if line == expected {
                break;
            }
            assert!(
                line.starts_with("pump-error:"),
                "unexpected peer event while waiting for {expected}: {line}"
            );
        }
    })
    .await
    .expect("peer did not observe the expected close event");
}

// 不调用 recv，让真实读任务/入站队列和内核缓冲共同承压。客户端每帧都等待真实
// 写完成回调；连续采样确认其进度停滞且尚未写完有界 32 MiB 工作量，不以 sleep
// 后直接假定队列已满。这里不伪造队列，也不把具体队列容量当作 API。
async fn establish_incoming_pressure(node: &mut ScriptPeer) {
    node.command("pump").await;
    timeout(CLOSE_WAIT, async {
        let mut previous = 0;
        let mut stable = 0;
        loop {
            node.command("status").await;
            let observed: Value = serde_json::from_str(&node.line().await)
                .expect("expected real write-progress observation, not pump completion/error");
            let completed = observed["completed"].as_u64().unwrap();
            assert!(
                completed < FLOW_FRAMES as u64,
                "bounded incoming load never encountered backpressure"
            );
            assert_eq!(observed["pumping"], true);
            if completed > 0 && completed == previous {
                stable += 1;
            } else {
                stable = 0;
            }
            if stable == 3 {
                break;
            }
            previous = completed;
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("could not establish sustained real incoming backpressure");
}

fn outbound_frames() -> Vec<String> {
    (0..WRITE_REQUESTS)
        .map(|i| format!("{i:03}:{}", "w".repeat(65500)))
        .collect()
}

async fn establish_outgoing_pressure(
    pending: &mut FuturesUnordered<IpcFut<'_, Result<(), DshError>>>,
) {
    let progress = timeout(Duration::from_millis(300), async {
        while let Some(result) = pending.next().await {
            result.expect("live peer write before close");
        }
    })
    .await;
    assert!(
        progress.is_err() && !pending.is_empty(),
        "a peer that never reads must exert real write backpressure"
    );
}

#[tokio::test]
async fn cancelled_accept_leaves_the_listener_usable_for_a_real_node_client() {
    if isolated_worker("cancelled_accept_leaves_the_listener_usable_for_a_real_node_client").await {
        return;
    }
    let pipe = BoundPipe::new().await;
    for _ in 0..2 {
        // timeout 真实 poll 然后丢弃 accept future；下一次 accept 仍须能接住客户端。
        assert!(timeout(Duration::from_millis(100), pipe.listener.accept())
            .await
            .is_err());
    }
    let mut node = flow_node(&pipe, "idle", 0);
    let connection = pipe.accept().await;
    assert_eq!(node.line().await, "connected");
    assert_eq!(connection.peer_os().pid, node.child.id().unwrap());
    connection.close();
    node.terminate().await;
}

#[tokio::test]
async fn stop_wakes_an_already_pending_accept_with_an_error() {
    if isolated_worker("stop_wakes_an_already_pending_accept_with_an_error").await {
        return;
    }
    let pipe = BoundPipe::new().await;
    let pending = pipe.listener.accept();
    tokio::pin!(pending);
    assert!(timeout(Duration::from_millis(100), &mut pending)
        .await
        .is_err());
    pipe.listener.stop();
    assert!(timeout(CLOSE_WAIT, &mut pending)
        .await
        .expect("stop must wake the original accept")
        .is_err());
    assert!(pipe.listener.endpoint().is_none());
}

#[tokio::test]
async fn stop_then_rebind_preserves_previously_accepted_connections() {
    if isolated_worker("stop_then_rebind_preserves_previously_accepted_connections").await {
        return;
    }
    let mut pipe = BoundPipe::new().await;
    let mut old_node = flow_node(&pipe, "idle", 0);
    let old_connection = pipe.accept().await;
    assert_eq!(old_node.line().await, "connected");
    pipe.listener.stop();
    assert!(pipe.listener.endpoint().is_none());
    pipe.address = timeout(IO_WAIT, pipe.listener.bind())
        .await
        .unwrap()
        .expect("same listener must rebind after stop");
    assert_eq!(
        pipe.listener.endpoint().as_deref(),
        Some(pipe.address.as_str())
    );
    let mut fresh_node = flow_node(&pipe, "idle", 0);
    let fresh_connection = pipe.accept().await;
    assert_eq!(fresh_node.line().await, "connected");
    // IpcListener::stop 契约：不影响已建立连接；重绑不得关掉旧的已接收连接。
    old_node.command("probe").await;
    assert_eq!(old_node.line().await, "probe-sent");
    assert_eq!(
        timeout(IO_WAIT, old_connection.recv())
            .await
            .unwrap()
            .unwrap(),
        "read-probe"
    );
    old_connection.close();
    fresh_connection.close();
    old_node.terminate().await;
    fresh_node.terminate().await;
}

#[tokio::test]
async fn stopped_accept_cannot_consume_a_client_from_the_rebound_generation() {
    if isolated_worker("stopped_accept_cannot_consume_a_client_from_the_rebound_generation").await {
        return;
    }
    let pipe = BoundPipe::new().await;
    for _ in 0..3 {
        let stale = pipe.listener.accept();
        tokio::pin!(stale);
        assert!(timeout(Duration::from_millis(100), &mut stale)
            .await
            .is_err());
        pipe.listener.stop();
        // 先 poll 重绑，再 poll 旧 accept，覆盖 watch 标记重置/迟到清理的交错。
        let (rebound, old_result) = timeout(IO_WAIT, async {
            tokio::join!(biased; pipe.listener.bind(), &mut stale)
        })
        .await
        .expect("stop/rebind and the old accept must both settle");
        let rebound = rebound.expect("rebind after stop");
        assert!(
            old_result.is_err(),
            "an old accept must terminate, not cross the stop boundary"
        );
        assert_eq!(pipe.listener.endpoint().as_deref(), Some(rebound.as_str()));
        let mut node = ScriptPeer::spawn(
            "node",
            "mjs",
            NODE_FLOW_CLIENT,
            &[OsStr::new(&rebound), OsStr::new("idle"), OsStr::new("0")],
        );
        let connection = pipe.accept().await;
        assert_eq!(node.line().await, "connected");
        assert_eq!(connection.peer_os().pid, node.child.id().unwrap());
        connection.close();
        node.terminate().await;
    }
}

#[tokio::test]
async fn close_releases_the_peer_when_the_incoming_queue_is_backpressured() {
    if isolated_worker("close_releases_the_peer_when_the_incoming_queue_is_backpressured").await {
        return;
    }
    let pipe = BoundPipe::new().await;
    let mut node = flow_node(&pipe, "flood", 0);
    let connection = pipe.accept().await;
    assert_eq!(node.line().await, "connected");
    establish_incoming_pressure(&mut node).await;
    connection.clone().close();
    assert!(
        timeout(CLOSE_WAIT, connection.recv())
            .await
            .unwrap()
            .is_err(),
        "explicit close must not drain the backlog"
    );
    wait_node_event(&mut node, "eof").await;
    // Node allowHalfOpen=true，仍保持进程/写半区；不能靠它先退出来解除服务端阻塞。
    assert!(node.child.try_wait().unwrap().is_none());
    node.terminate().await;
}

#[tokio::test]
async fn incoming_backpressure_recovers_without_losing_or_reordering_frames() {
    if isolated_worker("incoming_backpressure_recovers_without_losing_or_reordering_frames").await {
        return;
    }
    let pipe = BoundPipe::new().await;
    let mut node = flow_node(&pipe, "flood", 0);
    let connection = pipe.accept().await;
    assert_eq!(node.line().await, "connected");
    establish_incoming_pressure(&mut node).await;
    timeout(IO_WAIT, async {
        for index in 0..FLOW_FRAMES {
            let frame = connection.recv().await.expect("drain real incoming frame");
            assert!(
                frame.starts_with(&format!("in:{index:03}:")),
                "incoming sequence mismatch at {index}"
            );
            assert_eq!(frame.len(), 65507);
            assert!(frame.as_bytes()[7..].iter().all(|byte| *byte == b'r'));
        }
    })
    .await
    .expect("all incoming frames must drain within the recovery deadline");
    assert_eq!(node.line().await, "pump-complete");
    connection.close();
    node.terminate().await;
}

#[tokio::test]
async fn outgoing_backpressure_does_not_block_incoming_frames() {
    if isolated_worker("outgoing_backpressure_does_not_block_incoming_frames").await {
        return;
    }
    let pipe = BoundPipe::new().await;
    let mut node = flow_node(&pipe, "paused", 0);
    let connection = pipe.accept().await;
    assert_eq!(node.line().await, "connected");
    let frames = outbound_frames();
    let mut pending: FuturesUnordered<_> =
        frames.iter().map(|frame| connection.send(frame)).collect();
    establish_outgoing_pressure(&mut pending).await;
    node.command("probe").await;
    assert_eq!(node.line().await, "probe-sent");
    assert_eq!(
        timeout(CLOSE_WAIT, connection.recv())
            .await
            .unwrap()
            .unwrap(),
        "read-probe",
        "a blocked writer must not own the read-side lock"
    );
    node.command("resume").await;
    assert_eq!(node.line().await, "resumed");
    timeout(IO_WAIT, async {
        while let Some(result) = pending.next().await {
            result.expect("write must recover when the peer resumes reading");
        }
    })
    .await
    .expect("outgoing backpressure did not recover");
    assert_eq!(
        node.line().await,
        "all-received",
        "Node must verify all 96 distinct large frames"
    );
    connection.clone().close();
    node.terminate().await;
}

#[tokio::test]
async fn close_releases_all_writers_blocked_by_the_outgoing_queue_and_kernel_pipe() {
    if isolated_worker("close_releases_all_writers_blocked_by_the_outgoing_queue_and_kernel_pipe")
        .await
    {
        return;
    }
    let pipe = BoundPipe::new().await;
    let mut node = flow_node(&pipe, "paused", 0);
    let connection = pipe.accept().await;
    assert_eq!(node.line().await, "connected");
    let frames = outbound_frames();
    let mut pending: FuturesUnordered<_> =
        frames.iter().map(|frame| connection.send(frame)).collect();
    establish_outgoing_pressure(&mut pending).await;
    let blocked = pending.len();
    assert!(blocked > 0);
    connection.clone().close();
    timeout(CLOSE_WAIT, async {
        let mut released = 0;
        while let Some(result) = pending.next().await {
            assert!(
                result.is_err(),
                "writes still blocked at close must not report success"
            );
            released += 1;
        }
        assert_eq!(released, blocked);
    })
    .await
    .expect("close must release queued senders and the in-flight write");
    assert!(connection.is_closed());
    node.terminate().await;
}

#[tokio::test]
async fn peer_eof_preserves_frames_already_queued_before_the_connection_closed() {
    if isolated_worker("peer_eof_preserves_frames_already_queued_before_the_connection_closed")
        .await
    {
        return;
    }
    let pipe = BoundPipe::new().await;
    let mut node = flow_node(&pipe, "eof", 0);
    let connection = pipe.accept().await;
    assert_eq!(node.line().await, "connected");
    assert_eq!(node.line().await, "sent-eof");
    wait_closed(connection.as_ref()).await;
    // 必须先观察生产关闭态，再开始 recv；避免只测到 EOF 到来之前的正常读取。
    for index in 0..8 {
        assert_eq!(
            timeout(CLOSE_WAIT, connection.recv())
                .await
                .unwrap()
                .unwrap(),
            format!("queued:{index}")
        );
    }
    assert!(timeout(CLOSE_WAIT, connection.recv())
        .await
        .unwrap()
        .is_err());
    connection.close();
    node.success().await;
}

#[tokio::test]
async fn explicit_close_discards_frames_that_were_queued_before_peer_eof() {
    if isolated_worker("explicit_close_discards_frames_that_were_queued_before_peer_eof").await {
        return;
    }
    let pipe = BoundPipe::new().await;
    let mut node = flow_node(&pipe, "eof", 0);
    let connection = pipe.accept().await;
    assert_eq!(node.line().await, "connected");
    assert_eq!(node.line().await, "sent-eof");
    wait_closed(connection.as_ref()).await;
    connection.clone().close();
    assert!(
        timeout(CLOSE_WAIT, connection.recv())
            .await
            .unwrap()
            .is_err(),
        "explicit cancellation must override queued frames even after EOF"
    );
    node.success().await;
}

async fn live_process(pid: u32) -> DshProcess {
    timeout(
        IO_WAIT,
        tokio::task::spawn_blocking(move || {
            SysinfoProcessSource::new()
                .list_processes()
                .into_iter()
                .find(|process| process.pid == pid)
                .expect("real live Node process must be present in the production process table")
        }),
    )
    .await
    .expect("production process snapshot deadline")
    .unwrap()
}

#[tokio::test]
async fn authorization_rejects_previous_process_start_time_for_a_live_node_pid() {
    if isolated_worker("authorization_rejects_previous_process_start_time_for_a_live_node_pid")
        .await
    {
        return;
    }
    let pipe = BoundPipe::new().await;
    let mut previous = flow_node(&pipe, "idle", 0);
    let previous_connection = pipe.accept().await;
    assert_eq!(previous.line().await, "connected");
    let previous_start = kernel_start_ms(previous_connection.peer_os().pid) / 1000 * 1000;
    previous_connection.close();
    previous.terminate().await;
    // Sysinfo 是整秒精度；在有界期限内跨过秒边界，不能把两个同秒启动冒充换代证据。
    timeout(IO_WAIT, async {
        while (SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64)
            < previous_start + 1000
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("could not establish distinct real process start generations");
    let mut node = flow_node(&pipe, "identity", previous_start);
    let connection = pipe.accept().await;
    assert_eq!(node.line().await, "connected");
    let actual_pid = connection.peer_os().pid;
    assert_eq!(actual_pid, node.child.id().unwrap());
    let frame = timeout(IO_WAIT, connection.recv()).await.unwrap().unwrap();
    let claimed: Value = serde_json::from_str(&frame).unwrap();
    assert_eq!(claimed["pid"], actual_pid);
    assert_eq!(claimed["startedAtMs"], previous_start);
    let process = live_process(actual_pid).await;
    assert_eq!(
        process.started_at_ms,
        kernel_start_ms(actual_pid) / 1000 * 1000
    );
    assert!(process.started_at_ms > previous_start);
    let root = tempfile::tempdir().unwrap();
    // authorize_grant 的接收者是 `self: &Arc<Self>`：此处必须持 Arc（Windows
    // 交叉编译门下首次实际编译该 cfg(windows) 用例暴露的既有缺口）。
    let service = Arc::new(
        DshAuthService::new(isolated_env(root.path(), &root.path().join("state"))).unwrap(),
    );
    // 目标只用于调用进程身份前置门，不声称此 Node 是已安装/已授权的 Desktop。
    let host = LocalHost {
        host_id: INSTANCE.into(),
        platform: "windows".into(),
        profile_dir: root.path().join("shadow-profile"),
        executable: process.exe.clone(),
        pid: actual_pid,
        started_at_ms: process.started_at_ms,
        epoch: 1,
        authority: "127.0.0.1:1".into(),
        package_sha256: String::new(),
    };
    let mut client = ClientIdentity {
        public_key_spki: b64url_decode(claimed["publicKeySpki"].as_str().unwrap()).unwrap(),
        executable: PathBuf::from(claimed["executable"].as_str().unwrap()),
        pid: actual_pid,
        started_at_ms: previous_start,
    };
    // 正向对照先证明 exe/PID 都正确，只替换启动时刻的负向声明才进入拒绝分支。
    assert_eq!(client.executable, process.exe);
    client.started_at_ms = process.started_at_ms;
    let fresh = timeout(IO_WAIT, service.authorize_grant(&host, &client))
        .await
        .unwrap()
        .expect_err("a generic Node process must not receive an authorized Desktop grant");
    assert_ne!(
        fresh.code,
        codes::PEER_MISMATCH,
        "real start identity must pass the process-identity gate"
    );
    client.started_at_ms = previous_start;
    let stale = timeout(IO_WAIT, service.authorize_grant(&host, &client))
        .await
        .unwrap()
        .expect_err("the same live PID must not authorize an old start generation");
    assert_eq!(stale.code, codes::PEER_MISMATCH);
    connection.close();
    node.terminate().await;
}

fn isolated_env(root: &Path, state: &Path) -> DshEnvironment {
    let mut env = build_env(
        &DshEnvSnapshot {
            dsh_state_dir: Some(state.to_owned()),
        },
        root.to_owned(),
    );
    env.home = root.join("shadow-dsh");
    env
}

#[test]
fn windows_state_resolution_does_not_apply_the_unix_socket_budget() {
    let root = tempfile::tempdir().unwrap();
    // 纯路径测试不假设 D: 真存在；非系统盘实装另需 Windows 机器验收。
    for state in [
        PathBuf::from(format!(r"C:\Users\中文 用户\{}", "状态目录 ".repeat(15))),
        PathBuf::from(format!(
            r"D:\非系统盘\Program Files\{}",
            "远程状态".repeat(20)
        )),
    ] {
        assert!(state.as_os_str().as_encoded_bytes().len() > 103);
        let env = isolated_env(root.path(), &state);
        assert_eq!(
            checked_state_dir(&env).expect("Windows has no sun_path budget"),
            state
        );
    }
}

#[test]
fn windows_path_guards_accept_long_state_roots_for_both_endpoint_name_lengths() {
    let state = PathBuf::from(format!(r"C:\中文 用户\{}", "长目录 ".repeat(20)));
    check_state_dir_length(&state).expect("77-byte guard must be Unix-only");
    check_socket_path_length(&state, 25)
        .expect("random endpoint name has no Windows sun_path limit");
    check_socket_path_length(&state, 41)
        .expect("instance endpoint name has no Windows sun_path limit");
}

#[tokio::test]
async fn production_pipe_binds_from_a_long_chinese_state_directory_with_spaces() {
    if isolated_worker("production_pipe_binds_from_a_long_chinese_state_directory_with_spaces")
        .await
    {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let relative = PathBuf::from("影子 状态目录".repeat(12));
    assert!(
        root.path()
            .join(&relative)
            .as_os_str()
            .as_encoded_bytes()
            .len()
            > 103
    );
    let pipe = BoundPipe::at(root, relative).await;
    let mut node = pipe.node("idle");
    let connection = pipe.accept().await;
    assert_eq!(node.line().await, "connected");
    connection.close();
    node.success().await;
}

// 只给测试父目录放宽权限。返回实际描述符，让测试确认宽 ACL 前置条件确实生效。
const ACL_PROBE: &str = r#"
param([string]$Mode, [string]$Target)
$ErrorActionPreference = 'Stop'
[Console]::OutputEncoding = [Text.UTF8Encoding]::new($false)
$current = [Security.Principal.WindowsIdentity]::GetCurrent().User
$everyone = [Security.Principal.SecurityIdentifier]::new('S-1-1-0')
$pipe = $null
if ($Mode -eq 'widen') {
  $acl = Get-Acl -LiteralPath $Target
  $acl.SetAccessRuleProtection($true, $false)
  $inherit = [Security.AccessControl.InheritanceFlags]'ContainerInherit, ObjectInherit'
  $propagate = [Security.AccessControl.PropagationFlags]::None
  $allow = [Security.AccessControl.AccessControlType]::Allow
  $acl.AddAccessRule([Security.AccessControl.FileSystemAccessRule]::new($current, 'FullControl', $inherit, $propagate, $allow))
  $acl.AddAccessRule([Security.AccessControl.FileSystemAccessRule]::new($everyone, 'Modify', $inherit, $propagate, $allow))
  Set-Acl -LiteralPath $Target -AclObject $acl
}
if ($Mode -eq 'pipe') {
  $name = $Target.Substring('\\.\pipe\'.Length)
  $pipe = [IO.Pipes.NamedPipeClientStream]::new('.', $name, [IO.Pipes.PipeDirection]::InOut)
  $pipe.Connect(5000)
  $acl = $pipe.GetAccessControl()
} else {
  $acl = Get-Acl -LiteralPath $Target
}
$rules = @($acl.GetAccessRules($true, $true, [Security.Principal.SecurityIdentifier]) | ForEach-Object {
  $rights = if ($Mode -eq 'pipe') { [int64]$_.PipeAccessRights } else { [int64]$_.FileSystemRights }
  @{ sid = $_.IdentityReference.Value; allow = ($_.AccessControlType -eq 'Allow'); rights = $rights; inherited = $_.IsInherited; inheritance = [int]$_.InheritanceFlags }
})
@{ current = $current.Value; owner = $acl.GetOwner([Security.Principal.SecurityIdentifier]).Value; protected = $acl.AreAccessRulesProtected; rules = $rules } | ConvertTo-Json -Depth 5 -Compress
if ($null -ne $pipe) { [void][Console]::ReadLine(); $pipe.Dispose() }
"#;

async fn acl(mode: &str, path: &Path) -> Value {
    let mut probe = ScriptPeer::spawn(
        "powershell.exe",
        "ps1",
        ACL_PROBE,
        &[OsStr::new(mode), path.as_os_str()],
    );
    let observed = serde_json::from_str(&probe.line().await).expect("actual ACL observation JSON");
    probe.success().await;
    observed
}

async fn wide_parent() -> TempDir {
    let root = tempfile::tempdir().unwrap();
    let observed = acl("widen", root.path()).await;
    assert!(
        observed["rules"].as_array().unwrap().iter().any(|rule|
            rule["sid"] == "S-1-1-0"
                && rule["allow"] == true
                && rule["inheritance"] == 3
                && rule["rights"].as_i64().unwrap() & 0x0001_0003 == 0x0001_0003
        ),
        "test prerequisite: Everyone must really have inheritable read/write/delete rights: {observed}"
    );
    root
}

fn assert_restricted_acl(observed: &Value, require_protected: bool) {
    let current = observed["current"].as_str().unwrap();
    let trusted = [current, "S-1-5-18", "S-1-5-32-544"];
    assert!(
        trusted.contains(&observed["owner"].as_str().unwrap()),
        "owner must be the current user, SYSTEM or Administrators: {observed}"
    );
    if require_protected {
        assert_eq!(
            observed["protected"], true,
            "the private state directory must stop inheriting the wide parent ACL: {observed}"
        );
    }
    let rules = observed["rules"].as_array().unwrap();
    assert!(!rules.is_empty(), "an empty/null DACL must not be accepted");
    assert!(rules
        .iter()
        .any(|rule| rule["sid"] == current && rule["allow"] == true));
    for rule in rules {
        if rule["allow"] == true && rule["rights"].as_i64().unwrap() != 0 {
            let sid = rule["sid"].as_str().unwrap();
            assert!(
                trusted.contains(&sid),
                "unexpected allowed principal in actual production ACL: {rule}"
            );
        }
    }
}

#[tokio::test]
async fn production_pipe_dacl_has_no_untrusted_allow_entries() {
    if isolated_worker("production_pipe_dacl_has_no_untrusted_allow_entries").await {
        return;
    }
    let pipe = BoundPipe::new().await;
    let mut probe = ScriptPeer::spawn(
        "powershell.exe",
        "ps1",
        ACL_PROBE,
        &[OsStr::new("pipe"), OsStr::new(&pipe.address)],
    );
    let connection = pipe.accept().await;
    let observed: Value = serde_json::from_str(&probe.line().await).unwrap();
    // 读取真实内核管道 DACL；这不是异用户令牌的实际连接测试。
    assert_restricted_acl(&observed, false);
    probe.command("release").await;
    probe.success().await;
    connection.close();
}

#[tokio::test]
async fn authorization_key_and_state_dir_restrict_a_wide_parent_acl() {
    if isolated_worker("authorization_key_and_state_dir_restrict_a_wide_parent_acl").await {
        return;
    }
    let root = wide_parent().await;
    let state = root.path().join("state");
    let service = DshAuthService::new_for_instance(isolated_env(root.path(), &state), INSTANCE)
        .expect("production private-key writer must work under a wide parent");
    let instance = state.join("instances").join(INSTANCE);
    let key = instance.join("authorization.key");
    let stored: Value = serde_json::from_slice(&std::fs::read(&key).unwrap()).unwrap();
    assert_eq!(stored["version"], 1);
    assert_eq!(service.public_key_spki().len(), 44);
    assert_restricted_acl(&acl("read", &instance).await, true);
    // 文件可以继承已经加固的专用目录；核验实际权限，不强制某种创建 API。
    assert_restricted_acl(&acl("read", &key).await, false);
}

#[tokio::test]
async fn binding_ledger_acl_stays_restricted_after_each_production_replacement() {
    if isolated_worker("binding_ledger_acl_stays_restricted_after_each_production_replacement")
        .await
    {
        return;
    }
    let root = wide_parent().await;
    let state = root.path().join("state");
    let instance = state.join("instances").join(INSTANCE);
    std::fs::create_dir_all(&instance).unwrap();
    let ledger = instance.join("bindings.json");
    // 只预置真实账本数据，不预置安全权限；两次 revoke 都走生产原子写入口。
    let public_key = b64url_encode(RustSigningKey::generate().spki.as_slice());
    let rows: Vec<Value> = ["first", "second"]
        .into_iter()
        .map(|id| {
            json!({
                "bindingId": id, "publicKeySpki": public_key, "revokedKeySpki": "",
                "executable": "test-node.exe", "pid": 1, "startedAtMs": 1,
                "profileDir": "isolated-profile", "createdAtMs": 1, "state": "bound"
            })
        })
        .collect();
    std::fs::write(
        &ledger,
        serde_json::to_vec(&json!({ "bindings": rows })).unwrap(),
    )
    .unwrap();
    let service =
        DshAuthService::new_for_instance(isolated_env(root.path(), &state), INSTANCE).unwrap();
    for (index, id) in ["first", "second"].into_iter().enumerate() {
        service
            .revoke_local(id)
            .expect("production ledger replacement");
        let stored: Value = serde_json::from_slice(&std::fs::read(&ledger).unwrap()).unwrap();
        assert_eq!(stored["bindings"][index]["state"], "revoked");
        assert_restricted_acl(&acl("read", &instance).await, true);
        assert_restricted_acl(&acl("read", &ledger).await, false);
    }
}

const NODE_IDENTITY_STORAGE: &str = r#"
import { generateKeyPairSync } from 'node:crypto';
import { mkdirSync, writeFileSync } from 'node:fs';
import { join, dirname } from 'node:path';
import { pathToFileURL } from 'node:url';
import readline from 'node:readline';
const [entry, home, state, instance] = process.argv.slice(2);
const { instanceFileStorage } = await import(pathToFileURL(entry).href);
const rendezvous = join(state, 'instances', instance, 'rendezvous.json');
mkdirSync(dirname(rendezvous), { recursive: true });
writeFileSync(rendezvous, JSON.stringify({ hostId: instance, processIdentity: { pid: process.pid } }));
const storage = instanceFileStorage(home, state);
const { privateKey, publicKey } = generateKeyPairSync('ed25519');
const identity = {
  privateKeyPem: privateKey.export({ type: 'pkcs8', format: 'pem' }).toString(),
  publicKeySpki: publicKey.export({ type: 'spki', format: 'der' }).toString('base64url'),
  revoked: false,
};
const input = readline.createInterface({ input: process.stdin });
const lines = input[Symbol.asyncIterator]();
await storage.write(JSON.stringify(identity));
console.log('created');
await lines.next();
identity.revoked = true;
await storage.write(JSON.stringify(identity));
console.log('replaced');
input.close();
process.stdin.destroy();
"#;

#[tokio::test]
async fn node_production_identity_storage_restricts_acl_on_create_and_replace() {
    if isolated_worker("node_production_identity_storage_restricts_acl_on_create_and_replace").await
    {
        return;
    }
    let root = wide_parent().await;
    let entry =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../packages/dsh-remote-access/lib/index.js");
    assert!(
        entry.is_file(),
        "build the plugin on Windows before running this integration test"
    );
    let home = root.path().join("shadow-dsh");
    let state = root.path().join("state");
    let identity = home
        .join("tuvis-remote")
        .join(INSTANCE)
        .join("identity.json");
    let mut node = ScriptPeer::spawn(
        "node",
        "mjs",
        NODE_IDENTITY_STORAGE,
        &[
            entry.as_os_str(),
            home.as_os_str(),
            state.as_os_str(),
            OsStr::new(INSTANCE),
        ],
    );
    assert_eq!(node.line().await, "created");
    assert_restricted_acl(&acl("read", identity.parent().unwrap()).await, true);
    assert_restricted_acl(&acl("read", &identity).await, false);
    node.command("replace").await;
    assert_eq!(node.line().await, "replaced");
    node.success().await;
    let stored: Value = serde_json::from_slice(&std::fs::read(&identity).unwrap()).unwrap();
    assert_eq!(
        stored["revoked"], true,
        "the production replacement must have happened"
    );
    assert_restricted_acl(&acl("read", &identity).await, false);
}

const REMOTE_CONTROL_SERVER: &str = r#"
param([string]$Name)
$ErrorActionPreference = 'Stop'
$security = [IO.Pipes.PipeSecurity]::new()
$world = [Security.Principal.SecurityIdentifier]::new('S-1-1-0')
$security.AddAccessRule([IO.Pipes.PipeAccessRule]::new($world, 'FullControl', 'Allow'))
$pipe = [IO.Pipes.NamedPipeServerStream]::new($Name, [IO.Pipes.PipeDirection]::InOut, 1, [IO.Pipes.PipeTransmissionMode]::Byte, [IO.Pipes.PipeOptions]::None, 4096, 4096, $security)
try {
  [Console]::WriteLine('ready')
  $pipe.WaitForConnection()
  $reader = [IO.StreamReader]::new($pipe)
  $writer = [IO.StreamWriter]::new($pipe)
  $writer.AutoFlush = $true
  if ($reader.ReadLine() -ne 'control') { throw 'control payload mismatch' }
  $writer.WriteLine('reachable')
} finally { $pipe.Dispose() }
"#;

const REMOTE_CLIENT: &str = r#"
param([string]$Computer, [string]$Server, [string]$Control, [string]$Product, [string]$LocalComputer)
$ErrorActionPreference = 'Stop'
Invoke-Command -ComputerName $Computer -ArgumentList $Server,$Control,$Product,$LocalComputer -ScriptBlock {
  param($Server, $Control, $Product, $LocalComputer)
  $ErrorActionPreference = 'Stop'
  if ($env:COMPUTERNAME -eq $LocalComputer) { throw 'the client must run on a second Windows host' }
  if ($Server -in @('.', 'localhost', '127.0.0.1', '::1', $env:COMPUTERNAME)) { throw 'loopback is not a remote pipe test' }
  $controlPipe = [IO.Pipes.NamedPipeClientStream]::new($Server, $Control, [IO.Pipes.PipeDirection]::InOut)
  try {
    $controlPipe.Connect(5000)
    $writer = [IO.StreamWriter]::new($controlPipe); $writer.AutoFlush = $true
    $reader = [IO.StreamReader]::new($controlPipe)
    $writer.WriteLine('control')
    if ($reader.ReadLine() -ne 'reachable') { throw 'remote positive control failed' }
  } finally { $controlPipe.Dispose() }
  $productPipe = [IO.Pipes.NamedPipeClientStream]::new($Server, $Product, [IO.Pipes.PipeDirection]::InOut)
  $denied = $false
  try { $productPipe.Connect(5000) }
  catch {
    $cause = $_.Exception
    while ($null -ne $cause.InnerException) { $cause = $cause.InnerException }
    if ($cause -is [UnauthorizedAccessException] -or (($cause.HResult -band 65535) -eq 5)) {
      $denied = $true
    } else { throw } # timeout / 不可达绝不算拒绝通过
  } finally { $productPipe.Dispose() }
  @{ control = 'reachable'; denied = $denied } | ConvertTo-Json -Compress
}
"#;

#[tokio::test]
#[ignore = "requires two Windows hosts, working PowerShell Remoting and SMB positive control"]
async fn remote_client_is_denied_only_after_the_same_client_reaches_an_everyone_control() {
    if isolated_worker(
        "remote_client_is_denied_only_after_the_same_client_reaches_an_everyone_control",
    )
    .await
    {
        return;
    }
    // Computer = 第二台 Windows；Server = 它能通过 SMB 访问的本测试机名称。
    // 使用预先配置的 Remoting 身份，不在测试中创建账户/传口令/修改防火墙。
    let computer = std::env::var_os("TUVIS_DSH_TEST_REMOTE_COMPUTER")
        .expect("set TUVIS_DSH_TEST_REMOTE_COMPUTER to a second Windows host");
    let server = std::env::var_os("TUVIS_DSH_TEST_PIPE_SERVER")
        .expect("set TUVIS_DSH_TEST_PIPE_SERVER to this machine's remotely reachable name");
    let local_computer = std::env::var_os("COMPUTERNAME").expect("Windows computer name");
    let pipe = BoundPipe::new().await;
    let control_name = format!(
        "tuvis-h06-control-{}-{:016x}",
        std::process::id(),
        random_u64()
    );
    let mut control = ScriptPeer::spawn(
        "powershell.exe",
        "ps1",
        REMOTE_CONTROL_SERVER,
        &[OsStr::new(&control_name)],
    );
    assert_eq!(control.line().await, "ready");
    let product_name = pipe.address.strip_prefix(r"\\.\pipe\").unwrap();
    let mut remote = ScriptPeer::spawn(
        "powershell.exe",
        "ps1",
        REMOTE_CLIENT,
        &[
            &computer,
            &server,
            OsStr::new(&control_name),
            OsStr::new(product_name),
            &local_computer,
        ],
    );
    // Remoting 握手比本机进程更慢；整个步骤仍设硬期限。
    let observed = timeout(Duration::from_secs(45), remote.output.next_line())
        .await
        .expect("remote environment blocked; timeout is not proof of rejection")
        .unwrap()
        .expect("remote probe failed before producing a result");
    let observed: Value = serde_json::from_str(&observed).unwrap();
    remote.success().await;
    control.success().await;
    assert_eq!(observed["control"], "reachable");
    assert_eq!(
        observed["denied"], true,
        "remote production connection must fail with AccessDenied"
    );
}
