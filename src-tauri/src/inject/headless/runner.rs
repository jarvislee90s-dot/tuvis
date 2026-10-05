// 无头进程生命周期（H4）：并发上限 / watchdog / 取消 / kill 进程树 / 在飞登记。
//
// **两条路径、一套判定**：
// - [`RunnerCfg::run_once`]：**生命周期纯核**——真进程以注入的 `wait` 闭包替代（测试
//   用它模拟超时/取消时序，绝不真 sleep 600s），并发/超时/取消/归一全在这条路上；
// - [`RunnerCfg::run`]：生产异步路径——`tokio::process::Command` spawn + `Stdio::piped`
//   读 stdout 增量喂 [`FrameAccumulator`] + `wait()` 与 watchdog/取消 `select!`。
//   两条路径共用 `finish_*` 归一与 [`GlobalSem`]/[`CancelHandle`]，杜绝双轨判定。
//
// **kill 进程树**（H4：不静默跳过）：
// - Windows：优先 **Job Object 整树收编**（`KILL_ON_JOB_CLOSE` + `TerminateJobObject`
//   热终止——spec 附录 E-⑥ / AionCore 生产用法）；收编失败或非收编进程才落
//   `taskkill /T /F` 保底。附录 E-⑥ 原文的 CREATE_SUSPENDED 前置需自建 CreateProcess
//   拿线程句柄 ResumeThread，`tokio::process::Command` 不透出线程句柄——故本实现取
//   「spawn 后立即收编」的等价形态（收编前的微秒级窗口是已知残余，登记在此不隐瞒）。
// - POSIX：spawn 时 `process_group(0)`，kill 时打整组（`kill -TERM -- -<pid>`）；
//   组信号打空（非组长）→ 降级**单 pid** 信号并在 [`KillOutcome::SinglePid`] 如实标注。
//
// **优雅关闭 / 孤儿自检（H4）**：生产 spawn 即登记在飞表（[`inflight_registry`]），
// MAM 退出时 [`shutdown_inflight`] 逐个终结（lib.rs `RunEvent::Exit` 接线）；Windows
// 侧 Job 句柄随进程关闭触发 `KILL_ON_JOB_CLOSE`，即使退出钩子未跑到也不留孤儿。
//
// **重启后的孤儿自检 = 未实现，登记给 Task 14（E2E 批次）**，本文件只给到「退出即收」
// 这一半。Task 14 需要补齐的最小件（照此做，别另起炉灶）：
// ① **跨进程持久 pid 账本**：spawn 时把 `(tool, session_id, pid, 进程启动时刻)` 落库
//    （退出时删行）——内存里的 [`inflight_registry`] 跨不过进程重启，光靠它查不到上代孤儿；
// ② **启动清扫**：MAM 启动时遍历账本，用 pid + **启动时刻**双判据复核存活（裸 pid 会被
//    系统复用，必须比对进程创建时间；`sysinfo` 可取），命中的走 [`TreeGuard::adopt`] +
//    [`TreeGuard::kill`]（Windows 收编后热终止 / POSIX 组信号）后再删行；
// ③ **身份再验**：清扫前校验进程名/命令行确属本工具，避免误杀复用同一 pid 的无关进程；
// ④ 该清扫需要进程侧查询（`window/win32.rs::collect_ancestor_pids` / sysinfo 同源），
//    放在 Task 14 的 E2E 面，与「连续崩溃 N 次熔断提示」（H4 崩溃段）同批。
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{channel, RecvTimeoutError, Sender};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use super::receipt::{FrameAccumulator, Receipt, ReceiptStatus, Stage};

/// stderr 尾行上限（H4：崩溃回执带 stderr 尾行；只留尾部防内存膨胀）
const STDERR_TAIL_LINES: usize = 8;
/// 退出后等读线程收尾的上限（进程已退，管道 EOF 即到；不无限等）
const READER_DRAIN_TIMEOUT: Duration = Duration::from_secs(2);

// ============================================================
// 全局并发名额（H4：默认 2 可配；**全局** = 同进程所有无头 turn 共用一份）
// ============================================================

#[derive(Default)]
struct SemState {
    in_flight: usize,
    /// 阻塞在 [`GlobalSem::acquire`] 里的请求数（计入队列位置）
    waiting: usize,
}

struct SemInner {
    cap: AtomicUsize,
    state: Mutex<SemState>,
    cv: Condvar,
}

/// 全局并发名额（H4：超额请求**即时排队回执含全局队列位置**——不阻塞死等、不静默丢）
#[derive(Clone)]
pub struct GlobalSem {
    inner: Arc<SemInner>,
}

impl GlobalSem {
    /// 上限至少 1（0 会让所有请求永久排队）
    pub fn new(cap: usize) -> Self {
        Self {
            inner: Arc::new(SemInner {
                cap: AtomicUsize::new(cap.max(1)),
                state: Mutex::new(SemState::default()),
                cv: Condvar::new(),
            }),
        }
    }

    pub fn cap(&self) -> usize {
        self.inner.cap.load(Ordering::SeqCst)
    }

    /// 改上限（H4 配置落点：设置页改键后端点调用；已在飞者不被打断，自然退出即回落）
    pub fn set_cap(&self, cap: usize) {
        self.inner.cap.store(cap.max(1), Ordering::SeqCst);
        self.inner.cv.notify_all();
    }

    pub fn in_flight(&self) -> usize {
        self.lock().in_flight
    }

    /// 有名额即拿，无名额返回 `None`（调用方据此出**即时排队回执**，不在此阻塞）
    pub fn try_acquire(&self) -> Option<SemGuard> {
        let mut st = self.lock();
        if st.in_flight < self.cap() {
            st.in_flight += 1;
            Some(SemGuard { sem: self.clone() })
        } else {
            None
        }
    }

    /// 阻塞取名额（端点侧排队泵用：先把「已排队 + 位次」回执发出去，再等名额放行）
    pub fn acquire(&self) -> SemGuard {
        let mut st = self.lock();
        st.waiting += 1;
        while st.in_flight >= self.cap() {
            st = self.inner.cv.wait(st).unwrap_or_else(|e| e.into_inner());
        }
        st.waiting -= 1;
        st.in_flight += 1;
        drop(st);
        SemGuard { sem: self.clone() }
    }

