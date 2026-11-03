//! 有界后台正文分词工作队列。
use super::{
    body_observer::FinishedBody,
    estimate_store::{commit_estimate, stable_identity, CaptureGate, CapturePermit, EstimateFact},
    reference::count_reference,
};
use crate::services::usage::error::UsageError;
use rusqlite::Connection;
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};

const WORK_QUEUE_CAPACITY: usize = 32;
pub type BeforeCommitHook = Arc<dyn Fn() + Send + Sync>;
pub type CommitSink =
    Arc<dyn Fn(&EstimateFact, &CapturePermit) -> Result<bool, UsageError> + Send + Sync>;
type BuildSource = Arc<dyn Fn(u64) -> Option<String> + Send + Sync>;
struct Job {
    body: FinishedBody,
    plaintext: super::estimate_store::PlaintextGuard,
    permit: CapturePermit,
    run_id: String,
    source_build: String,
    done: tokio::sync::oneshot::Sender<Result<bool, UsageError>>,
}
// 字段按声明顺序析构：即使分词 panic，也先释放正文，再通知停采等待者。
struct TextWork {
    body: FinishedBody,
    _plaintext: super::estimate_store::PlaintextGuard,
}
#[derive(Default)]
struct Queue {
    jobs: VecDeque<Job>,
    closed: bool,
}
#[derive(Clone, Copy, Default)]
pub struct RuntimeDiagnostics {
    pub queue_full: u64,
    pub stale_rejected: u64,
    pub database_failures: u64,
    pub observation_gaps: u64,
    pub follow_failures: u64,
    pub budget_drops: u64,
    pub active_leases: u64,
}
#[derive(Default)]
struct RuntimeCounters {
    queue_full: std::sync::atomic::AtomicU64,
    stale_rejected: std::sync::atomic::AtomicU64,
    database_failures: std::sync::atomic::AtomicU64,
    observation_gaps: std::sync::atomic::AtomicU64,
    follow_failures: std::sync::atomic::AtomicU64,
    budget_drops: std::sync::atomic::AtomicU64,
    active_leases: std::sync::atomic::AtomicU64,
}
impl RuntimeCounters {
    fn snapshot(&self) -> RuntimeDiagnostics {
        use std::sync::atomic::Ordering;
        RuntimeDiagnostics {
            queue_full: self.queue_full.load(Ordering::Relaxed),
            stale_rejected: self.stale_rejected.load(Ordering::Relaxed),
            database_failures: self.database_failures.load(Ordering::Relaxed),
            observation_gaps: self.observation_gaps.load(Ordering::Relaxed),
            follow_failures: self.follow_failures.load(Ordering::Relaxed),
            budget_drops: self.budget_drops.load(Ordering::Relaxed),
            active_leases: self.active_leases.load(Ordering::Relaxed),
        }
    }
}
pub struct BodyRuntime {
    queue: Arc<Mutex<Queue>>,
    wake: Arc<tokio::sync::Notify>,
    gate: CaptureGate,
    counters: Arc<RuntimeCounters>,
    worker: Option<tokio::task::JoinHandle<()>>,
}
impl BodyRuntime {
    pub fn diagnostics(&self) -> RuntimeDiagnostics {
        self.counters.snapshot()
    }
    #[doc(hidden)]
    pub fn new_with_test_sink(sink: CommitSink, gate: CaptureGate) -> Self {
        Self::with_sink(sink, gate, None, None)
    }
    #[doc(hidden)]
    pub fn new_with_test_hooks(
        db: Arc<Mutex<Connection>>,
        gate: CaptureGate,
        before_tokenize: Option<BeforeCommitHook>,
        before_commit: Option<BeforeCommitHook>,
    ) -> Self {
        let sink: CommitSink = Arc::new(move |fact, permit| {
            let mut conn = db.lock().unwrap_or_else(|e| e.into_inner());
            commit_estimate(&mut conn, fact, permit)
        });
        Self::with_sink(sink, gate, before_tokenize, before_commit)
    }
    #[cfg(target_os = "macos")]
    fn production(gate: CaptureGate) -> Self {
        let sink: CommitSink = Arc::new(|fact, permit| {
            let mut conn = crate::database::connection::DB
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            commit_estimate(&mut conn, fact, permit)
        });
        Self::with_sink(sink, gate, None, None)
    }
    pub fn new_with_gate(
        db: Arc<Mutex<Connection>>,
        gate: CaptureGate,
        hook: Option<BeforeCommitHook>,
    ) -> Self {
        let sink: CommitSink = Arc::new(move |fact, permit| {
            let mut conn = db.lock().unwrap_or_else(|e| e.into_inner());
            commit_estimate(&mut conn, fact, permit)
        });
        Self::with_sink(sink, gate, None, hook)
    }
    fn with_sink(
        sink: CommitSink,
        gate: CaptureGate,
        before_tokenize: Option<BeforeCommitHook>,
        hook: Option<BeforeCommitHook>,
    ) -> Self {
        let queue = Arc::new(Mutex::new(Queue::default()));
        let wake = Arc::new(tokio::sync::Notify::new());
        let counters = Arc::new(RuntimeCounters::default());
        let worker = tokio::spawn(worker_loop(
            queue.clone(),
            wake.clone(),
            gate.clone(),
            sink,
            counters.clone(),
            before_tokenize,
            hook,
        ));
        Self {
            queue,
            wake,
            gate,
            counters,
            worker: Some(worker),
        }
    }
    pub async fn shutdown(mut self) {
        self.close_queue();
        if let Some(worker) = self.worker.take() {
            let _ = worker.await;
        }
    }
    fn close_queue(&self) {
        let mut queue = self.queue.lock().unwrap_or_else(|e| e.into_inner());
        queue.closed = true;
        for job in queue.jobs.drain(..) {
            let _ = job.done.send(Ok(false));
        }
        drop(queue);
        self.wake.notify_one();
    }
    pub fn try_submit(
        &self,
        body: FinishedBody,
        permit: CapturePermit,
        run_id: String,
        source_build: String,
    ) -> Option<tokio::sync::oneshot::Receiver<Result<bool, UsageError>>> {
        if !self.gate.owns(&permit) || !permit.is_current() {
            self.counters
                .stale_rejected
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            return None;
        }
        let mut queue = self.queue.lock().unwrap_or_else(|e| e.into_inner());
        if queue.closed {
            return None;
        }
        if queue.jobs.len() >= WORK_QUEUE_CAPACITY {
            self.counters
                .queue_full
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            return None;
        }
        let plaintext = permit.acquire_plaintext()?;
        let (done, receiver) = tokio::sync::oneshot::channel();
        queue.jobs.push_back(Job {
            body,
            plaintext,
            permit,
            run_id,
            source_build,
            done,
        });
        drop(queue);
        self.wake.notify_one();
        Some(receiver)
    }
}
impl Drop for BodyRuntime {
    fn drop(&mut self) {
        self.close_queue();
    }
}

