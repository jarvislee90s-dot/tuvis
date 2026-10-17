//! DesktopIpcAdapter：Desktop IPC 控制面门面（connection/owners/mirror/pending 的
//! 单一编排点）。与 `inject/headless` 平行，不共用其 transport/envelope/并发额度。
//!
//! 纪律（计划 Task 6）：
//! * 所有写入在派发前**最后复核**（lease 现行 + pending 现存 + token 有效 + 能力
//!   已验证），响应前读到的过期 lease 不算授权（writer 侧还有 validity 复核兜底）；
//! * start-turn 只带 `conversationId/turnStart{request{threadId,input,text_elements,
//!   clientUserMessageId},context{}}`——不发 model/权限/collaboration 覆盖；
//! * 两类审批各走各的 wire：command → `decision:"accept"|"decline"`；permissions →
//!   回显服务端保存的原 permissions（批准）或 `{}`（拒绝）+ `scope:"turn"`，
//!   绝不混用、绝不接受远端上送的自定义 permissions；
//! * 阻塞问答按原 qid 映射，不能漏答/混入未知 qid；`isOther=false` 禁自由文本；
//!   `isSecret` 的回答不进日志/错误文本（不回显保证）；
//! * 锁纪律：所有互斥锁只在同步临界区持有，不跨 await；
//! * 忙 Thread 留队由 router（Task 7）调度；审批/问答不等普通消息队列。

use super::connection::{ConnectionEvent, ConnectionHandle, RpcOutcome, RpcRequest};
use super::mirror::ThreadMirror;
use super::owners::{
    bind_lease, declare_following, discover_owner, wait_first_snapshot, FollowingSet,
};
use super::pending::parse_request_key;
use super::profile::ProtocolProfile;
use super::types::{
    CapabilitySet, ControlAvailability, Delivery, OwnerLease, PendingAction, PendingKind,
    PendingView, ProtocolError, ThreadKey, ThreadProjection,
};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::{broadcast, Mutex};

/// 连接工厂（生产：endpoint 解析 + transport；测试：注入 ScriptedPeer 的 io）。
///
/// R3（2026-10-17 审计）：连接必须绑定 endpoint 身份（build 门）——[`Connector::connect_endpoint`]
/// 返回 `(io, endpoint)`，adapter 每次连接按 endpoint 重算 profile；旧式只产 io 的
/// 实现走 [`Connector::connect`]（无 endpoint → closed profile，A 写全关）。
/// 两个方法互为缺省委托：实现者必须至少覆盖其一，否则运行时无限递归。
pub trait Connector: Send + Sync {
    /// 连接并携带 endpoint 身份（platform/build 来源；R3 每次连接重算能力的依据）。
    #[allow(clippy::type_complexity)]
    fn connect_endpoint(
        &self,
    ) -> Pin<
        Box<
            dyn std::future::Future<
                    Output = Result<
                        (super::transport::BoxIo, super::transport::DesktopEndpoint),
                        ProtocolError,
                    >,
                > + Send
                + '_,
        >,
    > {
        Box::pin(async move {
            let io = self.connect().await?;
            // 旧式实现不暴露 endpoint：按无 endpoint 处理（closed profile，只读仍可）
            let endpoint = super::transport::DesktopEndpoint {
                platform: std::env::consts::OS.to_string(),
                path: String::new(),
                build: None,
            };
            Ok((io, endpoint))
        })
    }

    /// 兼容旧形态：仅连接（缺省委托 `connect_endpoint`，丢弃 endpoint）。
    fn connect(
        &self,
    ) -> Pin<
        Box<
            dyn std::future::Future<Output = Result<super::transport::BoxIo, ProtocolError>>
                + Send
                + '_,
        >,
    > {
        Box::pin(async move { Ok(self.connect_endpoint().await?.0) })
    }
}

/// 生产连接器：resolve endpoint → transport::connect（endpoint 身份不丢弃，R3）。
pub struct ProductionConnector {
    pub resolver: Arc<dyn super::transport::EndpointResolver>,
}

impl Connector for ProductionConnector {
    fn connect_endpoint(
        &self,
    ) -> Pin<
        Box<
            dyn std::future::Future<
                    Output = Result<
                        (super::transport::BoxIo, super::transport::DesktopEndpoint),
                        ProtocolError,
                    >,
                > + Send
                + '_,
        >,
    > {
        Box::pin(async move {
            let endpoint: super::transport::DesktopEndpoint = self.resolver.resolve()?;
            let io = super::transport::connect(&endpoint).await?;
            Ok((io, endpoint))
        })
    }
}

/// 交互提交互斥条目（R4 + 2026-10-17 复审 C3）：`Dispatching` = 已派发待回执；
/// `BlockedUnknown` = 未知投递——同 (thread, request_key) 不得借新 actionId 再发。
/// **屏障按完整上下文分代**：`token` 记录 Unknown 提交所用的 contextToken——该
/// pending 消失或重现换 token（服务端新世界）即由状态应用驱动清理，旧 Unknown
/// 不再挡新卡；旧 token 本身仍被 pending 投影器拒绝。
#[derive(Debug, Clone)]
struct InflightEntry {
    state: InflightState,
    /// BlockedUnknown 时的提交上下文 token（Dispatching 为 None）
    token: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InflightState {
    Dispatching,
    BlockedUnknown,
}

struct AdapterInner {
    connector: Arc<dyn Connector>,
    /// 当前连接的 profile（R3：动态）。初始为构造入参（首个连接前的基线）；
    /// 每次连接按 endpoint build 重算（未知 build/无 endpoint → `closed()`）。
    /// 同步短临界区读写，绝不跨 await 持有、不与其他锁嵌套。
    profile: std::sync::Mutex<ProtocolProfile>,
    conn: Mutex<Option<ConnectionHandle>>,
    /// follow 互斥（C2）：settle 对账与派发路径可能并发 ensure 同一 thread——
    /// 冷启动竞跑会产生重复 discovery/绑定（lease 翻代）。持锁串行化 follow 段；
    /// 后到者在锁内重查新鲜镜像即短路。
    ensure_lock: tokio::sync::Mutex<()>,
    owners: Mutex<OwnerRegistry>,
    mirrors: Mutex<HashMap<ThreadKey, ThreadMirror>>,
    /// C4：全体镜像字节账本（生产 64MiB；所有镜像共用一份）
    mirror_ledger: Arc<std::sync::Mutex<super::mirror::MirrorLedger>>,
    followings: Mutex<FollowingSet>,
    /// (thread_id, request_key) → 提交互斥（R4；独立短锁，不与 mirrors/owners 嵌套）。
    inflight: Mutex<HashMap<(String, String), InflightEntry>>,
    /// 本 follow 已发起过 load-complete-history 的 thread（R1：bind 即清——每 follow
    /// 至多一次；owner 换代/重跟随自动可再发起）。
    history_requested: Mutex<HashSet<ThreadKey>>,
    /// 事件转发任务单飞标志（每 adapter 生命周期至多一个 forward_events）。
    forward_alive: AtomicBool,
    /// 重连恢复循环单飞标志（R2.5）。
    recovering: AtomicBool,
    /// 同 socket 恢复的工作队列与消费者资格（第四轮复审 B2 结构化）：pending 与
    /// running 同锁——「取空待办」与「释放消费者资格」在**同一临界区**完成，
    /// 退出判定与并发入队之间不存在窗口。
    recovery_queue: std::sync::Mutex<RecoveryQueue>,
    /// shutdown 信号（恢复循环/重拉循环的退出条件）。
    stopping: AtomicBool,
    /// C3 测试缝：respond 两阶段间的可注入等待（生产恒 None → 零开销直通）；
    /// 回归测试用它固定「阶段 1 读镜像之后、阶段 3 取 lease 之前」的 owner 换代。
    interphase_gate: std::sync::Mutex<Option<Arc<tokio::sync::Notify>>>,
    /// 第四轮复审 B3 测试缝：follow 的 bind 与 declare 之间的可注入等待
    /// （生产恒 None → 零开销直通）；回归固定「bind/declare 间卸载」交错。
    follow_gate: std::sync::Mutex<Option<Arc<tokio::sync::Notify>>>,
    /// B3 测试缝探针：任务已到达 follow gate。
    follow_gate_reached: AtomicBool,
    epoch: AtomicU64,
    events: broadcast::Sender<ThreadProjection>,
}

use super::owners::OwnerRegistry;

/// Desktop IPC 适配器（克隆共享）。
#[derive(Clone)]
pub struct DesktopIpcAdapter {
    inner: Arc<AdapterInner>,
}

impl DesktopIpcAdapter {
    pub fn new(connector: Arc<dyn Connector>, profile: ProtocolProfile) -> Self {
        Self {
            inner: Arc::new(AdapterInner {
                connector,
                profile: std::sync::Mutex::new(profile),
                conn: Mutex::new(None),
                ensure_lock: tokio::sync::Mutex::new(()),
                owners: Mutex::new(OwnerRegistry::default()),
                mirrors: Mutex::new(HashMap::new()),
                mirror_ledger: Arc::new(std::sync::Mutex::new(
                    super::mirror::MirrorLedger::global(),
                )),
                followings: Mutex::new(FollowingSet::default()),
                inflight: Mutex::new(HashMap::new()),
                history_requested: Mutex::new(HashSet::new()),
                forward_alive: AtomicBool::new(false),
                recovering: AtomicBool::new(false),
                recovery_queue: std::sync::Mutex::new(RecoveryQueue::default()),
                stopping: AtomicBool::new(false),
                interphase_gate: std::sync::Mutex::new(None),
                follow_gate: std::sync::Mutex::new(None),
                follow_gate_reached: AtomicBool::new(false),
                epoch: AtomicU64::new(0),
                events: broadcast::channel(256).0,
            }),
        }
    }