    /// 全局队列位次（1-based：在飞数 + 阻塞排队数 + 1）——排队回执的位置来源
    pub fn queue_position(&self) -> usize {
        let st = self.lock();
        st.in_flight + st.waiting + 1
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, SemState> {
        self.inner.state.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// 名额守卫（Drop 归还 + 唤醒一个等待者）
pub struct SemGuard {
    sem: GlobalSem,
}

impl Drop for SemGuard {
    fn drop(&mut self) {
        let mut st = self.sem.lock();
        st.in_flight = st.in_flight.saturating_sub(1);
        drop(st);
        self.sem.inner.cv.notify_one();
    }
}

static GLOBAL_SEM: std::sync::LazyLock<GlobalSem> =
    std::sync::LazyLock::new(|| GlobalSem::new(super::DEFAULT_CONCURRENCY));

/// 进程级全局名额句柄（**所有**无头通道必须经它取名额，否则「全局上限」名存实亡）
pub fn global_sem() -> GlobalSem {
    GLOBAL_SEM.clone()
}

// ============================================================
// kill 进程树（附录 E-⑥ / 保底 taskkill / POSIX 进程组）
// ============================================================

/// kill 结果（审计与回执可注明实际用了哪条路径）
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KillOutcome {
    /// Windows Job Object 热终止（整树）
    JobObject,
    /// Windows `taskkill /T /F` 保底
    Taskkill,
    /// POSIX 进程组信号（spawn 侧 `process_group(0)` 契约成立时）
    ProcessGroup,
    /// POSIX 保底：单 pid 信号（子进程不是组长——组信号打空时的降级路径）
    SinglePid,
    /// 测试桩
    TestStub,
    Failed(String),
}

impl KillOutcome {
    pub fn describe(&self) -> String {
        match self {
            KillOutcome::JobObject => "job_object".into(),
            KillOutcome::Taskkill => "taskkill".into(),
            KillOutcome::ProcessGroup => "process_group".into(),
            KillOutcome::SinglePid => "single_pid".into(),
            KillOutcome::TestStub => "test_stub".into(),
            KillOutcome::Failed(e) => format!("failed({e})"),
        }
    }
}

/// kill 出口类型（测试注入记录桩；生产 = [`kill_tree`]/Job Object）
pub type TreeKiller = Arc<dyn Fn(u32) -> KillOutcome + Send + Sync>;

/// 保底杀进程树（无 Job 句柄时的路径；Windows=`taskkill /T /F`，POSIX=进程组）。
///
/// POSIX 两级：先打**整组**（`kill -TERM -- -<pid>`——`--` 是**安全载荷**不是排版，
/// 理由见下方内联注释；spawn 侧 `process_group(0)` 契约成立时命中整棵树）；组信号打空
/// （子进程不是组长，例如调用方漏了 `process_group`，或进程已不在该组）→ **降级为
/// 单 pid 信号**，而不是直接报失败：宁可少杀孙子也不留下已确认的直系子进程当孤儿。
/// 降级在 [`KillOutcome::SinglePid`] 里如实标注。
pub fn kill_tree(pid: u32) -> KillOutcome {
    if pid == 0 {
        return KillOutcome::Failed("pid 未知".into());
    }
    #[cfg(windows)]
    {
        // 不捕获子进程输出（避免 stdio 管道）：只看退出码
        match std::process::Command::new("taskkill")
            .args(["/T", "/F", "/PID", &pid.to_string()])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
        {
            Ok(s) if s.success() => KillOutcome::Taskkill,
            Ok(s) => KillOutcome::Failed(format!("taskkill 退出码 {s}")),
            Err(e) => KillOutcome::Failed(format!("taskkill 启动失败: {e}")),
        }
    }
    #[cfg(not(windows))]
    {
        use std::process::{Command, Stdio};
        // 一级：整组（负号打组——子进程即组长时命中整棵树）。
        // **`--` 不可删（安全载荷，不是排版）**：Ubuntu 24.04 的 procps 4.0.4
        // （2:4.0.4-4ubuntu3.x）会把**裸负 pid 静默截断成 `kill(-1, SIGTERM)`**——
        // 那是「向本用户所有可杀进程发信号」，会连带打死测试进程与 CI agent 自身
        // （Launchpad #2166756，Confirmed 2026-09-08，strace 实证：`kill -0 -1443247`
        // → `kill(-1,0)`；同帖 `kill -TERM -- -120` 只命中目标组）。`--` 终止选项解析
        // 后，负 pid 才以操作数身份到达 kill(2) 的「进程组」语义。任何「简化掉 --」的
        // 改动 = 在 CI 上恢复大规模误杀，**禁止**。
        let group = Command::new("kill")
            .args(["-TERM", "--", &format!("-{pid}")])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        if matches!(&group, Ok(s) if s.success()) {
            return KillOutcome::ProcessGroup;
        }
        // 二级：单 pid 保底（非组长/组已散）
        let single = Command::new("kill")
            .args(["-TERM", &pid.to_string()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        match single {
            Ok(s) if s.success() => KillOutcome::SinglePid,
            Ok(s) => KillOutcome::Failed(format!("kill 组与单 pid 均失败（单 pid 退出码 {s}）")),
            Err(e) => KillOutcome::Failed(format!("kill 启动失败: {e}")),
        }
    }
}

#[cfg(windows)]
mod win_job {
    //! Job Object 整树收编（spec 附录 E-⑥）：`KILL_ON_JOB_CLOSE` 保证句柄关闭
    //! （含 MAM 进程退出）即整树死；`TerminateJobObject` 是热终止路径。
    //! 与附录原文的唯一偏差：不做 CREATE_SUSPENDED 前置（tokio 不透出线程句柄，
    //! 无法 ResumeThread）——改「spawn 后立即收编」，残余窗口见模块文档。
    use core::ffi::c_void;
    use windows::Win32::Foundation::{CloseHandle, HANDLE};
    use windows::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
        SetInformationJobObject, TerminateJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };
    use windows::Win32::System::Threading::{OpenProcess, PROCESS_SET_QUOTA, PROCESS_TERMINATE};

    pub struct WinJob(HANDLE);

    // HANDLE 是内核对象句柄（可跨线程使用），Job 的创建/终止线程无关
    unsafe impl Send for WinJob {}
    unsafe impl Sync for WinJob {}

    /// 把 pid 收编进新 Job（返回 None = 收编失败，调用方落 taskkill 保底）
    pub fn adopt(pid: u32) -> Option<WinJob> {
        if pid == 0 {
            return None;
        }
        unsafe {
            let job = match CreateJobObjectW(None, None) {
                Ok(h) => h,
                Err(e) => {
                    log::warn!("headless: CreateJobObjectW 失败: {e}");
                    return None;
                }
            };
            let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            let rc = SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                &info as *const _ as *const c_void,
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            );
            if let Err(e) = rc {
                log::warn!("headless: SetInformationJobObject(KILL_ON_JOB_CLOSE) 失败: {e}");
                let _ = CloseHandle(job);
                return None;
            }
            let proc = match OpenProcess(PROCESS_SET_QUOTA | PROCESS_TERMINATE, false, pid) {
                Ok(p) => p,
                Err(e) => {
                    log::warn!("headless: OpenProcess({pid}) 失败: {e}");
                    let _ = CloseHandle(job);
                    return None;
                }
            };
            let assigned = AssignProcessToJobObject(job, proc).is_ok();
            let _ = CloseHandle(proc);
            if !assigned {
                log::warn!("headless: AssignProcessToJobObject({pid}) 失败（落 taskkill 保底）");
                let _ = CloseHandle(job);
                return None;
            }
            Some(WinJob(job))
        }
    }

    /// 热终止整树（true = 已下发；false 由调用方落保底）
    pub fn terminate(job: &WinJob) -> bool {
        unsafe { TerminateJobObject(job.0, 1).is_ok() }
    }

    impl Drop for WinJob {
        fn drop(&mut self) {
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }
}

/// 进程树治理句柄：生产 spawn 后**立即** [`TreeGuard::adopt`]，超时/取消经
/// [`TreeGuard::kill`] 整树终结；句柄 Drop（MAM 退出）在 Windows 侧另触发
/// `KILL_ON_JOB_CLOSE` 兜底。
#[derive(Clone)]
pub struct TreeGuard {
    pid: u32,
    #[cfg(windows)]
    job: Option<Arc<win_job::WinJob>>,
}

impl TreeGuard {
    pub fn adopt(pid: u32) -> Self {
        #[cfg(windows)]
        {
            Self {
                pid,
                job: win_job::adopt(pid).map(Arc::new),
            }
        }
        #[cfg(not(windows))]
        {
            Self { pid }
        }
    }

    pub fn pid(&self) -> u32 {
        self.pid
    }

    /// 是否已被整树收编。
    ///
    /// **Windows**：Job Object 句柄在手才算收编（`false` = 落 `taskkill /T /F` 保底）——
    /// 这是能反映真实状态的判据。
    ///
    /// **POSIX**：这里恒 `true`，且**不会掩盖失败**——POSIX 没有「事后收编」这个动作，
    /// 收编发生在 **spawn 时**（`process_group(0)`，见 [`RunnerCfg::run`]）；本句柄只记
    /// pid。组语义若因调用方漏设而失效，[`kill_tree`] 会**降级为单 pid 信号**并在
    /// [`KillOutcome`] 里如实标注 `SinglePid`（不谎报整树已杀、也不留直系孤儿）。
    /// 因此**测试不得以本函数为组语义的证据**，要断言 [`KillOutcome::ProcessGroup`]
    /// （= 组信号真的命中，等价于「spawn 侧契约成立」）。
    pub fn is_adopted(&self) -> bool {
        #[cfg(windows)]
        {
            self.job.is_some()
        }
        #[cfg(not(windows))]
        {
            true
        }
    }

    /// 终结整树：优先热终止，失败落 [`kill_tree`] 保底
    pub fn kill(&self) -> KillOutcome {
        #[cfg(windows)]
        {
            if let Some(job) = &self.job {
                if win_job::terminate(job) {
                    return KillOutcome::JobObject;
                }
                log::warn!("headless: TerminateJobObject 失败，落 taskkill 保底");
            }
        }
        kill_tree(self.pid)
    }
}

/// 在飞回合登记表（H4：MAM 退出时优雅关闭在飞无头进程）
#[derive(Default)]
pub struct InflightRegistry {
    map: Mutex<HashMap<u32, TreeGuard>>,
}

impl InflightRegistry {
    pub fn register(&self, pid: u32, guard: TreeGuard) {
        self.map
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(pid, guard);
    }

    pub fn unregister(&self, pid: u32) {
        self.map
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&pid);
    }

    pub fn len(&self) -> usize {
        self.map.lock().unwrap_or_else(|e| e.into_inner()).len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// 关停全部在飞进程树，返回终结个数（生产出口 = [`shutdown_inflight`]）
    pub fn shutdown_all(&self) -> usize {
        let taken: Vec<TreeGuard> = {
            let mut g = self.map.lock().unwrap_or_else(|e| e.into_inner());
            g.drain().map(|(_, v)| v).collect()
        };
        for guard in &taken {
            guard.kill();
        }
        taken.len()
    }
}

static INFLIGHT: std::sync::LazyLock<InflightRegistry> =
    std::sync::LazyLock::new(InflightRegistry::default);

/// 进程级在飞表（生产 spawn 登记 / 退场注销）
pub fn inflight_registry() -> &'static InflightRegistry {
    &INFLIGHT
}

/// 关停全部在飞无头进程树（H4：MAM 退出钩子调用，返回终结个数）
pub fn shutdown_inflight() -> usize {
    inflight_registry().shutdown_all()
}

// ============================================================
// 取消（H4：与 watchdog 先到者生效，回执注明由谁终止）
// ============================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RunEvent {
    Exited,
    CancelRequested,
    TimedOut,
    /// wait 侧线程异常退出（监视失败）——不得把回合挂在死等里
    WatcherGone,
}

/// 线程 panic 兜底投递（正常路径先解除；异常展开时补发 [`RunEvent::WatcherGone`]）
struct DropSend {
    tx: Option<Sender<RunEvent>>,
    event: RunEvent,
}

impl Drop for DropSend {
    fn drop(&mut self) {
        if let Some(tx) = self.tx.take() {
            let _ = tx.send(self.event);
        }
    }
}

/// 取消投递口：一次一臂（同步路径=mpsc 事件；异步路径=oneshot 唤醒）
enum CancelSink {
    Blocking(Sender<RunEvent>),
    Async(tokio::sync::oneshot::Sender<()>),
}

#[derive(Default)]
struct CancelSlot {
    sink: Mutex<Option<CancelSink>>,
}

/// 移动端取消句柄（`/session-headless-cancel` 消费；句柄可克隆、可跨线程）
#[derive(Clone)]
pub struct CancelHandle {
    slot: Arc<CancelSlot>,
}

impl CancelHandle {
    /// 请求取消在飞回合。`true` = 请求送达（**先到者生效**——同回合重复取消/迟到
    /// 取消一律 `false`，端点据此决定是否落 `headless_cancel` 审计行）。
    pub fn cancel(&self) -> bool {
        let taken = self
            .slot
            .sink
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        match taken {
            Some(CancelSink::Blocking(tx)) => tx.send(RunEvent::CancelRequested).is_ok(),
            Some(CancelSink::Async(tx)) => tx.send(()).is_ok(),
            None => false,
        }
    }
}

// ============================================================
// 回合配置与执行
// ============================================================

/// 进程退出结局（wait 缝/生产 wait 共用）
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ProcessExit {
    /// None = 无退出码（被信号终止）
    pub code: Option<i32>,
    pub stderr_tail: String,
}

/// 无头 turn 运行配置（H4 配置落点：timeout + 并发上限）
pub struct RunnerCfg {
    program: String,
    args: Vec<String>,
    env: Vec<(String, String)>,
    cwd: Option<PathBuf>,
    timeout: Duration,
    sem: GlobalSem,
    cancel: Arc<CancelSlot>,
    killer: TreeKiller,
    /// 本回合的进程树治理句柄（生产 spawn 后置入；纯核/测试缝无真进程 → None）
    tree: Option<TreeGuard>,
    kill_log: Arc<Mutex<Vec<(u32, KillOutcome)>>>,
    scripted_pid: u32,
    scripted_exit: ProcessExit,
    scripted_stdout: Vec<String>,
    session_id: String,
    /// 原始 stdout 行（H6 探针判定与 Task 8/9 的专用回执解析消费——回执只带摘要，
    /// 原始行另有用途，如 codex `Queued message <id>`）
    raw_stdout: Vec<String>,
    /// 原始 stderr 尾行（同上：回执只带截断摘要）
    raw_stderr: String,
    /// 最近一回合的退出码（`None` = 无退出码/未跑）；H6 探针判定读它（评审 Minor 1：
    /// 不得把非零退出谎报成 None）
    last_exit: Option<i32>,
}

impl RunnerCfg {
    /// 生产配置（真 spawn；默认 watchdog = [`super::DEFAULT_TIMEOUT_MS`]、全局名额）
    pub fn new(program: impl Into<String>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            env: Vec::new(),
            cwd: None,
            timeout: Duration::from_millis(super::DEFAULT_TIMEOUT_MS),
            sem: global_sem(),
            cancel: Arc::new(CancelSlot::default()),
            killer: Arc::new(kill_tree),
            tree: None,
            kill_log: Arc::new(Mutex::new(Vec::new())),
            scripted_pid: 0,
            scripted_exit: ProcessExit {
                code: Some(0),
                stderr_tail: String::new(),
            },
            scripted_stdout: Vec::new(),
            session_id: String::new(),
            raw_stdout: Vec::new(),
            raw_stderr: String::new(),
            last_exit: None,
        }
    }

    /// 测试配置：**不真 spawn**——wait 缝提供进程存活期，脚本给退出结局/stdout 帧；
    /// 自持名额（cap=2），与其它用例互不干扰。
    pub fn for_test() -> Self {
        let mut cfg = Self::new("mam-headless-test-stub");
        cfg.sem = GlobalSem::new(super::DEFAULT_CONCURRENCY);
        cfg.scripted_pid = 4242;
        cfg.killer = Arc::new(|_| KillOutcome::TestStub);
        cfg
    }

    pub fn arg(mut self, a: impl Into<String>) -> Self {
        self.args.push(a.into());
        self
    }

    pub fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.args.extend(args.into_iter().map(Into::into));
        self
    }