async fn worker_loop(
    queue: Arc<Mutex<Queue>>,
    wake: Arc<tokio::sync::Notify>,
    gate: CaptureGate,
    sink: CommitSink,
    counters: Arc<RuntimeCounters>,
    before_tokenize: Option<BeforeCommitHook>,
    hook: Option<BeforeCommitHook>,
) {
    let Some(mut changes) = gate.subscribe() else {
        return;
    };
    let mut running: Option<tokio::task::JoinHandle<()>> = None;
    loop {
        let (job, closed) = {
            let mut state = queue.lock().unwrap_or_else(|e| e.into_inner());
            let mut kept = VecDeque::new();
            while let Some(job) = state.jobs.pop_front() {
                if job.permit.is_current() {
                    kept.push_back(job)
                } else {
                    counters
                        .stale_rejected
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    let _ = job.done.send(Ok(false));
                }
            }
            state.jobs = kept;
            let job = if running.is_none() {
                state.jobs.pop_front()
            } else {
                None
            };
            (job, state.closed)
        };
        if let Some(job) = job {
            let sink = sink.clone();
            let hook = hook.clone();
            let before_tokenize = before_tokenize.clone();
            let counters = counters.clone();
            running = Some(tokio::task::spawn_blocking(move || {
                let result = process_job(
                    TextWork {
                        body: job.body,
                        _plaintext: job.plaintext,
                    },
                    job.permit,
                    job.run_id,
                    job.source_build,
                    sink,
                    before_tokenize,
                    hook,
                );
                if result
                    .as_ref()
                    .is_err_and(|error| error.code == "usage-db-failed")
                {
                    counters
                        .database_failures
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }
                let _ = job.done.send(result);
            }));
        }
        if closed && running.is_none() {
            return;
        }
        if let Some(task) = running.as_mut() {
            tokio::select! { _=changes.changed()=>{}, _=wake.notified()=>{}, result=task=>{if result.is_err(){log::warn!("tokenizer estimate worker failed");}running=None;} }
        } else {
            tokio::select! { _=changes.changed()=>{}, _=wake.notified()=>{} }
        }
    }
}
fn process_job(
    work: TextWork,
    permit: CapturePermit,
    run_id: String,
    source_build: String,
    sink: CommitSink,
    before_tokenize: Option<BeforeCommitHook>,
    hook: Option<BeforeCommitHook>,
) -> Result<bool, UsageError> {
    if !permit.is_current() {
        return Ok(false);
    }
    if let Some(hook) = before_tokenize {
        hook();
    }
    let body = &work.body;
    let tokens = count_reference(&body.text)?;
    let fact = EstimateFact {
        host_key: stable_identity("host", &body.scope.host_key),
        thread_key: stable_identity("thread", &body.scope.thread_key),
        turn_key: stable_identity("turn", &body.turn_id),
        item_key: stable_identity("item", &body.item_id),
        model: body.model.clone(),
        channel: body.channel.clone(),
        model_source: if body.model.is_some() {
            "turn-config"
        } else {
            "unknown"
        }
        .into(),
        reference_profile_id: body.reference_profile_id.clone(),
        reference_tokens: tokens,
        duration_ns: body.duration_ns,
        ended_at_ms: body.last_wall_ms,
        run_id,
        source_build,
        short_window: body.short_window,
        small_sample: tokens < 50,
    };
    // 正文在进入事务及等待闸门之前释放。
    drop(work);
    if let Some(hook) = hook {
        hook();
    }
    // 重试只携带脱敏事实，不能持有正文；停采后不再发起新事务。
    for attempt in 0..3 {
        if !permit.is_current() {
            return Ok(false);
        }
        match sink(&fact, &permit) {
            Err(error) if error.code == "usage-db-failed" && attempt < 2 => {
                std::thread::sleep(std::time::Duration::from_millis(20 * (attempt + 1)));
            }
            result => return result,
        }
    }
    unreachable!("bounded retry always returns")
}