    /// 当前连接的 profile（R3 动态值；clone 出短临界区）。
    fn current_profile(&self) -> ProtocolProfile {
        self.inner.profile.lock().expect("profile lock").clone()
    }

    /// 更新当前 profile（同步短临界区；guard 不进入 async 栈帧）。
    fn set_profile(&self, profile: ProtocolProfile) {
        *self.inner.profile.lock().expect("profile lock") = profile;
    }

    fn current_caps(&self) -> CapabilitySet {
        self.current_profile().capabilities
    }

    /// 本 build 的能力基线（能力四档的来源；R3：读当前连接的动态 profile，
    /// 无连接/未知 build → closed）。
    pub fn capabilities(&self) -> CapabilitySet {
        self.current_caps()
    }

    /// 订阅投影事件（REST/SSE/队列调度共用）。
    pub fn subscribe(&self) -> broadcast::Receiver<ThreadProjection> {
        self.inner.events.subscribe()
    }

    /// 确保连接存活（每次重连 epoch 递增，不重置为 0）。
    /// R3：每次连接按 endpoint 重算 profile（未知 build/无 endpoint → closed）；
    /// 连接失败同样落到 closed——Desktop 晚启动不需要重启 Tuvis，投影照常给出
    /// stale/CompatOnly。R5：全程单锁短临界区，不嵌套。
    async fn ensure_connection(&self) -> Result<ConnectionHandle, ProtocolError> {
        if self.inner.stopping.load(Ordering::SeqCst) {
            // C4：shutdown 后不再新建连接（恢复循环/热重启统一由 stop/resume 管理）
            return Err(ProtocolError::Disconnected("adapter shutting down".into()));
        }
        {
            let conn = self.inner.conn.lock().await;
            if let Some(c) = conn.as_ref() {
                if !c.is_closed() {
                    return Ok(c.clone());
                }
            }
        }
        // 连接 + endpoint 身份（R3：resolver 产 endpoint，build 不丢弃）
        let (io, endpoint) = match self.inner.connector.connect_endpoint().await {
            Ok(v) => v,
            Err(e) => {
                // 无 endpoint（Desktop 缺席等）：诚实按 closed 投影
                self.set_profile(ProtocolProfile::closed());
                return Err(e);
            }
        };
        // endpoint → profile（R3：精确 build 匹配；未知 build → closed，A 写全关）
        let profile = super::profile::profile_for(
            &endpoint.platform,
            endpoint.build.as_deref().unwrap_or(""),
        )
        .unwrap_or_else(ProtocolProfile::closed);
        // 双检：另一任务可能已重连（持 conn 锁期间不做其他锁操作）
        let (conn, epoch) = {
            let mut guard = self.inner.conn.lock().await;
            if let Some(c) = guard.as_ref() {
                if !c.is_closed() {
                    return Ok(c.clone());
                }
            }
            let epoch = self.inner.epoch.fetch_add(1, Ordering::SeqCst) + 1;
            let conn = super::connection::spawn(io, profile.clone(), epoch);
            *guard = Some(conn.clone());
            (conn, epoch)
        };
        // 已释放 conn 锁：换代清理（短锁逐个）
        self.inner.owners.lock().await.reset(epoch);
        self.set_profile(profile);
        // 旧镜像全部标 stale（重新 following 的 snapshot 复活）
        {
            let mut mirrors = self.inner.mirrors.lock().await;
            for mirror in mirrors.values_mut() {
                mirror.mark_stale();
            }
        }
        // 事件转发任务单飞（恢复期间由恢复任务在 follow 前自行就位，不在此 spawn）
        if !self.inner.recovering.load(Ordering::SeqCst)
            && !self.inner.forward_alive.swap(true, Ordering::SeqCst)
        {
            spawn_forward_task(self.clone(), conn.clone());
        }
        Ok(conn)
    }

    /// 确保线程已发现 owner、声明 following 并收到 snapshot；返回投影。
    /// C2：follow 段经 ensure_lock 串行化——settle 对账与派发路径并发 ensure 时，
    /// 后到者持锁后重查新鲜镜像即短路（不产生第二次 discovery/绑定）。
    pub async fn ensure_thread(&self, key: &ThreadKey) -> Result<ThreadProjection, ProtocolError> {
        let conn = self.ensure_connection().await?;
        // 快路径：已跟随且新鲜（短锁）
        if let Some(p) = self.fresh_projection(key).await {
            return Ok(p);
        }
        let _ensure = self.inner.ensure_lock.lock().await;
        // 持锁重查：并发 ensure 可能已完成 follow
        if let Some(p) = self.fresh_projection(key).await {
            return Ok(p);
        }
        // 名额（短锁）
        {
            let mut f = self.inner.followings.lock().await;
            if !f.follow(key.clone()) {
                return Err(ProtocolError::Capacity("following set full (64)".into()));
            }
        }
        self.follow_thread(&conn, key).await?;
        Ok(self.projection(key).await)
    }

    /// 已跟随且新鲜的投影（短锁；None = 需要 follow）。
    async fn fresh_projection(&self, key: &ThreadKey) -> Option<ThreadProjection> {
        let mirrors = self.inner.mirrors.lock().await;
        mirrors
            .get(key)
            .filter(|m| !m.is_stale() && m.revision().is_some())
            .map(|m| m.project(self.current_caps()))
    }

    /// 恢复任务的 follow 统一入口（第三轮复审 B3）：**前置核验**仍有关注（已卸载
    /// 的 key——含 B2 待办合并进来的——直接跳过，不 discovery、不重建镜像）；
    /// follow 完成后**事后复查**，途中被退出关注的（discovery/following 已在途）
    /// 立即回收复活物（following:false + lease + 镜像）——撤销使旧恢复工作失效，
    /// 不只在任务开始时查一次。
    async fn follow_if_following(
        &self,
        conn: &ConnectionHandle,
        key: &ThreadKey,
    ) -> Result<(), ProtocolError> {
        {
            let f = self.inner.followings.lock().await;
            if !f.is_following(key) {
                return Ok(()); // 已卸载：跳过（不重新订阅、不复活镜像）
            }
        }
        // 第四轮复审 B3：成功/错误/取消**所有出口都过统一收尾**——本任务可能已
        // declare(true) 而本地关注已被撤销；错误出口不得 `?` 早退跳过撤销核验
        let result = self.follow_thread(conn, key).await;
        if !self.inner.followings.lock().await.is_following(key) {
            // unfollow 幂等重定义版：有复活 lease/mirror 即回收（含补偿 false）
            self.unfollow(key).await;
        }
        result
    }

