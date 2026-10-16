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
    ProtocolError, ThreadKey, ThreadProjection,
};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::{broadcast, Mutex};

/// 连接工厂（生产：endpoint 解析 + transport；测试：注入 ScriptedPeer 的 io）。
pub trait Connector: Send + Sync {
    fn connect(
        &self,
    ) -> Pin<
        Box<
            dyn std::future::Future<Output = Result<super::transport::BoxIo, ProtocolError>>
                + Send
                + '_,
        >,
    >;
}

/// 生产连接器：resolve endpoint → transport::connect。
pub struct ProductionConnector {
    pub resolver: Arc<dyn super::transport::EndpointResolver>,
}

impl Connector for ProductionConnector {
    fn connect(
        &self,
    ) -> Pin<
        Box<
            dyn std::future::Future<Output = Result<super::transport::BoxIo, ProtocolError>>
                + Send
                + '_,
        >,
    > {
        Box::pin(async move {
            let endpoint: super::transport::DesktopEndpoint = self.resolver.resolve()?;
            super::transport::connect(&endpoint).await
        })
    }
}

struct AdapterInner {
    connector: Arc<dyn Connector>,
    profile: ProtocolProfile,
    conn: Mutex<Option<ConnectionHandle>>,
    owners: Mutex<OwnerRegistry>,
    mirrors: Mutex<HashMap<ThreadKey, ThreadMirror>>,
    followings: Mutex<FollowingSet>,
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
                profile,
                conn: Mutex::new(None),
                owners: Mutex::new(OwnerRegistry::default()),
                mirrors: Mutex::new(HashMap::new()),
                followings: Mutex::new(FollowingSet::default()),
                epoch: AtomicU64::new(0),
                events: broadcast::channel(256).0,
            }),
        }
    }

    /// 本 build 的能力基线（能力四档的来源）。
    pub fn capabilities(&self) -> CapabilitySet {
        self.inner.profile.capabilities
    }

    /// 订阅投影事件（REST/SSE/队列调度共用）。
    pub fn subscribe(&self) -> broadcast::Receiver<ThreadProjection> {
        self.inner.events.subscribe()
    }

    /// 确保连接存活（每次重连 epoch 递增，不重置为 0）。
    async fn ensure_connection(&self) -> Result<ConnectionHandle, ProtocolError> {
        {
            let conn = self.inner.conn.lock().await;
            if let Some(c) = conn.as_ref() {
                if !c.is_closed() {
                    return Ok(c.clone());
                }
            }
        }
        let io = self.inner.connector.connect().await?;
        // 双检：另一任务可能已重连
        let mut guard = self.inner.conn.lock().await;
        if let Some(c) = guard.as_ref() {
            if !c.is_closed() {
                return Ok(c.clone());
            }
        }
        let epoch = self.inner.epoch.fetch_add(1, Ordering::SeqCst) + 1;
        let conn = super::connection::spawn(io, self.inner.profile.clone(), epoch);
        self.inner.owners.lock().await.reset(epoch);
        // 旧镜像全部标 stale（重新 following 的 snapshot 复活）
        for mirror in self.inner.mirrors.lock().await.values_mut() {
            mirror.mark_stale();
        }
        let forward_adapter = self.clone();
        let forward_conn = conn.clone();
        tokio::spawn(async move { forward_events(forward_adapter, forward_conn).await });
        *guard = Some(conn.clone());
        Ok(conn)
    }

    /// 确保线程已发现 owner、声明 following 并收到 snapshot；返回投影。
    pub async fn ensure_thread(&self, key: &ThreadKey) -> Result<ThreadProjection, ProtocolError> {
        let conn = self.ensure_connection().await?;
        // 已跟随且新鲜：直接投影
        {
            let mirrors = self.inner.mirrors.lock().await;
            if let Some(m) = mirrors.get(key) {
                if !m.is_stale() && m.revision().is_some() {
                    return Ok(m.project(self.inner.profile.capabilities));
                }
            }
        }
        // 名额（短锁）
        {
            let mut f = self.inner.followings.lock().await;
            if !f.follow(key.clone()) {
                return Err(ProtocolError::Capacity("following set full (64)".into()));
            }
        }
        // discovery（await 不持锁）→ 绑定（短锁）→ 建镜像（接 snapshot）→ following
        let owner_id = discover_owner(&conn, key).await?;
        let lease = {
            let mut owners = self.inner.owners.lock().await;
            let lease = bind_lease(&mut owners, key, owner_id, conn.epoch());
            // mirror 必须在 following 声明前就位（snapshot 由事件转发落进来）
            self.inner
                .mirrors
                .lock()
                .await
                .entry(key.clone())
                .or_insert_with(|| ThreadMirror::new(key.clone()));
            lease
        };
        let mut validity = self.inner.owners.lock().await.validity(&lease);
        declare_following(&conn, key).await?;
        // 换代兜底：following 声明后 lease 失效（如 Desktop 断开）则失败重试
        if !*validity.borrow_and_update() {
            return Err(ProtocolError::StaleContext(
                "lease lost during follow".into(),
            ));
        }
        wait_first_snapshot(&conn, key).await?;
        let _ = &mut validity;
        self.wait_fresh_projection(key).await?;
        Ok(self.projection(key).await)
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
        let mirrors = self.inner.mirrors.lock().await;
        if let Some(m) = mirrors.get(key) {
            return m.project(self.inner.profile.capabilities);
        }
        ThreadProjection {
            thread_id: key.thread_id.clone(),
            revision: None,
            stale: true,
            availability: ControlAvailability::CompatOnly,
            capabilities: self.inner.profile.capabilities,
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
        match self.inner.mirrors.try_lock() {
            Ok(mirrors) => mirrors
                .get(key)
                .map(|m| {
                    matches!(
                        m.project(self.inner.profile.capabilities)
                            .runtime_status
                            .as_str(),
                        "active" | "unknown"
                    )
                })
                .unwrap_or(false),
            Err(_) => true,
        }
    }

    /// 退出跟随（详情卸载；释放名额，镜像保留为 stale 只读）。
    pub async fn unfollow(&self, key: &ThreadKey) {
        self.inner.followings.lock().await.unfollow(key);
        if let Some(m) = self.inner.mirrors.lock().await.get_mut(key) {
            m.mark_stale();
        }
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
        if !self.inner.profile.capabilities.send {
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
    pub async fn respond(
        &self,
        key: &ThreadKey,
        request_key: &str,
        context_token: &str,
        action: PendingAction,
    ) -> RpcOutcome {
        let envelope_id = super::types::new_uuid();
        // 短锁内完成全部校验并组装 wire 载荷
        let prepared = {
            let mirrors = self.inner.mirrors.lock().await;
            let owners = self.inner.owners.lock().await;
            build_respond(
                &mirrors,
                &owners,
                self.inner.profile.capabilities,
                key,
                request_key,
                context_token,
                &action,
            )
        };
        let (method, version, params, lease, validity) = match prepared {
            Ok(v) => v,
            Err(reason) => return not_sent(&envelope_id, &reason),
        };
        self.dispatch(&method, version, params, lease, validity)
            .await
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

/// 派发前最后复核 + wire 组装。Err = 拒绝原因（零投递）。
#[allow(clippy::type_complexity)]
fn build_respond(
    mirrors: &HashMap<ThreadKey, ThreadMirror>,
    owners: &OwnerRegistry,
    caps: CapabilitySet,
    key: &ThreadKey,
    request_key: &str,
    context_token: &str,
    action: &PendingAction,
) -> Result<
    (
        String,
        u32,
        Value,
        OwnerLease,
        tokio::sync::watch::Receiver<bool>,
    ),
    String,
> {
    // 1) 线程现行 + pending 现存
    let Some(mirror) = mirrors.get(key) else {
        return Err("stale_context".into());
    };
    let projection = mirror.project(caps);
    let Some(pending) = projection
        .pending
        .iter()
        .find(|p| p.request_key == request_key)
    else {
        return Err("stale_context".into());
    };
    // 2) token 绑定当前上下文（过期卡拒绝）
    if !mirror.token_valid(request_key, context_token) {
        return Err("stale_context".into());
    }
    let request_id = parse_request_key(request_key).map_err(|_| "invalid_id".to_string())?;
    let request_id_value =
        serde_json::to_value(&request_id).map_err(|_| "invalid_id".to_string())?;
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
            let original = mirror
                .raw_state()
                .and_then(|raw| raw.get("conversationState"))
                .and_then(|cs| cs.get("requests"))
                .and_then(|r| r.as_array())
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
    // 3) lease 最后复核（响应前读到的过期 lease 不算授权）
    let lease = current_lease(owners, key).ok_or_else(|| "stale_context".to_string())?;
    let validity = owners.validity(&lease);
    Ok((method.to_string(), version, params, lease, validity))
}

/// broadcast 事件转发：state-changed → mirror → 投影广播（短锁，不跨 await 持锁）。
async fn forward_events(adapter: DesktopIpcAdapter, conn: ConnectionHandle) {
    let mut rx = conn.subscribe();
    loop {
        match rx.recv().await {
            Ok(ConnectionEvent::Broadcast { frame, .. }) => {
                if frame.get("method").and_then(|m| m.as_str())
                    != Some("thread-stream-state-changed")
                {
                    continue;
                }
                let conversation_id = frame["params"]["conversationId"]
                    .as_str()
                    .unwrap_or("")
                    .to_string();
                if conversation_id.is_empty() {
                    continue;
                }
                let change = frame["params"]["change"].clone();
                let key = ThreadKey {
                    host_id: "local".into(),
                    thread_id: conversation_id,
                };
                // 短锁 1：取现行 lease
                let lease = {
                    let owners = adapter.inner.owners.lock().await;
                    owners.current(&key)
                };
                let Some(lease) = lease else { continue };
                // 短锁 2：应用并投影
                let mut mirrors = adapter.inner.mirrors.lock().await;
                let Some(mirror) = mirrors.get_mut(&key) else {
                    continue;
                };
                if let Ok(eff) = mirror.apply(&lease, &change) {
                    if eff.changed {
                        let projection = mirror.project(adapter.inner.profile.capabilities);
                        let _ = adapter.inner.events.send(projection);
                    }
                }
            }
            Ok(ConnectionEvent::Disconnected { .. }) | Err(_) => return,
            Ok(ConnectionEvent::Initialized { .. }) => continue,
        }
    }
}