type BudgetSlot = Arc<
    Mutex<
        Option<std::sync::Weak<crate::services::codex_desktop_ipc::observation::SharedBodyBudget>>,
    >,
>;
type ThreadStream =
    tokio::sync::watch::Receiver<Vec<crate::services::codex_desktop_ipc::types::ThreadKey>>;
struct CaptureContext {
    adapter: Arc<crate::services::codex_desktop_ipc::adapter::DesktopIpcAdapter>,
    gate: CaptureGate,
    source_build: BuildSource,
    host_identity: String,
    budget_slot: BudgetSlot,
}

/// 捕获协调器：只跟随外部监控快照中的线程。
pub struct CaptureService {
    stop: tokio::sync::watch::Sender<bool>,
    gate: CaptureGate,
    closed: bool,
    diagnostics: Arc<RuntimeCounters>,
    task: Option<tokio::task::JoinHandle<()>>,
    budget_slot: BudgetSlot,
}
impl CaptureService {
    #[doc(hidden)]
    pub fn start_for_test(
        adapter: Arc<crate::services::codex_desktop_ipc::adapter::DesktopIpcAdapter>,
        gate: CaptureGate,
        db: Arc<Mutex<Connection>>,
        threads: tokio::sync::watch::Receiver<
            Vec<crate::services::codex_desktop_ipc::types::ThreadKey>,
        >,
        source_build: String,
    ) -> Self {
        let worker = BodyRuntime::new_with_gate(db, gate.clone(), None);
        let build: BuildSource = Arc::new(move |_| Some(source_build.clone()));
        Self::start(adapter, gate, worker, threads, build, "test-host".into())
    }
    fn start(
        adapter: Arc<crate::services::codex_desktop_ipc::adapter::DesktopIpcAdapter>,
        gate: CaptureGate,
        worker: BodyRuntime,
        threads: tokio::sync::watch::Receiver<
            Vec<crate::services::codex_desktop_ipc::types::ThreadKey>,
        >,
        source_build: BuildSource,
        host_identity: String,
    ) -> Self {
        let (stop, stop_rx) = tokio::sync::watch::channel(false);
        let budget_slot = Arc::new(Mutex::new(None));
        let diagnostics = worker.counters.clone();
        let context = CaptureContext {
            adapter,
            gate: gate.clone(),
            source_build,
            host_identity,
            budget_slot: budget_slot.clone(),
        };
        let task = tokio::spawn(capture_loop(context, worker, threads, stop_rx));
        Self {
            stop,
            gate,
            closed: false,
            diagnostics,
            task: Some(task),
            budget_slot,
        }
    }
    pub fn diagnostics(&self) -> RuntimeDiagnostics {
        self.diagnostics.snapshot()
    }
    #[doc(hidden)]
    pub fn buffered_bytes_for_tests(&self) -> usize {
        self.budget_slot
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .and_then(|v| v.upgrade())
            .map_or(0, |budget| budget.used_bytes())
    }
    pub async fn shutdown(mut self) {
        self.stop.send_replace(true);
        self.gate.invalidate_capture();
        self.closed = true;
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
    }
}
impl Drop for CaptureService {
    fn drop(&mut self) {
        if !self.closed {
            self.stop.send_replace(true);
            self.gate.invalidate_capture();
        }
    }
}