    /// 可复用的 follow 流程（R2.5：ensure_thread 与重连恢复共用）：
    /// 建镜像（接 snapshot）→ discovery → 绑定 → following → 等 snapshot。
    /// 锁序纪律（R5）：mirrors → owners，各自短临界区，绝不嵌套、不跨 await 持锁。
    async fn follow_thread(
        &self,
        conn: &ConnectionHandle,
        key: &ThreadKey,
    ) -> Result<(), ProtocolError> {
        // mirror 必须在 following 声明前就位（snapshot 由事件转发落进来；
        // C4：镜像挂全体字节账本）
        {
            let mut mirrors = self.inner.mirrors.lock().await;
            mirrors.entry(key.clone()).or_insert_with(|| {
                ThreadMirror::with_ledger(key.clone(), self.inner.mirror_ledger.clone())
            });
        }
        // discovery（await 不持锁）→ 绑定（短锁）。
        // 第三轮复审 B3：discovery 在途期间关注可能已被撤销——回包后**bind/declare
        // 之前**核验，已卸载则清除为接 snapshot 而建的空镜像并 bail（不绑定 lease、
        // 不声明 following:true）。调用方按失败退避，下一轮前置核验跳过。
        let owner_id = discover_owner(conn, key).await?;
        {
            let f = self.inner.followings.lock().await;
            if !f.is_following(key) {
                let removed = self.inner.mirrors.lock().await.remove(key);
                drop(removed);
                return Err(ProtocolError::StaleContext(
                    "unfollowed during discovery".into(),
                ));
            }
        }
        let lease = {
            let mut owners = self.inner.owners.lock().await;
            bind_lease(&mut owners, key, owner_id, conn.epoch())
        };
        // 新 follow（owner 换代）：load-complete-history 每 follow 至多一次（R1）
        self.inner.history_requested.lock().await.remove(key);
        // B3 测试缝：bind 与 declare 之间的可注入等待（生产恒 None 直通）
        let gate = self.inner.follow_gate.lock().expect("gate lock").clone();
        if let Some(gate) = gate {
            self.inner.follow_gate_reached.store(true, Ordering::SeqCst);
            gate.notified().await;
            self.inner
                .follow_gate_reached
                .store(false, Ordering::SeqCst);
        }
        // 第四轮复审 B3：declare **之前**最后核验——bind/declare 窗口内已卸载则
        // 不声明（清刚 bind 的复活 lease 与镜像，bail 交调用方退避/跳过）
        if !self.inner.followings.lock().await.is_following(key) {
            self.inner.owners.lock().await.release(key);
            let removed = self.inner.mirrors.lock().await.remove(key);
            drop(removed);
            return Err(ProtocolError::StaleContext(
                "unfollowed before declare".into(),
            ));
        }
        let mut validity = {
            let owners = self.inner.owners.lock().await;
            owners.validity(&lease)
        };
        declare_following(conn, key).await?;
        // 第四轮复审 B3：declare **之后**复查——declare 前已发生的 unfollow 把
        // following:false 发上了 wire，本任务刚发出的 true 就是「false 之后的
        // 迟到 true」，必须由本任务**直接向 wire** 补偿撤销（不依赖本地 lease/
        // mirror 是否还在——它们可能已被 unfollow 清空），随后清本地并按失败退出
        if !self.inner.followings.lock().await.is_following(key) {
            if let Ok(version) = conn.version_of("thread-stream-following-changed") {
                let _ = conn
                    .broadcast(
                        "thread-stream-following-changed",
                        version,
                        serde_json::json!({
                            "conversationId": key.thread_id,
                            "hostId": key.host_id,
                            "following": false,
                        }),
                    )
                    .await;
            }
            self.inner.owners.lock().await.release(key);
            let removed = self.inner.mirrors.lock().await.remove(key);
            drop(removed);
            return Err(ProtocolError::StaleContext(
                "unfollowed around declare".into(),
            ));
        }
        // 换代兜底：following 声明后 lease 失效（如 Desktop 断开）则失败重试
        if !*validity.borrow_and_update() {
            return Err(ProtocolError::StaleContext(
                "lease lost during follow".into(),
            ));
        }
        wait_first_snapshot(conn, key).await?;
        let _ = &mut validity;
        self.wait_fresh_projection(key).await?;
        Ok(())
    }