    pub fn env(mut self, k: impl Into<String>, v: impl Into<String>) -> Self {
        self.env.push((k.into(), v.into()));
        self
    }

    pub fn cwd(mut self, dir: impl Into<PathBuf>) -> Self {
        self.cwd = Some(dir.into());
        self
    }

    pub fn session_id(mut self, sid: impl Into<String>) -> Self {
        self.session_id = sid.into();
        self
    }

    pub fn timeout_ms(mut self, ms: u64) -> Self {
        self.timeout = Duration::from_millis(ms);
        self
    }

    pub fn sem(mut self, sem: GlobalSem) -> Self {
        self.sem = sem;
        self
    }

    /// 脚本退出码（测试缝：wait 闭包只模拟存活期，结局由配置给）
    pub fn exit_code(mut self, code: i32) -> Self {
        self.scripted_exit.code = Some(code);
        self
    }

    pub fn stderr_tail(mut self, tail: impl Into<String>) -> Self {
        self.scripted_exit.stderr_tail = tail.into();
        self
    }

    pub fn stdout_lines(mut self, lines: Vec<String>) -> Self {
        self.scripted_stdout = lines;
        self
    }

    pub fn scripted_pid(mut self, pid: u32) -> Self {
        self.scripted_pid = pid;
        self
    }