async fn capture_loop(
    context: CaptureContext,
    worker: BodyRuntime,
    threads: ThreadStream,
    stop: tokio::sync::watch::Receiver<bool>,
) {
    capture_inner(&context, &worker, threads, stop).await;
    worker.shutdown().await;
}

async fn capture_inner(
    context: &CaptureContext,
    worker: &BodyRuntime,
    mut threads: ThreadStream,
    mut stop: tokio::sync::watch::Receiver<bool>,
) {
    let CaptureContext {
        adapter,
        gate,
        source_build,
        host_identity,
        budget_slot,
    } = context;
    use super::body_observer::BodyObserver;
    use crate::services::codex_desktop_ipc::{observation::BodyEvent, owners::FollowPurpose};
    let Some(mut changes) = gate.subscribe() else {
        return;
    };
    let run_id = crate::services::codex_desktop_ipc::types::new_uuid();
    loop {
        if *stop.borrow() {
            return;
        }
        let Some(permit) = gate.permit() else {
            tokio::select! {_=stop.changed()=>{}, _=changes.changed()=>{}}
            continue;
        };
        let Some(plain_guard) = permit.acquire_plaintext() else {
            continue;
        };
        let mut events = adapter.subscribe_body_events();
        let budget = events.memory_budget();
        *budget_slot.lock().unwrap_or_else(|e| e.into_inner()) = Some(Arc::downgrade(&budget));
        let mut observer = BodyObserver::with_budget(budget.clone());
        let mut recorded_budget_rejections = 0u64;
        let mut leases: std::collections::HashMap<
            crate::services::codex_desktop_ipc::types::ThreadKey,
            crate::services::codex_desktop_ipc::adapter::FollowLease,
        > = std::collections::HashMap::new();
        let mut pending = std::collections::HashMap::new();
        let mut retry_at = std::collections::HashMap::new();
        let mut joins = tokio::task::JoinSet::new();
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(3));
        let mut source_closed = false;
        loop {
            if *stop.borrow() || !permit.is_current() {
                break;
            }
            let mut check_leases = false;
            tokio::select! {
                changed=stop.changed()=>{if changed.is_err(){source_closed=true;}},
                changed=changes.changed()=>{if changed.is_err(){source_closed=true;}},
                changed=threads.changed()=>{if changed.is_err(){source_closed=true;}else{check_leases=true;}},
                _=tick.tick()=>{check_leases=true;},
                Some(result)=joins.join_next_with_id(), if !joins.is_empty()=>{
                    match result {
                        Ok((_,(key,result)))=>{
                            pending.remove(&key);
                            match result {
                                Ok(lease)=>{retry_at.remove(&key);if threads.borrow().contains(&key) && permit.is_current() {leases.insert(key,lease);} else {drop(lease);}},
                                Err(_)=>{worker.counters.follow_failures.fetch_add(1,std::sync::atomic::Ordering::Relaxed);retry_at.insert(key,tokio::time::Instant::now()+std::time::Duration::from_secs(3));},
                            }
                        },
                        Err(error)=>{pending.retain(|_,abort:&mut tokio::task::AbortHandle|abort.id()!=error.id());}
                    }
                },
                event=events.recv()=>{
                    match event {
                        Some(BodyEvent::Lost)=>{worker.counters.observation_gaps.fetch_add(1,std::sync::atomic::Ordering::Relaxed);observer=BodyObserver::with_budget(budget.clone());check_leases=true;},
                        Some(BodyEvent::Frame(frame))=>{
                            let still_monitored=threads.borrow().iter().any(|key|key.host_id==frame.scope.host_key && key.thread_id==frame.scope.thread_key);
                            if permit.is_current() && still_monitored {
                                for mut body in observer.on_verified_frame(frame) {
                                    // 本地 wire 的 "local" 不是跨实例身份；使用不可逆的数据根指纹。
                                    body.scope.host_key=host_identity.clone();
                                    let Some(build)=source_build(body.scope.owner_epoch) else {log::warn!("tokenizer estimate dropped: source generation changed");continue;};
                                    if let Some(done)=worker.try_submit(body,permit.clone(),run_id.clone(),build) {
                                        tokio::spawn(async move {if let Ok(Err(error))=done.await {log::warn!("tokenizer estimate persistence failed: {}",error.code);}});
                                    } else {log::warn!("tokenizer estimate dropped: worker unavailable or queue full");}
                                }
                            } else {
                                observer.invalidate(&frame.scope);
                            }
                            events.release_delivered();
                        },
                        None=>break,
                    }
                },
            }
            let rejected = budget.rejections();
            worker.counters.budget_drops.fetch_add(
                rejected.saturating_sub(recorded_budget_rejections),
                std::sync::atomic::Ordering::Relaxed,
            );
            recorded_budget_rejections = rejected;
            if source_closed || *stop.borrow() || !permit.is_current() {
                break;
            }
            let desired: std::collections::HashSet<_> = threads.borrow().iter().cloned().collect();
            if check_leases {
                let mut lost = Vec::new();
                for (key, lease) in &leases {
                    if !lease.is_current().await {
                        lost.push(key.clone());
                    }
                }
                for key in lost {
                    leases.remove(&key);
                    retry_at.insert(
                        key,
                        tokio::time::Instant::now() + std::time::Duration::from_secs(3),
                    );
                }
            }
            leases.retain(|key, _| desired.contains(key));
            worker
                .counters
                .active_leases
                .store(leases.len() as u64, std::sync::atomic::Ordering::Relaxed);
            retry_at.retain(|key, _| desired.contains(key));
            pending.retain(|key, abort: &mut tokio::task::AbortHandle| {
                if desired.contains(key) {
                    true
                } else {
                    abort.abort();
                    false
                }
            });
            let occupied: std::collections::HashSet<_> =
                leases.keys().chain(pending.keys()).cloned().collect();
            let eligible: std::collections::HashSet<_> = desired
                .iter()
                .filter(|key| {
                    !retry_at
                        .get(*key)
                        .is_some_and(|at| *at > tokio::time::Instant::now())
                })
                .cloned()
                .collect();
            for key in bounded_follow_candidates(&eligible, &occupied) {
                let target = key.clone();
                let client = adapter.clone();
                let abort = joins.spawn(async move {
                    let result = client
                        .acquire_following(&target, FollowPurpose::TokenizerEstimate)
                        .await;
                    (target, result)
                });
                pending.insert(key, abort);
            }
        }
        joins.abort_all();
        events.close_and_wait().await;
        let rejected = budget.rejections();
        worker.counters.budget_drops.fetch_add(
            rejected.saturating_sub(recorded_budget_rejections),
            std::sync::atomic::Ordering::Relaxed,
        );
        drop(observer);
        drop(events);
        drop(leases);
        worker
            .counters
            .active_leases
            .store(0, std::sync::atomic::Ordering::Relaxed);
        drop(plain_guard);
        if source_closed || *stop.borrow() {
            return;
        }
    }
}