    async fn wait_fresh_projection(&self, key: &ThreadKey) -> Result<(), ProtocolError> {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            {
                let mirrors = self.inner.mirrors.lock().await;
                if let Some(m) = mirrors.get(key) {
                    if !m.is_stale() && m.revision().is_some() {
                        return Ok(());
                    }
                }
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(ProtocolError::Timeout(
                    "snapshot not applied in time".into(),
                ));
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }

    /// 当前投影（不触发连接；未跟随/无状态 → stale 空投影）。
    pub async fn projection(&self, key: &ThreadKey) -> ThreadProjection {
        let caps = self.current_caps();
        let mirrors = self.inner.mirrors.lock().await;
        if let Some(m) = mirrors.get(key) {
            return m.project(caps);
        }
        ThreadProjection {
            thread_id: key.thread_id.clone(),
            revision: None,
            stale: true,
            availability: ControlAvailability::CompatOnly,
            capabilities: caps,
            runtime_status: "unknown".into(),
            messages: Vec::new(),
            truncated: false,
            pending: Vec::new(),
        }
    }

    /// 同步忙快照（Codex Desktop IPC Task 8 加性接口：`DesktopDispatch::thread_busy`
    /// 的 R2.3 忙判定消费；不跨 await）。active/unknown（含快照缺字段）按「忙」保守
    /// ——消息留队等 idle 边沿接力；idle/failed 不忙；锁被占（镜像正在写）也按忙保守。
    /// 无镜像（未跟随）= 不忙（派发路径 ensure_thread 先行，正常不可达此态）。
    pub fn busy_snapshot(&self, key: &ThreadKey) -> bool {
        let caps = self.current_caps();
        match self.inner.mirrors.try_lock() {
            Ok(mirrors) => mirrors
                .get(key)
                .map(|m| {
                    matches!(
                        m.project(caps).runtime_status.as_str(),
                        "active" | "unknown"
                    )
                })
                .unwrap_or(false),
            Err(_) => true,
        }
    }

    /// 2026-10-17 复审 C1：绑定 Turn 的终态证据——该 turnId 的实体在当前镜像中以
    /// 终态（completed/failed）出现才 true。无镜像/镜像 stale/锁被占（镜像正在写）
    /// → false（保守不释放：屏障宁可多留一拍，绝不因证据缺失放行第二条）。
    pub fn turn_evidence_terminal(&self, key: &ThreadKey, turn_id: &str) -> bool {
        match self.inner.mirrors.try_lock() {
            Ok(mirrors) => mirrors
                .get(key)
                .map(|m| m.turn_is_terminal(turn_id))
                .unwrap_or(false),
            Err(_) => false,
        }
    }

    /// 退出跟随（2026-10-17 复审 C4 收口：最后一引用退出 = 真正退场）——
    /// 向 Desktop 声明 `following:false`、回收 owner lease（后续该 owner 的帧无
    /// 授权可套）、删除镜像（字节账本随 Drop 释放；卸载后的帧**不复活镜像**）。
    /// 未在跟随（幂等）→ 零副作用。
    pub async fn unfollow(&self, key: &ThreadKey) {
        let was_following = self.inner.followings.lock().await.unfollow(key);
        // C4：只对**真正声明过 following**（有现行 lease）的 thread 发 following:false
        //——ensure 失败回滚（discovery 未成功、从未声明）只回收名额，不发噪声帧
        let had_lease = self.inner.owners.lock().await.current(key).is_some();
        let had_mirror = self.inner.mirrors.lock().await.contains_key(key);
        // 第三轮复审 B3：幂等 = 「真的无物可回收」——following 集合、lease、镜像
        // 三者皆无才早退。在途恢复任务复活出的 lease/镜像（following 已被撤销）
        // 必须同样被回收，不能因 was_following=false 提前返回而滞留。
        if !was_following && !had_lease && !had_mirror {
            return;
        }
        if had_lease && !self.inner.stopping.load(Ordering::SeqCst) {
            if let Some(conn) = current_conn(self).await {
                if let Ok(version) = conn.version_of("thread-stream-following-changed") {
                    let _ = conn
                        .broadcast(
                            "thread-stream-following-changed",
                            version,
                            serde_json::json!({
                                "conversationId": key.thread_id,
                                "hostId": key.host_id,
                                "following": false,
                            }),
                        )
                        .await;
                }
            }
        }
        self.inner.owners.lock().await.release(key);
        let removed = self.inner.mirrors.lock().await.remove(key);
        drop(removed); // Drop 释放字节账本占用
    }

    /// C4：进入停机——恢复循环/重拉/转发不再重启，ensure 不再新建连接；
    /// 已派发动作的独立落账任务不受影响（只经 store/conn 完成落账）。
    pub fn begin_shutdown(&self) {
        self.inner.stopping.store(true, Ordering::SeqCst);
    }

    /// C4：从停机恢复（hub.start 热重启路径）——重新允许连接。
    pub fn resume(&self) {
        self.inner.stopping.store(false, Ordering::SeqCst);
    }

    /// C4 探针：是否处于停机态。
    #[doc(hidden)]
    pub fn is_stopping(&self) -> bool {
        self.inner.stopping.load(Ordering::SeqCst)
    }

    /// 原生 start turn（text 为已组装文本——来源标记由 router 恰好组装一次）。
    pub async fn start_turn(
        &self,
        key: &ThreadKey,
        _action_id: &str,
        text: &str,
        client_message_id: &str,
    ) -> RpcOutcome {
        let envelope_id = super::types::new_uuid();
        // R3：能力读当前连接的动态 profile（无连接/未知 build → closed → 拒）
        if !self.current_caps().send {
            return not_sent(&envelope_id, "capability_unavailable");
        }
        let (lease, validity) = {
            let owners = self.inner.owners.lock().await;
            match current_lease(&owners, key) {
                Some(l) => {
                    let v = owners.validity(&l);
                    (l, v)
                }
                None => return not_sent(&envelope_id, "stale_context"),
            }
        };
        let version = {
            let conn = self.inner.conn.lock().await;
            match conn
                .as_ref()
                .map(|c| c.version_of("thread-follower-start-turn"))
            {
                Some(Ok(v)) => v,
                _ => return not_sent(&envelope_id, "disconnected"),
            }
        };
        let params = json!({
            "conversationId": key.thread_id,
            "turnStart": {
                "request": {
                    "threadId": key.thread_id,
                    "input": [{ "type": "text", "text": text, "text_elements": [] }],
                    "clientUserMessageId": client_message_id,
                },
                "context": {},
            }
        });
        self.dispatch(
            "thread-follower-start-turn",
            version,
            params,
            lease,
            validity,
        )
        .await
    }

    /// 审批/作答。requestKey/contextToken 回显校验；服务端保存的 pending 上下文是
    /// 唯一事实源（远端不上送 owner/permissions/raw method）。
    /// 2026-10-17 复审 C3：拆锁后的最终复核携带**完整上下文代际**——阶段 1 记录镜像
    /// 绑定的 owner lease；阶段 3 的现行 lease 必须与其同代（client/epoch/generation
    /// 全同），并在 owners 复核后以短锁重验镜像非 stale 且 token 仍有效（卡内容/存在
    /// 未变）。两阶段间 owner 换代/参数变化 → 旧载荷零写入（stale_context）。
    pub async fn respond(
        &self,
        key: &ThreadKey,
        request_key: &str,
        context_token: &str,
        action: PendingAction,
    ) -> RpcOutcome {
        let envelope_id = super::types::new_uuid();
        let caps = self.current_caps();
        let inflight_key = (key.thread_id.clone(), request_key.to_string());
        // 阶段 1（R5 短锁：只持 mirrors）：stale 门（R4.1）+ 全部 mirror 读 + 镜像
        // 绑定的 owner lease（C3 同代复核锚）
        let snap = {
            let mirrors = self.inner.mirrors.lock().await;
            let Some(mirror) = mirrors.get(key) else {
                return not_sent(&envelope_id, "stale_context");
            };
            if mirror.is_stale() {
                return not_sent(&envelope_id, "stale_context");
            }
            RespondSnapshot::collect(mirror, caps, request_key, context_token)
        };
        // 阶段 2（R5 短锁：只持 inflight）：清理已消失/换 token 的 Blocked + 抢占提交权
        {
            let mut inflight = self.inner.inflight.lock().await;
            let live: std::collections::HashMap<&str, &str> = snap
                .live_pendings
                .iter()
                .map(|(rk, tk)| (rk.as_str(), tk.as_str()))
                .collect();
            inflight.retain(|(tid, rk), entry| {
                !(tid == &key.thread_id
                    && entry.state == InflightState::BlockedUnknown
                    && match live.get(rk.as_str()) {
                        // pending 消失 → 屏障解除
                        None => true,
                        // 重现但换 token（新世界）→ 旧 Unknown 不挡新卡
                        Some(cur) => Some(*cur) != entry.token.as_deref(),
                    })
            });
            if inflight.contains_key(&inflight_key) {
                return not_sent(&envelope_id, "pending_submit_in_progress");
            }
            inflight.insert(
                inflight_key.clone(),
                InflightEntry {
                    state: InflightState::Dispatching,
                    token: None,
                },
            );
        }
        // 校验 + wire 组装（不持任何锁；失败必须退掉提交权）。
        // C3 同代复核锚在 snap 被消耗前取出
        let mirror_lease = snap.mirror_lease.clone();
        let prepared = match snap.into_prepared(key, request_key, context_token, &action) {
            Ok(p) => p,
            Err(reason) => {
                self.inner.inflight.lock().await.remove(&inflight_key);
                return not_sent(&envelope_id, &reason);
            }
        };
        // C3 测试缝：阶段 1/2 与阶段 3 之间可注入等待（生产恒直通）
        //（先绑定 clone 再判——if let 暂存的 MutexGuard 会跨 await，破坏 Send）
        let gate = self
            .inner
            .interphase_gate
            .lock()
            .expect("gate lock")
            .clone();
        if let Some(gate) = gate {
            gate.notified().await;
        }
        // 阶段 3（R5 短锁：只持 owners）：lease 最后复核（响应前读到的过期 lease 不算
        // 授权）+ C3 同代复核——现行 lease 必须就是阶段 1 镜像所绑定的那一代
        let lease_valid = {
            let owners = self.inner.owners.lock().await;
            match current_lease(&owners, key) {
                Some(l) if Some(&l) == mirror_lease.as_ref() => {
                    let v = owners.validity(&l);
                    Some((l, v))
                }
                // 无 lease / 已换代（阶段 1 之后 owner 失效或被接管）：旧载荷零写入
                _ => None,
            }
        };
        let (lease, validity) = match lease_valid {
            Some(v) => v,
            None => {
                self.inner.inflight.lock().await.remove(&inflight_key);
                return not_sent(&envelope_id, "stale_context");
            }
        };
        // 阶段 3.5（C3 短锁：只持 mirrors）：镜像复核——非 stale 且 token 仍绑定当前
        // 上下文（卡还在场、内容/方法未变；流式 revision 前进不影响同 token）
        let reverified = {
            let mirrors = self.inner.mirrors.lock().await;
            mirrors
                .get(key)
                .is_some_and(|m| !m.is_stale() && m.token_valid(request_key, context_token))
        };
        if !reverified {
            self.inner.inflight.lock().await.remove(&inflight_key);
            return not_sent(&envelope_id, "stale_context");
        }
        let outcome = self
            .dispatch(
                &prepared.method,
                prepared.version,
                prepared.params,
                lease,
                validity,
            )
            .await;
        // R4.3 + C3：Accepted/Rejected/NotSent（保守零投递，可重试）释放提交权；
        // UnknownDelivery → 保留为 Blocked（记提交所用 token 分代，状态应用驱动清理）。
        {
            let mut inflight = self.inner.inflight.lock().await;
            if outcome.delivery == Delivery::UnknownDelivery {
                inflight.insert(
                    inflight_key,
                    InflightEntry {
                        state: InflightState::BlockedUnknown,
                        token: Some(context_token.to_string()),
                    },
                );
            } else {
                inflight.remove(&inflight_key);
            }
        }
        outcome
    }

    /// 统一派发：mutating RPC（未知结果绝不自动重试——connection 语义保证）。
    async fn dispatch(
        &self,
        method: &str,
        version: u32,
        params: Value,
        target: OwnerLease,
        validity: tokio::sync::watch::Receiver<bool>,
    ) -> RpcOutcome {
        let conn = self.inner.conn.lock().await.clone();
        let Some(conn) = conn else {
            return not_sent(&super::types::new_uuid(), "disconnected");
        };
        conn.request(RpcRequest {
            method: method.to_string(),
            params,
            version: Some(version),
            target: Some(target),
            mutating: true,
            validity: Some(validity),
        })
        .await
    }

    pub fn shutdown(&self) -> Pin<Box<dyn std::future::Future<Output = ()> + Send + '_>> {
        let inner = self.inner.clone();
        Box::pin(async move {
            let conn = inner.conn.lock().await.clone();
            if let Some(conn) = conn {
                conn.shutdown().await;
            }
        })
    }

    /// C3 测试缝：注入 respond 两阶段间的等待门（生产不调用——恒直通）。
    #[doc(hidden)]
    pub fn set_respond_interphase_gate(&self, gate: Arc<tokio::sync::Notify>) {
        *self.inner.interphase_gate.lock().expect("gate lock") = Some(gate);
    }

    /// B3 测试缝：注入 follow 的 bind/declare 间等待门（生产不调用——恒直通）。
    #[doc(hidden)]
    pub fn set_follow_gate(&self, gate: Arc<tokio::sync::Notify>) {
        *self.inner.follow_gate.lock().expect("gate lock") = Some(gate);
    }

    /// B3 测试缝：任务是否已到达 follow gate。
    #[doc(hidden)]
    pub fn follow_gate_reached(&self) -> bool {
        self.inner.follow_gate_reached.load(Ordering::SeqCst)
    }