    /// H4 配置落点：设置页读出的超时 + 并发上限落到本回合（并刷新全局名额上限）
    pub fn apply_limits(&mut self, limits: super::HeadlessLimits) {
        self.timeout = Duration::from_millis(limits.timeout_ms);
        self.sem.set_cap(limits.concurrency);
    }

    pub fn cancel_handle(&self) -> CancelHandle {
        CancelHandle {
            slot: self.cancel.clone(),
        }
    }

    /// **长驻变体的取消观察口**（Task 13 / H11 claude：进程存活至 turn 结束，没有 `run()`
    /// 的 wait 事件环）。语义与 [`Self::run`] 内的取消**同一套**（同一个 [`CancelSlot`] 单点、
    /// 先到者生效）——武装后返回（句柄, 接收端）：句柄交给 [`super::turn::registry`] 当取消靶子，
    /// 接收端由长驻回合 `select!` 等待；回合终结时调用 [`Self::disarm_cancel`]，此后迟到的
    /// 取消与既有通道同口径地**如实报「未送达」**（不谎报已取消）。
    ///
    /// **为什么必须在此加口**（而不是让 claude 自建一套）：取消的「先到者生效」与
    /// watchdog 的关系是 H4 的既有语义，第二套实现必然漂移；本方法是**唯一**让外部回合
    /// 接到该语义的公开面（`arm`/`disarm` 仍是私有）。
    pub fn arm_cancel(&self) -> (CancelHandle, tokio::sync::oneshot::Receiver<()>) {
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        self.arm(CancelSink::Async(tx));
        (self.cancel_handle(), rx)
    }

    /// 解除取消武装（长驻回合收尾；此后 `CancelHandle::cancel()` 恒 `false` = 未送达）
    pub fn disarm_cancel(&self) {
        self.disarm();
    }

    /// 本回合的 kill 记录（pid, 路径）——测试断言「watchdog/取消必须 kill」用
    pub fn kill_calls(&self) -> Vec<(u32, KillOutcome)> {
        self.kill_log
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// 原始 stdout 行（最近一回合；H6 探针判定 / Task 8 的专用回执解析消费）
    pub fn captured_stdout(&self) -> Vec<String> {
        self.raw_stdout.clone()
    }

    /// 原始 stderr 尾行（最近一回合）
    pub fn captured_stderr(&self) -> String {
        self.raw_stderr.clone()
    }

    /// 本回合 watchdog 超时（毫秒）——「runner 启动时读取」的可观测面
    /// （构建器叫 [`RunnerCfg::timeout_ms`]，故取值口另起名避免同名冲突）
    pub fn watchdog_timeout_ms(&self) -> u64 {
        self.timeout.as_millis() as u64
    }

    /// 最近一回合的退出码（`None` = 无退出码/未跑）——探针判定与回执解析消费
    pub fn last_exit_code(&self) -> Option<i32> {
        self.last_exit
    }

    fn kill(&self, pid: u32) -> KillOutcome {
        // 生产路径有 Job 句柄 → 走整树热终止；测试缝/无句柄 → 注入的 killer
        let outcome = match &self.tree {
            Some(guard) => guard.kill(),
            None => (self.killer)(pid),
        };
        self.kill_log
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push((pid, outcome.clone()));
        outcome
    }

    fn arm(&self, sink: CancelSink) {
        *self.cancel.sink.lock().unwrap_or_else(|e| e.into_inner()) = Some(sink);
    }

    fn disarm(&self) {
        *self.cancel.sink.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }

    /// **生命周期纯核**（同步；wait 闭包替代真进程）——并发/超时/取消/归一全在此。
    ///
    /// `wait(pid)` 模拟子进程存活期（真超时/取消由主线程侧判定，闭包被 detach，
    /// 不 join——测试里它可能还在睡，生产路径不用本函数）。
    pub fn run_once<W>(&mut self, wait: W) -> Receipt
    where
        W: FnOnce(u32) + Send + 'static,
    {
        let started = Instant::now();
        let Some(_slot) = self.sem.try_acquire() else {
            return Receipt::queued(self.sem.queue_position()).with_session(&self.session_id);
        };
        let mut acc = FrameAccumulator::default();
        for line in &self.scripted_stdout {
            acc.push_line(line);
        }
        self.raw_stdout = self.scripted_stdout.clone();
        self.raw_stderr = self.scripted_exit.stderr_tail.clone();
        self.last_exit = self.scripted_exit.code;
        let (tx, rx) = channel::<RunEvent>();
        self.arm(CancelSink::Blocking(tx.clone()));
        let pid = self.scripted_pid;
        let watcher = std::thread::spawn(move || {
            // wait 侧 panic 也必须让主线程立刻知道（否则取消槽里的 sender 会让
            // recv_timeout 一路等到 watchdog 到点——把「监视挂了」误报成超时）
            let mut on_unwind = DropSend {
                tx: Some(tx.clone()),
                event: RunEvent::WatcherGone,
            };
            wait(pid);
            on_unwind.tx = None;
            let _ = tx.send(RunEvent::Exited);
        });
        let event = {
            let remaining = self.timeout.saturating_sub(started.elapsed());
            match rx.recv_timeout(remaining) {
                Ok(e) => e,
                Err(RecvTimeoutError::Timeout) => RunEvent::TimedOut,
                Err(RecvTimeoutError::Disconnected) => RunEvent::WatcherGone,
            }
        };
        self.disarm();
        // wait 侧线程不 join：超时/取消时它可能还在睡（生产路径不用本函数）
        drop(watcher);
        let duration_ms = started.elapsed().as_millis() as u64;
        match event {
            RunEvent::Exited => {
                finish_exit(&acc, &self.session_id, &self.scripted_exit, duration_ms)
            }
            RunEvent::CancelRequested => {
                let killed = self.kill(pid);
                Receipt::cancelled(&format!(
                    "已取消（移动端请求，先到者生效）；kill 进程树 = {}",
                    killed.describe()
                ))
                .with_session(&self.session_id)
                .with_duration_ms(duration_ms)
            }
            RunEvent::TimedOut => {
                let killed = self.kill(pid);
                Receipt::failed(
                    Stage::Timeout,
                    &format!(
                        "watchdog {}s 到点，已 kill 进程树 = {}（可重试）",
                        self.timeout.as_secs(),
                        killed.describe()
                    ),
                )
                .with_session(&self.session_id)
                .with_duration_ms(duration_ms)
            }
            RunEvent::WatcherGone => {
                let killed = self.kill(pid);
                Receipt::failed(
                    Stage::Crash,
                    &format!(
                        "进程监视失败（wait 侧异常退出）；kill 进程树 = {}",
                        killed.describe()
                    ),
                )
                .with_session(&self.session_id)
                .with_duration_ms(duration_ms)
            }
        }
    }

    /// **生产异步路径**：真 spawn + piped stdout 增量解析 + watchdog/取消 `select!`
    pub async fn run(&mut self) -> Receipt {
        let started = Instant::now();
        let Some(_slot) = self.sem.try_acquire() else {
            return Receipt::queued(self.sem.queue_position()).with_session(&self.session_id);
        };
        let mut cmd = tokio::process::Command::new(&self.program);
        cmd.args(&self.args);
        for (k, v) in &self.env {
            cmd.env(k, v);
        }
        if let Some(cwd) = &self.cwd {
            cmd.current_dir(cwd);
        }
        // stdin 关闭（无头 turn 不做双向审批——claude 的双向 control 归 Task 13）
        cmd.stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        // POSIX：自成进程组 → 杀树可打整组（H4）
        #[cfg(unix)]
        cmd.process_group(0);
        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                return Receipt::failed(Stage::Spawn, &format!("spawn 失败: {e}"))
                    .with_session(&self.session_id)
            }
        };
        let pid = child.id().unwrap_or(0);
        let guard = TreeGuard::adopt(pid);
        self.tree = Some(guard.clone());
        inflight_registry().register(pid, guard.clone());