/// 从既有监控快照挑选 Codex APP 线程，不触发扫描。
#[doc(hidden)]
pub fn cached_thread_keys<F>(
    sessions: &[crate::session::Session],
    resolve: F,
) -> Vec<crate::services::codex_desktop_ipc::types::ThreadKey>
where
    F: Fn(&str) -> Option<String>,
{
    let ids = crate::adapter::select_codex_app_ids(sessions);
    resolve_thread_ids(&ids, resolve)
}
fn resolve_thread_ids<F>(
    ids: &[String],
    resolve: F,
) -> Vec<crate::services::codex_desktop_ipc::types::ThreadKey>
where
    F: Fn(&str) -> Option<String>,
{
    let mut seen = std::collections::HashSet::new();
    ids.iter()
        .filter_map(|id| resolve(id))
        .filter(|id| crate::inject::headless::codex::is_uuid(id) && seen.insert(id.clone()))
        .map(
            |thread_id| crate::services::codex_desktop_ipc::types::ThreadKey {
                host_id: "local".into(),
                thread_id,
            },
        )
        .collect()
}

#[cfg(target_os = "macos")]
static PRODUCTION_STOPPED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);
#[cfg(target_os = "macos")]
static PRODUCTION_STARTED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);
#[cfg(target_os = "macos")]
static PRODUCTION_SERVICE: std::sync::Mutex<Option<CaptureService>> = std::sync::Mutex::new(None);