    /// C3 测试缝：该 (thread, request_key) 是否已抢占提交权（回归测试固定
    /// 「respond 已过阶段 2、停在测试缝」的时序用）。
    #[doc(hidden)]
    pub fn inflight_claimed(&self, thread_id: &str, request_key: &str) -> bool {
        self.inner
            .inflight
            .try_lock()
            .map(|g| g.contains_key(&(thread_id.to_string(), request_key.to_string())))
            .unwrap_or(false)
    }
}

fn not_sent(envelope_id: &str, reason: &str) -> RpcOutcome {
    RpcOutcome {
        delivery: Delivery::NotSent,
        envelope_id: envelope_id.to_string(),
        result: None,
        handled_by: None,
        reason_code: Some(reason.to_string()),
    }
}

fn current_lease(owners: &OwnerRegistry, key: &ThreadKey) -> Option<OwnerLease> {
    let lease = owners.current(key)?;
    owners.is_current(&lease).then_some(lease)
}

/// respond 的镜像侧快照（阶段 1 的产物；收集于 mirrors 短锁内，之后不持锁使用）。
struct RespondSnapshot {
    /// 能力基线（阶段 1 取自当前动态 profile）
    caps: CapabilitySet,
    /// 当前投影中的 (request_key, context_token) 全集（R4/C3 Blocked 清理依据——
    /// 屏障按完整上下文分代，重现换 token 即解除）
    live_pendings: Vec<(String, String)>,
    /// 当前投影中的本请求卡（存在性 + kind + questions）
    pending: Option<PendingView>,
    /// token 是否绑定当前上下文
    token_valid: bool,
    /// 阶段 1 镜像绑定的 owner lease（C3 同代复核锚：阶段 3 现行 lease 必须与之全同）
    mirror_lease: Option<OwnerLease>,
    /// 服务端保存的原 permissions 回显源（requests[].params.permissions）
    raw_requests: Option<Value>,
    request_id: Option<Value>,
}

impl RespondSnapshot {
    /// 阶段 1：mirrors 短锁内完成全部 mirror 读（R4.1 stale 门已由调用方先行）。
    fn collect(
        mirror: &ThreadMirror,
        caps: CapabilitySet,
        request_key: &str,
        _token: &str,
    ) -> Self {
        let projection = mirror.project(caps);
        let live_pendings = projection
            .pending
            .iter()
            .map(|p| (p.request_key.clone(), p.context_token.clone()))
            .collect();
        let pending = projection
            .pending
            .iter()
            .find(|p| p.request_key == request_key)
            .cloned();
        // requestKey 无损可逆；解析失败按 invalid_id 拒（阶段 2.5 报告）
        let request_id = parse_request_key(request_key)
            .ok()
            .and_then(|id| serde_json::to_value(&id).ok());
        // R1：raw_state 即 conversationState 本体，requests 按根直读
        let raw_requests = mirror
            .raw_state()
            .and_then(|raw| raw.get("requests").cloned());
        Self {
            caps,
            live_pendings,
            pending,
            token_valid: mirror.token_valid(request_key, _token),
            mirror_lease: mirror.lease().cloned(),
            raw_requests,
            request_id,
        }
    }

    /// 阶段 2.5：校验 + wire 组装（不持任何锁）。Err = 拒绝原因（零投递）。
    fn into_prepared(
        self,
        key: &ThreadKey,
        request_key: &str,
        context_token: &str,
        action: &PendingAction,
    ) -> Result<PreparedRespond, String> {
        // pending 现存
        let pending = self.pending.ok_or_else(|| "stale_context".to_string())?;
        // token 绑定当前上下文（过期卡拒绝）
        if !self.token_valid {
            return Err("stale_context".into());
        }
        let request_id_value = self.request_id.ok_or_else(|| "invalid_id".to_string())?;
        let caps = self.caps;
        let (method, version): (&str, u32) = match (&pending.kind, action) {
            (PendingKind::CommandApproval, PendingAction::Approve | PendingAction::Reject) => {
                if !caps.command {
                    return Err("capability_unavailable".into());
                }
                ("thread-follower-command-approval-decision", 1)
            }
            (PendingKind::PermissionsApproval, PendingAction::Approve | PendingAction::Reject) => {
                if !caps.permissions {
                    return Err("capability_unavailable".into());
                }
                ("thread-follower-permissions-request-approval-response", 1)
            }
            (PendingKind::UserInput, PendingAction::Answer(_)) => {
                if !caps.blocking_input {
                    return Err("capability_unavailable".into());
                }
                ("thread-follower-submit-user-input", 1)
            }
            (PendingKind::Unsupported, _) => return Err("unsupported".into()),
            _ => return Err("action_mismatch".into()),
        };
        let approve = matches!(action, PendingAction::Approve);
        let params = match (&pending.kind, action) {
            (PendingKind::CommandApproval, _) => {
                // command 变体：只发 decision 词，绝不携带 permissions 对象
                json!({
                    "conversationId": key.thread_id,
                    "requestId": request_id_value,
                    "decision": if approve { "accept" } else { "decline" },
                })
            }
            (PendingKind::PermissionsApproval, _) => {
                // permissions 变体：批准=回显服务端保存的原 permissions；拒绝={}；scope=turn
                let original = self
                    .raw_requests
                    .and_then(|r| r.as_array().cloned())
                    .and_then(|arr| {
                        arr.iter()
                            .find(|r| {
                                r.get("id")
                                    .map(|id| id == &request_id_value)
                                    .unwrap_or(false)
                            })
                            .cloned()
                    })
                    .and_then(|req| {
                        req.get("params")
                            .and_then(|p| p.get("permissions"))
                            .cloned()
                    });
                let permissions = if approve {
                    original.unwrap_or(json!({}))
                } else {
                    json!({})
                };
                json!({
                    "conversationId": key.thread_id,
                    "requestId": request_id_value,
                    "response": { "permissions": permissions, "scope": "turn" },
                })
            }
            (PendingKind::UserInput, PendingAction::Answer(answers)) => {
                // 按 qid 精确映射：不能漏答/多答/混入未知 qid；isOther=false 禁自由文本
                let questions = &pending.questions;
                if answers.len() != questions.len() {
                    return Err("answers_mismatch".into());
                }
                let mut mapped = serde_json::Map::new();
                for q in questions {
                    let Some(entries) = answers.get(&q.id) else {
                        return Err("answers_mismatch".into());
                    };
                    if entries.is_empty() {
                        return Err("answers_mismatch".into());
                    }
                    if !q.is_other
                        && entries
                            .iter()
                            .any(|e| !q.options.iter().any(|o| &o.label == e))
                    {
                        return Err("answer_not_option".into());
                    }
                    mapped.insert(q.id.clone(), json!({ "answers": entries }));
                }
                json!({
                    "conversationId": key.thread_id,
                    "requestId": request_id_value,
                    "response": { "answers": Value::Object(mapped) },
                })
            }
            _ => return Err("action_mismatch".into()),
        };
        let _ = request_key;
        let _ = context_token;
        Ok(PreparedRespond {
            method: method.to_string(),
            version,
            params,
        })
    }
}

/// wire 组装完成后的派发载荷。
struct PreparedRespond {
    method: String,
    version: u32,
    params: Value,
}

// ============================================================
// R1/R2：broadcast 事件转发（need_snapshot 闭环 / owner 失效 / 迟到帧 /
// following 查询 / 断线重连恢复）。锁纪律（R5）：全程顺序短锁，绝不嵌套。
// ============================================================

/// forward_events 的单飞 spawn。独立非 async 函数：把 forward_events 的 future
/// 类型移出 ensure_connection 的协程内部类型，断开 ensure→forward→recovery→ensure
/// 的递归类型环（否则该环上 auto trait Send 不可计算，编译报 cycle）。
fn spawn_forward_task(adapter: DesktopIpcAdapter, conn: ConnectionHandle) {
    tokio::spawn(async move { forward_events(adapter, conn).await });
}

/// broadcast 事件转发主循环：state/信号处理；断线 → 全体 stale + 发布 → 单飞
/// 交接给重连恢复任务后退出（新连接的事件处理由恢复任务在 follow 前先行就位）。
async fn forward_events(adapter: DesktopIpcAdapter, conn: ConnectionHandle) {
    let mut rx = conn.subscribe();
    loop {
        match rx.recv().await {
            Ok(ConnectionEvent::Broadcast { frame, .. }) => {
                handle_broadcast(&adapter, &conn, frame).await;
            }
            Ok(ConnectionEvent::Initialized { .. }) => continue,
            Ok(ConnectionEvent::Disconnected { .. }) | Err(_) => break,
        }
    }
    // 连接断开：全体 mirror stale + 发布（保留视图只读）
    mark_all_mirrors_stale(&adapter, false).await;
    adapter.inner.forward_alive.store(false, Ordering::SeqCst);
    if adapter.inner.stopping.load(Ordering::SeqCst) {
        return;
    }
    // 单飞交接（R2.5：AtomicBool 防并发恢复；已有恢复在跑则本任务直接退出）
    if adapter.inner.recovering.swap(true, Ordering::SeqCst) {
        return;
    }
    spawn_recovery_task(adapter);
}

/// 恢复任务的单飞 spawn（独立非 async 函数，理由同 [`spawn_forward_task`]）。
fn spawn_recovery_task(adapter: DesktopIpcAdapter) {
    tokio::spawn(async move {
        let recovered = recovery_loop(&adapter).await;
        adapter.inner.recovering.store(false, Ordering::SeqCst);
        if recovered.is_none() {
            // shutdown：转发任务已随断线退出，恢复任务也退出
            adapter.inner.forward_alive.store(false, Ordering::SeqCst);
        }
    });
}

/// 同 socket 恢复的工作队列与消费者资格（第四轮复审 B2）：pending 与 running
/// 同锁。「取空待办」与「释放消费者资格」在 [`Self::take_or_release`] 的**同一
/// 临界区**内完成——任务退出判定与并发入队之间不存在可插入窗口：take 空 ⇒
/// 资格已原子释放，其后到达的 [`Self::enqueue`] 必然拿到资格并启动新任务
///（`pending=[B], running=false` 无消费者的状态不可能出现）。
#[derive(Default)]
struct RecoveryQueue {
    pending: Vec<ThreadKey>,
    running: bool,
}

impl RecoveryQueue {
    /// spawn 侧：并入 keys 并竞争消费者资格；`true` = 本调用者须启动任务。
    fn enqueue(&mut self, keys: impl IntoIterator<Item = ThreadKey>) -> bool {
        self.pending.extend(keys);
        if self.running {
            // 在飞任务会在下一轮 take_or_release 消费（同锁，绝不丢）
            return false;
        }
        self.running = true;
        true
    }