        let acc = Arc::new(Mutex::new(FrameAccumulator::default()));
        let tail = Arc::new(Mutex::new(String::new()));
        let raw_lines = Arc::new(Mutex::new(Vec::<String>::new()));
        let stdout_task = child.stdout.take().map(|out| {
            let acc = acc.clone();
            let raw = raw_lines.clone();
            tokio::spawn(async move {
                use tokio::io::AsyncBufReadExt;
                let mut lines = tokio::io::BufReader::new(out).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    acc.lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .push_line(&line);
                    raw.lock().unwrap_or_else(|e| e.into_inner()).push(line);
                }
            })
        });
        let stderr_task = child.stderr.take().map(|err| {
            let tail = tail.clone();
            tokio::spawn(async move {
                use tokio::io::AsyncBufReadExt;
                let mut lines = tokio::io::BufReader::new(err).lines();
                let mut buf: std::collections::VecDeque<String> = std::collections::VecDeque::new();
                while let Ok(Some(line)) = lines.next_line().await {
                    if buf.len() == STDERR_TAIL_LINES {
                        buf.pop_front();
                    }
                    buf.push_back(line);
                }
                *tail.lock().unwrap_or_else(|e| e.into_inner()) =
                    buf.into_iter().collect::<Vec<_>>().join("\n");
            })
        });

        let (cancel_tx, mut cancel_rx) = tokio::sync::oneshot::channel::<()>();
        self.arm(CancelSink::Async(cancel_tx));
        let timeout = self.timeout;
        let mut status: Option<std::process::ExitStatus> = None;
        let event = tokio::select! {
            st = child.wait() => {
                match st {
                    Ok(s) => { status = Some(s); RunEvent::Exited }
                    Err(e) => { log::warn!("headless: wait 失败: {e}"); RunEvent::WatcherGone }
                }
            }
            _ = tokio::time::sleep(timeout) => RunEvent::TimedOut,
            _ = &mut cancel_rx => RunEvent::CancelRequested,
        };
        self.disarm();
        // 只有自然退出不需要动进程树；监视异常（wait 失败）时进程可能仍在跑 → 照杀
        let killed = if matches!(event, RunEvent::Exited) {
            None
        } else {
            Some(self.kill(pid))
        };
        self.tree = None;
        // 收尾读线程（进程/树已终结 → 管道 EOF；有上限不无限等）
        for task in [stdout_task, stderr_task].into_iter().flatten() {
            let _ = tokio::time::timeout(READER_DRAIN_TIMEOUT, task).await;
        }
        inflight_registry().unregister(pid);
        let duration_ms = started.elapsed().as_millis() as u64;
        let stderr_tail = tail.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let acc = acc.lock().unwrap_or_else(|e| e.into_inner()).clone();
        self.raw_stdout = raw_lines.lock().unwrap_or_else(|e| e.into_inner()).clone();
        self.raw_stderr = stderr_tail.clone();
        self.last_exit = status.and_then(|s| s.code());
        match event {
            RunEvent::Exited => {
                let exit = ProcessExit {
                    code: status.and_then(|s| s.code()),
                    stderr_tail,
                };
                finish_exit(&acc, &self.session_id, &exit, duration_ms)
            }
            RunEvent::CancelRequested => Receipt::cancelled(&format!(
                "已取消（移动端请求，先到者生效）；kill 进程树 = {}",
                killed.map(|k| k.describe()).unwrap_or_else(|| "-".into())
            ))
            .with_session(&self.session_id)
            .with_duration_ms(duration_ms),
            RunEvent::TimedOut => Receipt::failed(
                Stage::Timeout,
                &format!(
                    "watchdog {}s 到点，已 kill 进程树 = {}（可重试）",
                    timeout.as_secs(),
                    killed.map(|k| k.describe()).unwrap_or_else(|| "-".into())
                ),
            )
            .with_session(&self.session_id)
            .with_duration_ms(duration_ms),
            RunEvent::WatcherGone => Receipt::failed(
                Stage::Crash,
                &format!(
                    "进程监视失败；kill 进程树 = {}",
                    killed.map(|k| k.describe()).unwrap_or_else(|| "-".into())
                ),
            )
            .with_session(&self.session_id)
            .with_duration_ms(duration_ms),
        }
    }
}