/// 读取已运行服务的轻量状态；不初始化采集，也不访问磁盘。
#[cfg(target_os = "macos")]
pub fn production_diagnostics() -> Option<RuntimeDiagnostics> {
    PRODUCTION_SERVICE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .map(CaptureService::diagnostics)
}
#[cfg(not(target_os = "macos"))]
pub fn production_diagnostics() -> Option<RuntimeDiagnostics> {
    None
}

/// 应用启动时只装配一次；不会启动远程服务或扫描历史目录。
#[cfg(target_os = "macos")]
pub fn spawn_production() {
    use std::sync::atomic::Ordering;
    if PRODUCTION_STARTED.swap(true, Ordering::SeqCst) {
        return;
    }
    tauri::async_runtime::spawn(async {
        let setup = tokio::task::spawn_blocking(|| {
            (
                super::estimate_store::global_capture_gate().clone(),
                crate::remote::codex_ipc::shared_production_adapter(),
            )
        })
        .await;
        let Ok((gate, adapter)) = setup else {
            log::warn!("tokenizer estimate setup failed");
            return;
        };
        if PRODUCTION_STOPPED.load(Ordering::SeqCst) {
            return;
        }
        let (tx, rx) = tokio::sync::watch::channel(Vec::new());
        let worker = BodyRuntime::production(gate.clone());
        let build_adapter = adapter.clone();
        let source_build: BuildSource =
            Arc::new(move |epoch| build_adapter.source_build_for_epoch(epoch));
        let root = crate::services::codex_desktop_ipc::transport::unix::default_codex_home(None)
            .join("sessions");
        let host_identity = super::sources::host_instance_tag(&root);
        let service = CaptureService::start(
            adapter,
            gate.clone(),
            worker,
            rx,
            source_build,
            host_identity,
        );
        let stop = service.stop.subscribe();
        tokio::spawn(cached_session_bridge(gate, tx, stop));
        let mut slot = PRODUCTION_SERVICE.lock().unwrap_or_else(|e| e.into_inner());
        if PRODUCTION_STOPPED.load(Ordering::SeqCst) {
            drop(service);
        } else {
            *slot = Some(service);
        }
    });
}
#[cfg(not(target_os = "macos"))]
pub fn spawn_production() {}