    /// 任务侧：原子取走全部待办；**为空时同时释放消费者资格**（B2 不变量）。
    fn take_or_release(&mut self) -> Option<Vec<ThreadKey>> {
        let keys = std::mem::take(&mut self.pending);
        if keys.is_empty() {
            self.running = false;
            None
        } else {
            Some(keys)
        }
    }

    /// 任务侧中途退出（连接断/停机）：keys 放回待办并释放资格（同一临界区；
    /// 其后的 enqueue 拿到资格即成为新消费者，keys 不丢）。
    fn requeue_and_release(&mut self, keys: impl IntoIterator<Item = ThreadKey>) {
        self.pending.extend(keys);
        self.running = false;
    }
}

/// 2026-10-17 复审 C2 / 第三轮复审 B2：**同 socket 恢复**——owner 失效
///（client-status-changed disconnected）或 `ipc-connection-reset` 后，连接本身
/// 仍活着，连接级恢复循环（由断线驱动）不会触发；若无人重新 discovery/follow，
/// 重新接管的 owner 帧因无 lease 被丢、视图永久 stale。单飞只避免重复工作：
/// 后到失效事件的 keys 在待办锁内并入（绝不丢弃），由在飞任务继续消费。
fn spawn_same_socket_recovery(adapter: DesktopIpcAdapter, keys: Vec<ThreadKey>) {
    if keys.is_empty() || adapter.inner.stopping.load(Ordering::SeqCst) {
        return;
    }
    // B2：入队并竞争消费者资格（同锁原子）——在飞任务存在时直接返回，后到
    // 事件由它继续消费；退出边界瞬间入队的由继任任务接管（RecoveryQueue 契约）
    let start = {
        let mut queue = adapter.inner.recovery_queue.lock().expect("queue lock");
        queue.enqueue(keys)
    };
    if start {
        tokio::spawn(run_same_socket_recovery(adapter));
    }
}

/// 同 socket 恢复主循环（第四轮复审 B2 重构）：每轮 `take_or_release` 取走全部
/// 待办为**一批**；批内失败的 keys 就地退避重试（预算 3 轮，只约束本批——预算
/// 耗尽仅放弃本批，**绝不 clear 共享待办**，等待期间后到的 keys 由下一轮
/// take 消费）；已卸载的 key 由 [`DesktopIpcAdapter::follow_if_following`] 跳过/
/// 回收（B3）。连接断开/停机时未完成 keys 放回队列并释放资格（同临界区）。
async fn run_same_socket_recovery(adapter: DesktopIpcAdapter) {
    loop {
        if adapter.inner.stopping.load(Ordering::SeqCst) {
            // 停机：待办放回（进程退出即随进程消失），资格原子释放
            let mut queue = adapter.inner.recovery_queue.lock().expect("queue lock");
            let leftover = queue.take_or_release();
            if let Some(keys) = leftover {
                queue.requeue_and_release(keys);
            }
            return;
        }
        // 唯一取待办点：空 ⇒ 资格已在同一临界区释放 ⇒ 退出（其后 enqueue 必有
        // 新消费者——B2 原子交接不变量）
        let Some(keys) = ({
            let mut queue = adapter.inner.recovery_queue.lock().expect("queue lock");
            queue.take_or_release()
        }) else {
            break;
        };
        let Some(conn) = current_conn(&adapter).await else {
            // 连接已断：keys 放回待办 + 资格释放（同临界区），让位连接级恢复；
            // 后续事件 enqueue 拿到资格即继续消费
            let mut queue = adapter.inner.recovery_queue.lock().expect("queue lock");
            queue.requeue_and_release(keys);
            return;
        };
        // —— 本批：批内退避重试，预算只约束本批 keys ——
        let mut batch: Vec<ThreadKey> = keys;
        let mut rounds = 0u32;
        loop {
            let mut still_failed: Vec<ThreadKey> = Vec::new();
            for key in &batch {
                if adapter.follow_if_following(&conn, key).await.is_err() {
                    still_failed.push(key.clone());
                }
            }
            if still_failed.is_empty() {
                break;
            }
            rounds += 1;
            if rounds >= 3 {
                // 本批预算耗尽：仅放弃本批失败 keys（与旧约定一致的有界资源）。
                // **共享待办不 clear**——本批等待响应期间后到的 keys 一次都未被
                // 尝试，由循环头 take_or_release 继续消费（B2 回归 2）
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            if adapter.inner.stopping.load(Ordering::SeqCst) {
                let mut queue = adapter.inner.recovery_queue.lock().expect("queue lock");
                queue.requeue_and_release(still_failed);
                return;
            }
            let Some(conn2) = current_conn(&adapter).await else {
                // 连接断：失败 keys 放回待办 + 资格释放，让位连接级恢复
                let mut queue = adapter.inner.recovery_queue.lock().expect("queue lock");
                queue.requeue_and_release(still_failed);
                return;
            };
            let _ = conn2;
            batch = still_failed;
        }
    }
}

/// R2.5 重连恢复循环：退避 1/2/4/8/16/30s（成功归零——本函数成功即返回）；
/// 每轮 ensure_connection → 新连接的事件转发先行就位（swap 幂等防重复）→
/// 对关注集中每个 thread 重新 discover→bind→declare→等 snapshot（B3：经
/// follow_if_following 统一入口——在途退出关注的 key 跳过/回收）。仅退出于
/// shutdown（None）或恢复成功（Some）。
async fn recovery_loop(adapter: &DesktopIpcAdapter) -> Option<ConnectionHandle> {
    let backoffs = [1u64, 2, 4, 8, 16, 30];
    let mut round = 0usize;
    loop {
        if adapter.inner.stopping.load(Ordering::SeqCst) {
            return None;
        }
        if let Ok(conn) = adapter.ensure_connection().await {
            // 新连接的事件处理先行：follow 的 snapshot 由它落进镜像
            if !adapter.inner.forward_alive.swap(true, Ordering::SeqCst) {
                spawn_forward_task(adapter.clone(), conn.clone());
            }
            let keys = followed_keys(adapter).await;
            let mut all_ok = true;
            for key in &keys {
                // B3：恢复统一入口——在途退出关注的 key 跳过/回收，不复活订阅
                if adapter.follow_if_following(&conn, key).await.is_err() {
                    all_ok = false;
                    break;
                }
            }
            if all_ok {
                return Some(conn);
            }
            // 某 thread follow 失败：整轮退避重试（连接可能又断了）
        }
        let delay = backoffs[round.min(backoffs.len() - 1)];
        round += 1;
        tokio::time::sleep(std::time::Duration::from_secs(delay)).await;
    }
}

async fn handle_broadcast(adapter: &DesktopIpcAdapter, conn: &ConnectionHandle, frame: Value) {
    match frame.get("method").and_then(|m| m.as_str()) {
        Some("thread-stream-state-changed") => handle_state_changed(adapter, frame).await,
        Some("client-status-changed") => handle_client_status_changed(adapter, &frame).await,
        // 连接级信号：全体 stale + 发布，并**同 socket 重跟随**（C2：Desktop 侧要求
        // 重置状态时连接未断——只 stale 不重跟随会让视图永久停在旧世界）
        Some("ipc-connection-reset") => {
            mark_all_mirrors_stale(adapter, false).await;
            let keys = followed_keys(adapter).await;
            spawn_same_socket_recovery(adapter.clone(), keys);
        }
        Some("thread-stream-following-status-requested") => {
            handle_following_status(adapter, conn, &frame).await;
        }
        _ => {}
    }
}

/// state-changed：来源核对（R2.4）→ 原子应用 → changed 发布 / need_snapshot 或
/// 非法增量 → stale 发布 + 有界重拉（R1）→ 不完整历史 → 只读 load-complete-history。
async fn handle_state_changed(adapter: &DesktopIpcAdapter, frame: Value) {
    let conversation_id = frame["params"]["conversationId"].as_str().unwrap_or("");
    if conversation_id.is_empty() {
        return;
    }
    let key = ThreadKey {
        host_id: "local".into(),
        thread_id: conversation_id.to_string(),
    };
    let source = frame["sourceClientId"].as_str();
    // 短锁 1：现行 lease（缺失 = 旧 owner 迟到帧，丢弃）
    let lease = {
        let owners = adapter.inner.owners.lock().await;
        owners.current(&key)
    };
    let Some(lease) = lease else { return };
    // R2.4：帧来源必须与现行 lease 一致——旧 owner 迟到帧不得污染新镜像
    if source != Some(lease.client_id.as_str()) {
        return;
    }
    let change = frame["params"]["change"].clone();
    let caps = adapter.current_caps();
    // 短锁 2：原子应用
    let mut fresh: Option<ThreadProjection> = None;
    let mut need_recover = false;
    let mut history_needed = false;
    {
        let mut mirrors = adapter.inner.mirrors.lock().await;
        let Some(mirror) = mirrors.get_mut(&key) else {
            return; // 未跟随的 thread 不建镜像、不投影
        };
        match mirror.apply(&lease, &change) {
            Ok(eff) => {
                if eff.changed {
                    fresh = Some(mirror.project(caps));
                    if !mirror.is_history_complete() {
                        history_needed = true;
                    }
                } else if eff.need_snapshot {
                    // revision 缺口/未知 change 形态/缺 revision：stale + 重拉
                    need_recover = true;
                }
            }
            Err(_) => {
                // 非法/预算超限：apply 内已置 stale；同样触发有界重拉
                need_recover = true;
            }
        }
    }
    if need_recover {
        // 先发布 stale 投影（SSE 不能停在旧视图），再有界重拉
        publish_projection(adapter, &key).await;
        spawn_stale_refollow(adapter.clone(), key.clone(), current_conn(adapter).await);
    }
    if let Some(p) = fresh {
        // C3：状态应用驱动的提交屏障清理——pending 消失或重现换 token 即解除旧
        // Unknown 屏障（不等下一次 respond 入口），新卡立即可答
        cleanup_inflight_for(adapter, &key.thread_id, &p.pending).await;
        let _ = adapter.inner.events.send(p);
        if history_needed {
            maybe_request_complete_history(adapter, &key).await;
        }
    }
}

/// C3：把该 thread 的 BlockedUnknown 提交屏障与当前投影对齐——(request_key, token)
/// 仍与在场卡完全一致才保留；消失/换 token 的旧屏障解除（Dispatching 不动）。
async fn cleanup_inflight_for(
    adapter: &DesktopIpcAdapter,
    thread_id: &str,
    pending: &[PendingView],
) {
    let live: std::collections::HashMap<&str, &str> = pending
        .iter()
        .map(|p| (p.request_key.as_str(), p.context_token.as_str()))
        .collect();
    let mut inflight = adapter.inner.inflight.lock().await;
    inflight.retain(|(tid, rk), entry| {
        !(tid == thread_id
            && entry.state == InflightState::BlockedUnknown
            && match live.get(rk.as_str()) {
                None => true,
                Some(cur) => Some(*cur) != entry.token.as_deref(),
            })
    });
}

/// R1：消费 need_snapshot/apply error 的有界重拉——复用 probe 定案
///（再声明 following 即得新 snapshot），最多 3 次、间隔 500ms；成功即停，
/// 3 次失败保持 stale 并停止（等下次事件/ensure 救活）。
fn spawn_stale_refollow(
    adapter: DesktopIpcAdapter,
    key: ThreadKey,
    conn: Option<ConnectionHandle>,
) {
    let Some(conn) = conn else { return };
    tokio::spawn(async move {
        for attempt in 0..3u32 {
            if attempt > 0 {
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            }
            if adapter.inner.stopping.load(Ordering::SeqCst) || conn.is_closed() {
                return;
            }
            if is_fresh(&adapter, &key).await {
                return; // 已被并发恢复救活
            }
            if declare_following(&conn, &key).await.is_err() {
                continue;
            }
            // 等新 snapshot 落进镜像（正常路径恢复）
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(4);
            loop {
                if is_fresh(&adapter, &key).await {
                    return;
                }
                if std::time::Instant::now() >= deadline {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        }
        // 3 次失败：保持 stale 并停止（等下次事件/ensure）
    });
}

async fn is_fresh(adapter: &DesktopIpcAdapter, key: &ThreadKey) -> bool {
    let mirrors = adapter.inner.mirrors.lock().await;
    mirrors
        .get(key)
        .is_some_and(|m| !m.is_stale() && m.revision().is_some())
}

/// 发布该 thread 的当前投影（短锁读取后发广播）。
async fn publish_projection(adapter: &DesktopIpcAdapter, key: &ThreadKey) {
    let caps = adapter.current_caps();
    let projection = {
        let mirrors = adapter.inner.mirrors.lock().await;
        mirrors.get(key).map(|m| m.project(caps))
    };
    if let Some(p) = projection {
        let _ = adapter.inner.events.send(p);
    }
}

async fn current_conn(adapter: &DesktopIpcAdapter) -> Option<ConnectionHandle> {
    let conn = adapter.inner.conn.lock().await;
    conn.as_ref().filter(|c| !c.is_closed()).cloned()
}

/// R1.4：snapshot 后历史不完整 → v1 只读 load-complete-history（mutating=false，
/// params 带 conversationId 与 hostId），每 follow 至多一次（bind 即清）。
async fn maybe_request_complete_history(adapter: &DesktopIpcAdapter, key: &ThreadKey) {
    let first = {
        let mut requested = adapter.inner.history_requested.lock().await;
        if requested.contains(key) {
            false
        } else {
            requested.insert(key.clone());
            true
        }
    };
    if !first {
        return;
    }
    let Some(conn) = current_conn(adapter).await else {
        adapter.inner.history_requested.lock().await.remove(key);
        return;
    };
    let Ok(version) = conn.version_of("thread-follower-load-complete-history") else {
        return;
    };
    let c = conn.clone();
    let k = key.clone();
    tokio::spawn(async move {
        // 只读：mutating=false、不携带 target/validity（结果只影响后续 snapshot）
        let _ = c
            .request(RpcRequest {
                method: "thread-follower-load-complete-history".into(),
                params: json!({ "conversationId": k.thread_id, "hostId": k.host_id }),
                version: Some(version),
                target: None,
                mutating: false,
                validity: None,
            })
            .await;
    });
}

/// R2.1：owner 广播 disconnected → 清该 client 全部 lease；受影响 thread 的
/// mirror invalidate（视图+token 全清）并逐个发布 stale 投影。
async fn handle_client_status_changed(adapter: &DesktopIpcAdapter, frame: &Value) {
    let Some(client_id) = frame["params"]["clientId"].as_str() else {
        return;
    };
    if frame["params"]["status"].as_str() != Some("disconnected") {
        return;
    }
    // 短锁 A：候选 keys（mirror 存在的 thread）
    let candidates: Vec<ThreadKey> = {
        let mirrors = adapter.inner.mirrors.lock().await;
        mirrors.keys().cloned().collect()
    };
    // 短锁 B：owners——收集受影响 thread + invalidate_client
    let affected: Vec<ThreadKey> = {
        let mut owners = adapter.inner.owners.lock().await;
        let affected: Vec<ThreadKey> = candidates
            .into_iter()
            .filter(|k| {
                owners
                    .current(k)
                    .map(|l| l.client_id == client_id)
                    .unwrap_or(false)
            })
            .collect();
        owners.invalidate_client(client_id);
        affected
    };
    if affected.is_empty() {
        return;
    }
    // 短锁 C：mirror invalidate + 投影；顺带清该 thread 的提交互斥（owner 换代即新世界）
    let caps = adapter.current_caps();
    let mut projections = Vec::new();
    {
        let mut mirrors = adapter.inner.mirrors.lock().await;
        for key in &affected {
            if let Some(m) = mirrors.get_mut(key) {
                m.invalidate();
                projections.push(m.project(caps));
            }
        }
    }
    {
        let mut inflight = adapter.inner.inflight.lock().await;
        inflight.retain(|(tid, _), _| !affected.iter().any(|k| &k.thread_id == tid));
    }
    for p in projections {
        let _ = adapter.inner.events.send(p);
    }
    // 2026-10-17 复审 C2：owner 失效但 socket 仍活着——连接级恢复循环不会触发；
    // 主动重新 discovery/follow 让新 owner 接管（重新接管的帧才能落进镜像），
    // 不依赖手机下次 GET
    spawn_same_socket_recovery(adapter.clone(), affected);
}
/// R2.3：owner 问「谁在跟随」——当前关注集包含该 conversation 才回
/// broadcast thread-stream-following-changed {following:true}（version 按表）。
async fn handle_following_status(
    adapter: &DesktopIpcAdapter,
    conn: &ConnectionHandle,
    frame: &Value,
) {
    let Some(conversation_id) = frame["params"]["conversationId"].as_str() else {
        return;
    };
    let key = ThreadKey {
        host_id: "local".into(),
        thread_id: conversation_id.to_string(),
    };
    let following = {
        let f = adapter.inner.followings.lock().await;
        f.is_following(&key)
    };
    if !following {
        return;
    }
    let Ok(version) = conn.version_of("thread-stream-following-changed") else {
        return;
    };
    let _ = conn
        .broadcast(
            "thread-stream-following-changed",
            version,
            json!({
                "conversationId": conversation_id,
                "hostId": "local",
                "following": true,
            }),
        )
        .await;
}

/// 全部 mirror stale + 发布。`invalidate=true`（owner 失效）清视图与 token；
/// `false`（断线/reset）保留最后视图只读。
async fn mark_all_mirrors_stale(adapter: &DesktopIpcAdapter, invalidate: bool) {
    let caps = adapter.current_caps();
    let mut projections = Vec::new();
    {
        let mut mirrors = adapter.inner.mirrors.lock().await;
        for mirror in mirrors.values_mut() {
            let publish = if invalidate {
                mirror.invalidate();
                true
            } else if !mirror.is_stale() {
                mirror.mark_stale();
                true
            } else {
                false
            };
            if publish {
                projections.push(mirror.project(caps));
            }
        }
    }
    for p in projections {
        let _ = adapter.inner.events.send(p);
    }
}

/// 关注集中的 thread（mirror 键 ∩ 关注集；两锁顺序读取，不嵌套）。
async fn followed_keys(adapter: &DesktopIpcAdapter) -> Vec<ThreadKey> {
    let mirror_keys: Vec<ThreadKey> = {
        let mirrors = adapter.inner.mirrors.lock().await;
        mirrors.keys().cloned().collect()
    };
    let followings = adapter.inner.followings.lock().await;
    mirror_keys
        .into_iter()
        .filter(|k| followings.is_following(k))
        .collect()
}

#[cfg(test)]
mod recovery_queue_tests {
    //! 第四轮复审 B2：RecoveryQueue 交接契约——「取空待办」与「释放消费者资格」
    //! 原子性（同一临界区）与多生产者零丢失。

    use super::*;

    fn key(id: &str) -> ThreadKey {
        ThreadKey {
            host_id: "local".into(),
            thread_id: id.into(),
        }
    }

    /// B2 契约：take 空 = 资格原子释放；随后 enqueue 必然获得新消费者资格，
    /// `pending=[B], running=false`（无消费者）的状态不可能出现。
    #[test]
    fn take_empty_releases_running_atomically_and_later_enqueue_gets_consumer() {
        let mut q = RecoveryQueue::default();
        assert!(q.enqueue([key("A")]), "首个入队者获得消费者资格");
        assert_eq!(q.take_or_release(), Some(vec![key("A")]));
        // 任务处理完（批为空）再 take：空 ⇒ 原子释放资格
        assert_eq!(q.take_or_release(), None);
        assert!(!q.running, "资格已随 take 空原子释放");
        // 退出边界瞬间入队 B：必然拿到资格（新消费者接管），不出现无消费者状态
        assert!(q.enqueue([key("B")]), "资格释放后的入队必须获得新消费者");
        assert_eq!(q.take_or_release(), Some(vec![key("B")]));
        assert_eq!(q.take_or_release(), None);
        assert!(!q.running);
    }

    /// B2 契约：在飞期间入队不获资格（单飞），但 keys 必在待办中等待消费
    ///（任务已把首批 take 走，后到的 B 单独等下一轮）。
    #[test]
    fn enqueue_while_running_waits_in_pending() {
        let mut q = RecoveryQueue::default();
        assert!(q.enqueue([key("A")]));
        assert_eq!(q.take_or_release(), Some(vec![key("A")]), "首批被任务取走");
        assert!(!q.enqueue([key("B")]), "在飞期间入队不获资格");
        assert_eq!(q.pending.len(), 1, "B 必在待办等待消费");
        assert_eq!(q.take_or_release(), Some(vec![key("B")]));
    }

    /// B2 契约：中途退出（连接断/停机）= keys 放回 + 资格释放（同临界区）。
    #[test]
    fn requeue_and_release_keeps_keys_and_frees_consumer() {
        let mut q = RecoveryQueue::default();
        assert!(q.enqueue([key("A")]));
        q.requeue_and_release([key("A"), key("B")]);
        assert!(!q.running);
        assert_eq!(q.pending.len(), 3, "原 A（未消费）+ 放回的 A/B 都在待办");
        assert!(q.enqueue([key("C")]), "资格已释放：后来者接管");
        let taken = q.take_or_release().expect("keys");
        assert_eq!(
            taken.len(),
            4,
            "放回的 keys（原A+放回A/B）与新入队 C 都被一并消费"
        );
    }

    /// B2 压力：多生产者并发入队、单消费者循环 take_or_release——零丢失、零重复、
    /// 终态 running=false 且 pending 空（真实线程交错打在交接窗口上）。
    #[test]
    fn concurrent_producers_single_consumer_lose_nothing() {
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
        use std::sync::Arc;
        const PRODUCERS: usize = 4;
        const PER_PRODUCER: usize = 500;
        let queue = Arc::new(std::sync::Mutex::new(RecoveryQueue::default()));
        let consumed = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let done = Arc::new(AtomicBool::new(false));
        let produced_total = Arc::new(AtomicUsize::new(0));

        let mut handles = Vec::new();
        for p in 0..PRODUCERS {
            let queue = queue.clone();
            let consumed = consumed.clone();
            let produced_total = produced_total.clone();
            handles.push(std::thread::spawn(move || {
                for i in 0..PER_PRODUCER {
                    let k = key(&format!("p{p}-i{i}"));
                    let start = {
                        let mut q = queue.lock().unwrap();
                        q.enqueue([k])
                    };
                    produced_total.fetch_add(1, Ordering::SeqCst);
                    if start {
                        // 本线程获得消费者资格：驱动消费循环至 take 空（原子释放）
                        loop {
                            let consumed_now = {
                                let mut q = queue.lock().unwrap();
                                q.take_or_release()
                            };
                            match consumed_now {
                                Some(keys) => {
                                    let mut c = consumed.lock().unwrap();
                                    c.extend(keys.into_iter().map(|k| k.thread_id));
                                }
                                None => break, // 资格已原子释放；由后续 enqueue 接管
                            }
                        }
                    }
                }
            }));
        }
        // 独立常驻消费者（任意时刻拿到资格就消费）
        let consumer_queue = queue.clone();
        let consumer_consumed = consumed.clone();
        let consumer_done = done.clone();
        let consumer = std::thread::spawn(move || loop {
            if consumer_done.load(Ordering::SeqCst) {
                break;
            }
            let consumed_now = {
                let mut q = consumer_queue.lock().unwrap();
                q.take_or_release()
            };
            match consumed_now {
                Some(keys) => {
                    let mut c = consumer_consumed.lock().unwrap();
                    c.extend(keys.into_iter().map(|k| k.thread_id));
                }
                None => std::thread::sleep(std::time::Duration::from_micros(50)),
            }
        });
        for h in handles {
            h.join().unwrap();
        }
        // 全部生产完成后等消费者排空，再停
        loop {
            let consumed_now = {
                let mut q = queue.lock().unwrap();
                q.take_or_release()
            };
            match consumed_now {
                Some(keys) => {
                    let mut c = consumed.lock().unwrap();
                    c.extend(keys.into_iter().map(|k| k.thread_id));
                }
                None => break,
            }
        }
        done.store(true, Ordering::SeqCst);
        consumer.join().unwrap();

        let produced = produced_total.load(Ordering::SeqCst);
        let mut c = consumed.lock().unwrap();
        assert_eq!(c.len(), produced, "零丢失（含资格交接窗口）");
        c.sort();
        c.dedup();
        assert_eq!(c.len(), produced, "零重复消费");
        let q = queue.lock().unwrap();
        assert!(q.pending.is_empty() && !q.running, "终态无残留无消费者");
    }
}