/// 自然退出归一：0 → Ok；非 0 → Crash（退出码 + stderr 尾行，H4：不自动重试）
fn finish_exit(
    acc: &FrameAccumulator,
    session_id: &str,
    exit: &ProcessExit,
    duration_ms: u64,
) -> Receipt {
    match exit.code {
        Some(0) => acc.receipt(session_id, ReceiptStatus::Ok, None, duration_ms),
        Some(code) => {
            let mut r = acc.receipt(
                session_id,
                ReceiptStatus::Failed,
                Some(Stage::Crash),
                duration_ms,
            );
            r.reason = Some(format!("退出码 {code}{}", stderr_suffix(&exit.stderr_tail)));
            r
        }
        None => {
            let mut r = acc.receipt(
                session_id,
                ReceiptStatus::Failed,
                Some(Stage::Crash),
                duration_ms,
            );
            r.reason = Some(format!(
                "进程被信号终止（无退出码）{}",
                stderr_suffix(&exit.stderr_tail)
            ));
            r
        }
    }
}

fn stderr_suffix(tail: &str) -> String {
    let t = tail.trim();
    if t.is_empty() {
        String::new()
    } else {
        format!(
            "；stderr 尾行：{}",
            crate::inject::normalize::summarize(t, crate::inject::normalize::AUDIT_SUMMARY_CHARS)
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inject::headless::receipt::{ReceiptStatus, Stage, Terminator};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering as AtomicOrdering};

    /// 计划书 Step 2 原例：watchdog 到点（注入 wait 闭包模拟超时，**不真 sleep 600s**）
    /// 必须 kill 进程树。
    #[test]
    fn watchdog_fires_and_kills_tree_stub() {
        let mut r = RunnerCfg::for_test().timeout_ms(50);
        let out = r.run_once(|_| std::thread::sleep(std::time::Duration::from_millis(500)));
        assert!(matches!(out.stage, Some(Stage::Timeout)));
        assert_eq!(out.status, ReceiptStatus::Failed);
        assert_eq!(
            out.terminator(),
            Terminator::Watchdog,
            "回执须注明由 watchdog 终止"
        );
        assert_eq!(r.kill_calls().len(), 1, "watchdog 到点必须 kill 进程树");
    }

    /// 计划书 Step 2 原例：全局上限 2（H4 默认 2 可配），第三个必须排队——
    /// 不得静默丢弃、不得阻塞死等。
    #[test]
    fn concurrency_cap_queues_second_turn_globally() {
        let sem = GlobalSem::new(2);
        let _a = sem.acquire();
        let _b = sem.acquire();
        assert!(sem.try_acquire().is_none(), "第三个应排队");
        // 全局语义：同一个 sem 的克隆句柄共享名额（跨调用方）
        let clone = sem.clone();
        assert!(clone.try_acquire().is_none(), "克隆句柄必须共享同一份名额");
        drop(_a);
        let c = sem.try_acquire();
        assert!(c.is_some(), "有名额释放即放行");
        drop(_b);
        drop(c);
        assert_eq!(sem.in_flight(), 0);
    }

    /// 超额请求：**即时**排队回执 + 全局队列位置（H4），不 spawn、不阻塞
    #[test]
    fn over_cap_turn_returns_queued_receipt_with_position() {
        let sem = GlobalSem::new(2);
        let hold_a = sem.acquire();
        let hold_b = sem.acquire();
        let ran = Arc::new(AtomicBool::new(false));
        let ran_in_wait = ran.clone();
        let mut r = RunnerCfg::for_test().sem(sem).timeout_ms(5_000);
        let out = r.run_once(move |_| {
            ran_in_wait.store(true, AtomicOrdering::SeqCst);
        });
        assert_eq!(out.status, ReceiptStatus::Queued);
        assert_eq!(out.terminator(), Terminator::NotStarted);
        assert_eq!(
            out.queue_position(),
            Some(3),
            "2 个在飞 → 本请求全局第 3 位"
        );
        assert!(
            out.reason.as_deref().unwrap_or("").contains('3'),
            "排队回执必须带出队列位置: {out:?}"
        );
        assert!(!ran.load(AtomicOrdering::SeqCst), "排队请求不得起跑");
        assert!(r.kill_calls().is_empty(), "排队请求不得 kill 任何进程");
        drop(hold_a);
        drop(hold_b);
    }

    /// 取消先到 → Cancelled（终止方 = 取消）；且 kill 进程树恰好一次
    #[test]
    fn cancellation_wins_over_watchdog_and_names_terminator() {
        let mut r = RunnerCfg::for_test().timeout_ms(2_000);
        let handle = r.cancel_handle();
        let fired = Arc::new(AtomicBool::new(false));
        let fired2 = fired.clone();
        let t = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(20));
            fired2.store(handle.cancel(), AtomicOrdering::SeqCst);
        });
        let out = r.run_once(|_| std::thread::sleep(std::time::Duration::from_millis(1_500)));
        t.join().unwrap();
        assert!(fired.load(AtomicOrdering::SeqCst), "在飞回合的取消必须生效");
        assert_eq!(out.status, ReceiptStatus::Cancelled);
        assert_eq!(out.terminator(), Terminator::Cancel, "回执须注明由取消终止");
        assert_eq!(out.stage, None, "取消不是失败阶段");
        assert!(out.reason.as_deref().unwrap_or("").contains("取消"));
        assert_eq!(r.kill_calls().len(), 1, "取消必须 kill 进程树");
    }

    /// watchdog 先到 → Timeout（终止方 = watchdog）：迟到的取消不生效、不落第二次 kill
    #[test]
    fn watchdog_wins_when_cancel_arrives_late() {
        let mut r = RunnerCfg::for_test().timeout_ms(120);
        let handle = r.cancel_handle();
        let late = Arc::new(AtomicBool::new(true));
        let late2 = late.clone();
        let t = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(600));
            late2.store(handle.cancel(), AtomicOrdering::SeqCst);
        });
        let out = r.run_once(|_| std::thread::sleep(std::time::Duration::from_millis(1_500)));
        t.join().unwrap();
        assert!(matches!(out.stage, Some(Stage::Timeout)));
        assert_eq!(out.terminator(), Terminator::Watchdog);
        assert!(
            !late.load(AtomicOrdering::SeqCst),
            "回合已终结，迟到取消不生效"
        );
        assert_eq!(r.kill_calls().len(), 1, "先到者生效——只 kill 一次");
    }

    /// 回合自然结束后取消无效（返回 false，不落审计）
    #[test]
    fn cancel_after_finish_is_not_effective() {
        let mut r = RunnerCfg::for_test();
        let handle = r.cancel_handle();
        let out = r.run_once(|_| {});
        assert_eq!(out.status, ReceiptStatus::Ok);
        assert!(!handle.cancel(), "无在飞回合时取消必须报 false");
        assert!(r.kill_calls().is_empty());
    }

    /// 非零退出 → Crash + 退出码 + stderr 尾行（H4：不自动重试，交回执卡）
    #[test]
    fn nonzero_exit_maps_to_crash_with_code_and_stderr_tail() {
        let mut r = RunnerCfg::for_test()
            .exit_code(3)
            .stderr_tail("Model creation failed: 工作区忙");
        let out = r.run_once(|_| {});
        assert_eq!(out.status, ReceiptStatus::Failed);
        assert!(matches!(out.stage, Some(Stage::Crash)));
        let reason = out.reason.clone().unwrap_or_default();
        assert!(reason.contains('3'), "退出码必须在回执里: {reason}");
        assert!(
            reason.contains("工作区忙"),
            "stderr 尾行必须在回执里: {reason}"
        );
        assert_eq!(out.terminator(), Terminator::Exit);
        assert!(r.kill_calls().is_empty(), "自然退出不需要 kill");
    }

    /// 无退出码（被信号终止）→ Crash 且原因如实（不谎报退出码）
    #[test]
    fn missing_exit_code_is_reported_honestly() {
        let mut r = RunnerCfg::for_test();
        r.scripted_exit.code = None;
        let out = r.run_once(|_| {});
        assert!(matches!(out.stage, Some(Stage::Crash)));
        assert!(out.reason.unwrap_or_default().contains("无退出码"));
    }

    /// 自然退出 0：stdout 帧（含 Mac 实测的噪音前缀行）归一进回执
    #[test]
    fn zero_exit_normalizes_streamed_frames() {
        let mut r = RunnerCfg::for_test()
            .session_id("sess_fallback")
            .stdout_lines(vec![
                "ZCode Built-in skipped (not-due)".to_string(),
                "{\"sessionId\":\"s1\",\"response\":\"hi\",\"tokens\":9}".to_string(),
            ]);
        let out = r.run_once(|_| {});
        assert_eq!(out.status, ReceiptStatus::Ok);
        assert_eq!(out.session_id, "s1", "帧里的会话号覆盖回填值");
        assert_eq!(out.last_assistant.as_deref(), Some("hi"));
        assert_eq!(out.tokens, Some(9));
        assert!(out.stage.is_none() && out.reason.is_none());
    }

    /// wait 缝线程 panic（进程监视异常）→ Crash，不得把回合挂在死等里
    #[test]
    fn waiter_panic_maps_to_crash() {
        let mut r = RunnerCfg::for_test().timeout_ms(3_000);
        let out = r.run_once(|_| panic!("模拟监视失败"));
        assert!(matches!(out.stage, Some(Stage::Crash)));
    }

    /// 真 spawn 缝（hazard F）：短命无害子进程走 `Stdio::piped` 读 stdout，JSON 行经
    /// 前缀跳过解析归一回执。**这是生产异步路径 [`RunnerCfg::run`] 的缝测**——子进程 =
    /// **本测试二进制自身**（`--ignored --exact` 只拉起下面的打印桩）：零 shell、零引号
    /// 歧义（Windows `Command` 会把带引号的实参转义成 `\"`，`cmd /c echo` 出不了干净
    /// JSON）、零临时文件，且真实产出「harness 噪音行 + JSON 行」以验证前缀跳过。
    #[tokio::test]
    async fn real_spawn_pipes_stdout_through_prefix_skip() {
        let exe = std::env::current_exe().expect("测试二进制路径");
        let mut r = RunnerCfg::new(exe.to_string_lossy().to_string())
            .args([
                "--exact",
                "inject::headless::runner::tests::stdout_helper_prints_noise_then_json",
                "--ignored",
                "--nocapture",
            ])
            .session_id("sess_real")
            .timeout_ms(60_000)
            // 自持名额：真 spawn 用例与其它用例并行跑，不抢全局名额（避免互相排队）
            .sem(GlobalSem::new(4));
        let out = r.run().await;
        assert_eq!(out.status, ReceiptStatus::Ok, "{out:?}");
        assert_eq!(
            out.session_id, "s-real",
            "真实管道上的 JSON 必须经前缀跳过解出"
        );
        assert_eq!(out.last_assistant.as_deref(), Some("pong"));
    }

    /// 上面缝测的**子进程桩**：打印噪音前缀行 + JSON 帧后正常退出。
    /// `#[ignore]` = 正常套件不跑；只由缝测以 `--ignored --exact` 拉起。
    #[test]
    #[ignore]
    fn stdout_helper_prints_noise_then_json() {
        println!("ZCode Built-in skipped (not-due)");
        println!("{{\"sessionId\":\"s-real\",\"response\":\"pong\"}}");
    }

    /// 真 spawn：非零退出（无害命令）→ Crash + 退出码
    #[tokio::test]
    async fn real_spawn_nonzero_exit_maps_to_crash() {
        #[cfg(windows)]
        let r = RunnerCfg::new("cmd").args(["/c", "exit 7"]);
        #[cfg(not(windows))]
        let r = RunnerCfg::new("sh").args(["-c", "exit 7"]);
        let r = r.timeout_ms(30_000);
        let mut r = r.sem(GlobalSem::new(4)); // 自持名额（同上）
        let out = r.run().await;
        assert_eq!(out.status, ReceiptStatus::Failed, "{out:?}");
        assert!(matches!(out.stage, Some(Stage::Crash)));
        assert!(out.reason.unwrap_or_default().contains('7'));
    }

    /// 真 spawn：watchdog 到点 → 真 kill 进程树（进程必须消失，无孤儿）
    #[tokio::test]
    async fn real_spawn_watchdog_kills_long_running_process() {
        let mut r = long_child_cfg().timeout_ms(200);
        let out = r.run().await;
        assert!(matches!(out.stage, Some(Stage::Timeout)), "{out:?}");
        let calls = r.kill_calls();
        assert_eq!(calls.len(), 1, "超时必须 kill 树");
        assert!(!matches!(calls[0].1, KillOutcome::Failed(_)), "{calls:?}");
    }

    /// 评审 Minor 2：**生产**异步路径的取消竞速（注入时序：300ms 取消 vs 30s watchdog）
    /// ——取消先到必须 Cancelled（终止方 = 取消）+ 恰好一次 kill（不是等到 watchdog）
    #[tokio::test]
    async fn real_spawn_cancel_wins_over_watchdog() {
        let mut r = long_child_cfg().timeout_ms(30_000);
        let handle = r.cancel_handle();
        let canceller = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
            handle.cancel()
        });
        let out = r.run().await;
        let fired = canceller.await.unwrap();
        assert!(fired, "在飞回合的取消必须生效（先到者）");
        assert_eq!(out.status, ReceiptStatus::Cancelled, "{out:?}");
        assert_eq!(out.terminator(), Terminator::Cancel, "回执须注明由取消终止");
        assert_eq!(r.kill_calls().len(), 1, "取消必须 kill 进程树");
    }

    /// 评审 Minor 1：退出码必须能从回合配置读出（H6 探针判定读它——不得把非零退出
    /// 谎报成 None）
    #[tokio::test]
    async fn real_spawn_exposes_exit_code() {
        #[cfg(windows)]
        let bad = RunnerCfg::new("cmd").args(["/c", "exit", "7"]);
        #[cfg(not(windows))]
        let bad = RunnerCfg::new("sh").args(["-c", "exit 7"]);
        let mut bad = bad.timeout_ms(30_000).sem(GlobalSem::new(4));
        let out = bad.run().await;
        assert!(matches!(out.stage, Some(Stage::Crash)), "{out:?}");
        assert_eq!(
            bad.last_exit_code(),
            Some(7),
            "退出码必须透出（探针提示不得谎报 None）"
        );
        #[cfg(windows)]
        let ok = RunnerCfg::new("cmd").args(["/c", "exit", "0"]);
        #[cfg(not(windows))]
        let ok = RunnerCfg::new("sh").args(["-c", "exit 0"]);
        let mut ok = ok.timeout_ms(30_000).sem(GlobalSem::new(4));
        assert_eq!(ok.run().await.status, ReceiptStatus::Ok);
        assert_eq!(ok.last_exit_code(), Some(0));
        // 无退出码（监视失败/信号）如实为 None，不编 0
        let mut scripted = RunnerCfg::for_test();
        scripted.scripted_exit.code = None;
        let _ = scripted.run_once(|_| {});
        assert_eq!(scripted.last_exit_code(), None);
    }

    /// spawn 失败（不存在的可执行文件）→ Stage::Spawn（不是 Crash/ChannelError）
    #[tokio::test]
    async fn spawn_failure_maps_to_spawn_stage() {
        let mut r = RunnerCfg::new("mam-nonexistent-binary-xyz").timeout_ms(5_000);
        let out = r.run().await;
        assert!(matches!(out.stage, Some(Stage::Spawn)), "{out:?}");
        assert_eq!(out.status, ReceiptStatus::Failed);
    }

    /// 并发上限可配（H4 配置落点）：set_cap 后立即生效；clamp 下界 1
    #[test]
    fn sem_cap_is_configurable_and_clamped() {
        let sem = GlobalSem::new(0);
        assert_eq!(sem.cap(), 1, "上限至少 1（0 会让所有请求永久排队）");
        sem.set_cap(3);
        assert_eq!(sem.cap(), 3);
        let _a = sem.acquire();
        let _b = sem.acquire();
        let _c = sem.acquire();
        assert!(sem.try_acquire().is_none());
    }

    /// 测试用长命真子进程（~30s）：**POSIX 侧与生产同形**——`process_group(0)` 自成
    /// 进程组（生产 [`RunnerCfg::run`] 对 unix 就这么做），组信号才有靶子
    fn spawn_test_child() -> std::process::Child {
        #[cfg(windows)]
        {
            std::process::Command::new("cmd")
                .args(["/c", "ping -n 30 127.0.0.1"])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .expect("起一个 30s 的真子进程")
        }
        #[cfg(not(windows))]
        {
            use std::os::unix::process::CommandExt;
            let mut cmd = std::process::Command::new("sleep");
            cmd.arg("30")
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null());
            cmd.process_group(0);
            cmd.spawn().expect("起一个 30s 的真子进程")
        }
    }

    /// 测试用长命回合配置（真 spawn；取消/超时竞速用）
    fn long_child_cfg() -> RunnerCfg {
        #[cfg(windows)]
        let r = RunnerCfg::new("cmd").args(["/c", "ping -n 30 127.0.0.1"]);
        #[cfg(not(windows))]
        let r = RunnerCfg::new("sleep").args(["30"]);
        r.sem(GlobalSem::new(4)) // 自持名额：真 spawn 用例并行跑，不抢全局名额
    }

    /// 真 kill 树（H4 底线）：收编真子进程（Windows Job Object / POSIX 进程组），
    /// kill 后进程必须消失——**不静默跳过进程树要求**。
    /// POSIX 分支断言 `ProcessGroup`（= 组信号真的命中，证明 spawn 侧 `process_group(0)`
    /// 契约成立）而不是相信 `is_adopted()` 的自述——CI 宿主是 ubuntu-latest，这条必须真过。
    #[test]
    fn tree_guard_kills_a_real_child_process() {
        let mut child = spawn_test_child();
        let pid = child.id();
        let guard = TreeGuard::adopt(pid);
        #[cfg(windows)]
        assert!(
            guard.is_adopted(),
            "Windows 必须被 Job Object 收编（附录 E-⑥）"
        );
        let outcome = guard.kill();
        #[cfg(windows)]
        assert!(
            matches!(outcome, KillOutcome::JobObject | KillOutcome::Taskkill),
            "kill 树不得失败: {outcome:?}"
        );
        #[cfg(not(windows))]
        assert_eq!(
            outcome,
            KillOutcome::ProcessGroup,
            "组信号必须命中（spawn 侧 process_group(0) 契约）: {outcome:?}"
        );
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut gone = false;
        while std::time::Instant::now() < deadline {
            if matches!(child.try_wait(), Ok(Some(_))) {
                gone = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        let _ = child.wait(); // 收尸（clippy::zombie_processes：所有路径都要 wait）
        assert!(gone, "kill 后子进程必须消失（不得留孤儿）");
    }

    /// POSIX 保底（评审 Important 1）：**没设 `process_group(0)`** 的子进程不是组长 →
    /// 组信号 ESRCH → 必须降级为**单 pid kill**，而不是直接报失败（不得把「忘了收编」
    /// 谎报成「杀树失败」，也不得留下孤儿）
    #[cfg(unix)]
    #[test]
    fn non_leader_child_falls_back_to_single_pid_kill() {
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("起一个真子进程（刻意不设 process_group）");
        let pid = child.id();
        let outcome = kill_tree(pid);
        assert_eq!(
            outcome,
            KillOutcome::SinglePid,
            "组信号对非组长必然失败——必须保底单 pid kill: {outcome:?}"
        );
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut gone = false;
        while std::time::Instant::now() < deadline {
            if matches!(child.try_wait(), Ok(Some(_))) {
                gone = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        let _ = child.wait();
        assert!(gone, "保底杀必须真的终止进程（不得留孤儿）");
    }

    /// 在飞登记表（H4 优雅关闭）：登记 → 关停 → 槽位清空
    #[test]
    fn inflight_registry_shutdown_clears_slots() {
        let reg = InflightRegistry::default();
        let mut child = spawn_test_child();
        let pid = child.id();
        reg.register(pid, TreeGuard::adopt(pid));
        assert_eq!(reg.len(), 1);
        assert!(!reg.is_empty());
        assert_eq!(reg.shutdown_all(), 1, "关停必须逐个终结在飞进程树");
        assert_eq!(reg.len(), 0);
        let _ = child.kill();
        let _ = child.wait(); // 收尸（clippy::zombie_processes）
    }

    /// 计数口径：in_flight 在自然退出/超时/取消三条路径上都必须归零
    /// （否则并发名额泄漏 → 后续请求永久排队）
    #[test]
    fn slots_are_released_on_every_path() {
        let sem = GlobalSem::new(2);
        let mut ok = RunnerCfg::for_test().sem(sem.clone());
        assert_eq!(ok.run_once(|_| {}).status, ReceiptStatus::Ok);
        let mut timeout = RunnerCfg::for_test().sem(sem.clone()).timeout_ms(30);
        assert!(matches!(
            timeout
                .run_once(|_| std::thread::sleep(std::time::Duration::from_millis(300)))
                .stage,
            Some(Stage::Timeout)
        ));
        let mut cancelled = RunnerCfg::for_test().sem(sem.clone()).timeout_ms(2_000);
        let h = cancelled.cancel_handle();
        let t = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(20));
            h.cancel()
        });
        assert_eq!(
            cancelled
                .run_once(|_| std::thread::sleep(std::time::Duration::from_millis(1_000)))
                .status,
            ReceiptStatus::Cancelled
        );
        t.join().unwrap();
        assert_eq!(sem.in_flight(), 0, "三条终结路径都必须归还名额");
    }

    /// wait 缝的入参 pid：真路径传 spawn 的 pid，脚本路径传脚本 pid（kill 有靶子）
    #[test]
    fn wait_seam_receives_pid() {
        let seen = Arc::new(AtomicUsize::new(0));
        let seen2 = seen.clone();
        let mut r = RunnerCfg::for_test().scripted_pid(7777);
        let _ = r.run_once(move |pid| {
            seen2.store(pid as usize, AtomicOrdering::SeqCst);
        });
        assert_eq!(seen.load(AtomicOrdering::SeqCst), 7777);
    }
}