/// 进程退出时发出停止信号；不创建系统守护进程。
#[cfg(target_os = "macos")]
pub fn shutdown_production() {
    PRODUCTION_STOPPED.store(true, std::sync::atomic::Ordering::SeqCst);
    let service = PRODUCTION_SERVICE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .take();
    drop(service);
}
#[cfg(not(target_os = "macos"))]
pub fn shutdown_production() {}

#[cfg(target_os = "macos")]
async fn cached_session_bridge(
    gate: CaptureGate,
    tx: tokio::sync::watch::Sender<Vec<crate::services::codex_desktop_ipc::types::ThreadKey>>,
    mut stop: tokio::sync::watch::Receiver<bool>,
) {
    let mut sessions = crate::adapter::subscribe_cached_sessions();
    let Some(mut changes) = gate.subscribe() else {
        return;
    };
    loop {
        if *stop.borrow() {
            return;
        }
        if let Some(permit) = gate.permit() {
            let observed = tokio::task::spawn_blocking(|| {
                let Some(ids) = crate::adapter::cached_codex_app_ids() else {
                    return Vec::new();
                };
                let root =
                    crate::services::codex_desktop_ipc::transport::unix::default_codex_home(None);
                let roots =
                    crate::monitor::codex_thread_parser::CodexThreadRoots::from_codex_root(&root);
                resolve_thread_ids(&ids, |id| {
                    let path =
                        crate::inject::headless::codex::rollout_path_in_db(&roots.state_db, id);
                    path.as_deref()
                        .and_then(crate::inject::headless::codex::thread_id_of)
                        .or_else(|| {
                            crate::inject::headless::codex::is_uuid(id).then(|| id.to_owned())
                        })
                })
            })
            .await;
            if permit.is_current() {
                if let Ok(keys) = observed {
                    tx.send_if_modified(|old| {
                        if *old != keys {
                            *old = keys;
                            true
                        } else {
                            false
                        }
                    });
                }
            }
        } else {
            tx.send_if_modified(|old| {
                if old.is_empty() {
                    false
                } else {
                    old.clear();
                    true
                }
            });
        }
        tokio::select! {
            changed=stop.changed()=>if changed.is_err() {return},
            changed=changes.changed()=>if changed.is_err() {return},
            changed=sessions.changed()=>if changed.is_err() {return},
        }
    }
}

fn bounded_follow_candidates(
    desired: &std::collections::HashSet<crate::services::codex_desktop_ipc::types::ThreadKey>,
    occupied: &std::collections::HashSet<crate::services::codex_desktop_ipc::types::ThreadKey>,
) -> Vec<crate::services::codex_desktop_ipc::types::ThreadKey> {
    let available =
        crate::services::codex_desktop_ipc::owners::MAX_FOLLOWINGS.saturating_sub(occupied.len());
    desired
        .iter()
        .filter(|key| !occupied.contains(*key))
        .take(available)
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn follow_candidates_respect_global_capacity() {
        use crate::services::codex_desktop_ipc::{owners::MAX_FOLLOWINGS, types::ThreadKey};
        let desired = (0..100)
            .map(|i| ThreadKey {
                host_id: "local".into(),
                thread_id: format!("{i}"),
            })
            .collect();
        let occupied = (0..10)
            .map(|i| ThreadKey {
                host_id: "local".into(),
                thread_id: format!("{i}"),
            })
            .collect();
        let candidates = bounded_follow_candidates(&desired, &occupied);
        assert_eq!(candidates.len(), MAX_FOLLOWINGS - 10);
        assert!(candidates.iter().all(|key| !occupied.contains(key)));
    }
}
