//! Codex Control（Task 7）账本/路由集成测试：持久动作账本、普通消息 FIFO 与安全路由。
//!
//! 全部走内存库（`DeviceStore::Owned(Arc<Mutex<Connection>>)`）与假派发器
//! （[`FakeDispatch`]），绝不接触真实 `~/.tuvis` / `~/.codex`，绝不启动真实 Tuvis
//! 进程或官方 Desktop。进程重启用「同一连接句柄 + 新建 Router/Store」模拟。

use multi_agents_manager_lib::inject::headless::codex::QueueEvidence;
use multi_agents_manager_lib::remote::pairing::{
    persist_device, revoke_device, DeviceStore, NewDevice,
};
use multi_agents_manager_lib::services::codex_control::{
    payload_hash, reconcile_compat_fence_with, wire_text_hash, ActionContext, ActionKind,
    ActionPayload, ActionState, ActionStore, BarrierRelease, CodexControlRouter, CompatFence,
    ControlRoute, Delivery, DesktopDispatch, DispatchOutcome, PendingAction, RouterOutcome,
    SendGate, StoreError,
};
use multi_agents_manager_lib::services::codex_desktop_ipc::types::{
    ActionReceipt, CodexActionView, ProtocolError, ThreadKey, ThreadProjection,
};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

// ============================================================
// 测试基建：内存库 + 设备 + 假派发器
// ============================================================

type SharedConn = Arc<Mutex<rusqlite::Connection>>;

/// 打开共享内存库（schema::init → migration::migrate，与 DeviceStore::memory 同序，
/// 但保留 Arc 以便模拟「进程重启仍在同一 DB 上」）。
fn shared_conn() -> SharedConn {
    let conn = rusqlite::Connection::open_in_memory().expect("open memory db");
    multi_agents_manager_lib::database::schema::init(&conn);
    multi_agents_manager_lib::database::migration::migrate(&conn).expect("migrate");
    Arc::new(Mutex::new(conn))
}

/// 设置「远程消息带设备签名」（settings KV；缺省关）。
fn set_signature(conn: &SharedConn, on: bool) {
    conn.lock()
        .unwrap()
        .execute(
            "INSERT INTO settings (key, value) VALUES ('remote_message_signature', ?1) \
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            [if on { "on" } else { "off" }],
        )
        .expect("set signature");
}

fn seed_device(conn: &SharedConn, id: &str, name: &str) {
    let dev = NewDevice {
        id: id.to_string(),
        name: name.to_string(),
        ua: format!("ua-{id}"),
        origin_ip: format!("127.0.0.1#{id}"),
        via: "lan".into(),
        // 恒用当前时刻：device_valid 按 180 天 TTL 判活，固定旧时间会被判吊销
        paired_at: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0),
    };
    let store = DeviceStore::Owned(conn.clone());
    store.with(|c| persist_device(c, &dev).expect("seed device"));
}

#[derive(Clone, Debug)]
struct SendRecord {
    action_id: String,
    composed: String,
}

/// 假派发器：记录 A 路派发输入（组合后的文本），返回预设 Rpc 结果。
/// `terminal_turns` = 2026-10-17 复审 C1 的 Turn 终态证据面（镜像中哪些 turnId 已以
/// 终态在场）——release 判定必须先经此证据，缺省空集 = 无证据（保守不释放）。
/// 写边界撤销检查缝的类型别名（clippy type_complexity：双层 Arc + dyn Fn 收口）
type RevokeCheck = Arc<Mutex<Option<Arc<dyn Fn(&str) -> bool + Send + Sync>>>>;

struct FakeDispatch {
    busy: AtomicBool,
    send_outcome: Mutex<DispatchOutcome>,
    respond_outcome: Mutex<DispatchOutcome>,
    sends: Mutex<Vec<SendRecord>>,
    responds: Mutex<Vec<(ThreadKey, String, String)>>,
    terminal_turns: Mutex<std::collections::HashSet<String>>,
    /// 派发门（PR #165 事项①测试）：Some = send_text 在门打开前挂起（模拟分钟级/
    /// RPC 级在飞派发，guard 由其持有），None = 立即返回。
    send_gate: Mutex<Option<Arc<tokio::sync::Notify>>>,
    /// 写前忙（模拟 adapter 在 ensure 后、真正 start-turn 前判定 busy）：send_text
    /// **不记录** start-turn 调用，只返回 `NotSent(busy_before_write)`。
    busy_before_write: AtomicBool,
    /// 写前忙检查次数（对应 adapter 的 pre_write_snapshot 调用）。
    pre_write_checks: Mutex<usize>,
    /// S2 发送守卫注入缝（flush/dispatch 的 send_gate 消费）：Some = 覆盖默认判定。
    gate_override: Mutex<Option<SendGate>>,
    /// S3 迟到结果缝：envelope → 迟到回执（`deliver_late` 投放；`wait_late_result`
    /// 在收尾预算内轮询消费——模拟 RPC 超时后迟到的有效响应）。
    /// Arc 化：`wait_late_result` 的 'static future 捕获克隆（FakeDispatch 非 Clone）。
    late_results: Arc<Mutex<std::collections::HashMap<String, DispatchOutcome>>>,
    /// S3：wait_late_result 轮询间隔（受控时间注参；缺省 20ms）。
    late_poll_ms: Arc<Mutex<u64>>,
    /// S4 写出边界撤销检查缝（生产 = AdapterDispatch::send_text 在 start-turn 前
    /// 查 `codex_write_revoke`；测试 = 注入同源判定闭包）。None = 不检查。
    /// 判定点在探测等待（gate）之后、真正写出之前——与生产「ensure → 写边界 →
    /// start-turn」的位次同构（停在探测时被撤销的旧编排恢复后不得写出）。
    revoke_check: RevokeCheck,
    /// S4 真实写出计数（通过写边界之后的 start-turn 次数；send_count = 尝试数）。
    /// Arc 化：send_text 的 'static future 需要跨 await 计数。
    writes: Arc<Mutex<usize>>,
    /// T04：respond 门（模拟"已发出、回执未回"的在途状态）。
    respond_gate: Mutex<Option<Arc<tokio::sync::Notify>>>,
}

impl FakeDispatch {
    fn new(outcome: DispatchOutcome) -> Self {
        Self {
            busy: AtomicBool::new(false),
            send_outcome: Mutex::new(outcome.clone()),
            respond_outcome: Mutex::new(outcome),
            sends: Mutex::new(Vec::new()),
            responds: Mutex::new(Vec::new()),
            terminal_turns: Mutex::new(std::collections::HashSet::new()),
            send_gate: Mutex::new(None),
            busy_before_write: AtomicBool::new(false),
            pre_write_checks: Mutex::new(0),
            gate_override: Mutex::new(None),
            late_results: Arc::new(Mutex::new(std::collections::HashMap::new())),
            late_poll_ms: Arc::new(Mutex::new(20)),
            revoke_check: Arc::new(Mutex::new(None)),
            writes: Arc::new(Mutex::new(0)),
            respond_gate: Mutex::new(None),
        }
    }

    fn accepted() -> Self {
        Self::new(DispatchOutcome {
            delivery: Delivery::Accepted,
            envelope_id: Some("env-1".into()),
            turn_id: Some("turn-1".into()),
            reason_code: None,
            revision: None,
        })
    }

    /// Accepted + 指定回执 turnId 与投影 revision 基线（R6.3 完成证据判定用）。
    fn accepted_turn(turn_id: &str, revision: Option<u64>) -> Self {
        Self::new(DispatchOutcome {
            delivery: Delivery::Accepted,
            envelope_id: Some("env-1".into()),
            turn_id: Some(turn_id.into()),
            reason_code: None,
            revision,
        })
    }

    fn not_sent(reason: &str) -> Self {
        Self::new(DispatchOutcome {
            delivery: Delivery::NotSent,
            envelope_id: Some("env-2".into()),
            turn_id: None,
            reason_code: Some(reason.into()),
            revision: None,
        })
    }

    /// Accepted + 派发门：send_text 在 [`Self::release_send`] 前挂起——模拟「A 路派发
    /// 在飞（guard 由 T1 的 send 持到落账）」的慢派发形态（PR #165 事项①）。
    fn accepted_gated(gate: Arc<tokio::sync::Notify>) -> Self {
        let d = Self::accepted();
        *d.send_gate.lock().unwrap() = Some(gate);
        d
    }

    /// 开门放行所有挂起中的 send_text
    fn release_send(&self) {
        if let Some(g) = self.send_gate.lock().unwrap().as_ref() {
            g.notify_one();
        }
    }

    /// 撤门：后续 send_text 立即返回（接力阶段恢复正常派发——门只模拟第一次在飞，
    /// 否则接力的第二个派发会再次撞门挂起）
    fn disarm_send(&self) {
        *self.send_gate.lock().unwrap() = None;
    }

    fn set_busy(&self, b: bool) {
        self.busy.store(b, Ordering::SeqCst);
    }

    /// 模拟「router 判 idle 后、send_text 的 ensure 返回 active」：写前检查返回忙，
    /// 本次不产生任何 start-turn 调用。
    fn set_busy_before_write(&self, b: bool) {
        self.busy_before_write.store(b, Ordering::SeqCst);
    }

    fn pre_write_check_count(&self) -> usize {
        *self.pre_write_checks.lock().unwrap()
    }

    /// C1：登记一个已以终态在场的 turnId（镜像侧完成证据）
    fn set_terminal_turn(&self, turn_id: &str) {
        self.terminal_turns
            .lock()
            .unwrap()
            .insert(turn_id.to_string());
    }

    /// C1：撤销终态证据（turn 回到 inProgress / 不在场）
    fn unset_terminal_turn(&self, turn_id: &str) {
        self.terminal_turns.lock().unwrap().remove(turn_id);
    }

    /// 运行期替换 A 路派发结果（构造器已有初始值；本方法供分阶段用例切换）。
    fn set_outcome(&self, out: DispatchOutcome) {
        *self.send_outcome.lock().unwrap() = out;
    }

    fn send_count(&self) -> usize {
        self.sends.lock().unwrap().len()
    }

    /// S4：真实写出计数（通过写边界之后的 start-turn 次数）。
    fn writes(&self) -> usize {
        *self.writes.lock().unwrap()
    }

    fn respond_count(&self) -> usize {
        self.responds.lock().unwrap().len()
    }

    fn recorded_composed(&self) -> Vec<String> {
        self.sends
            .lock()
            .unwrap()
            .iter()
            .map(|s| s.composed.clone())
            .collect()
    }
}

impl FakeDispatch {
    /// S2 测试缝：注入发送守卫三态（flush/dispatch 消费）。
    fn set_send_gate(&self, g: SendGate) {
        *self.gate_override.lock().unwrap() = Some(g);
    }

    /// S3 测试缝：投递迟到回执（在 `wait_late_result` 预算内被消费）。
    fn deliver_late(&self, envelope: &str, out: DispatchOutcome) {
        self.late_results
            .lock()
            .unwrap()
            .insert(envelope.to_string(), out);
    }

    /// S3 测试缝：收窄迟到轮询间隔（受控时间）。
    fn set_late_poll_ms(&self, ms: u64) {
        *self.late_poll_ms.lock().unwrap() = ms;
    }

    /// S4 测试缝：注入写出边界撤销判定闭包（与生产 AdapterDispatch 同位同源判定）。
    fn set_write_revoke_check(&self, check: Arc<dyn Fn(&str) -> bool + Send + Sync>) {
        *self.revoke_check.lock().unwrap() = Some(check);
    }
}

impl DesktopDispatch for FakeDispatch {
    fn thread_busy(&self, _key: &ThreadKey) -> bool {
        self.busy.load(Ordering::SeqCst)
    }
    /// S2：busy 原子量与显式注入共同决定守卫三态（busy 优先级低于显式注入）。
    fn send_gate(&self, _key: &ThreadKey) -> SendGate {
        if let Some(g) = self.gate_override.lock().unwrap().clone() {
            return g;
        }
        if self.busy.load(Ordering::SeqCst) {
            return SendGate::Busy;
        }
        SendGate::Clear
    }

    /// S3：迟到结果轮询（预算内查注入缝；消费即取走——同一证据只收敛一次）。
    fn wait_late_result(
        &self,
        envelope_id: &str,
        _client_message_id: &str,
        deadline: multi_agents_manager_lib::services::codex_desktop_ipc::budget::Deadline,
    ) -> multi_agents_manager_lib::services::codex_control::BoxLateResultFut {
        let late = self.late_results.clone();
        let poll_ms = self.late_poll_ms.clone();
        let env = envelope_id.to_string();
        Box::pin(async move {
            let poll = {
                let ms = *poll_ms.lock().unwrap();
                std::time::Duration::from_millis(ms)
            };
            loop {
                if let Some(out) = late.lock().unwrap().remove(&env) {
                    return Some(out);
                }
                if deadline.is_expired() {
                    return None;
                }
                let wait = deadline.remaining().map(|r| r.min(poll)).unwrap_or(poll);
                tokio::time::sleep(wait).await;
            }
        })
    }

    /// C1：绑定 turnId 的终态证据（测试显式登记；未登记 = 无证据 = 不释放）
    fn turn_evidence_terminal(&self, _key: &ThreadKey, turn_id: &str) -> bool {
        self.terminal_turns.lock().unwrap().contains(turn_id)
    }

    fn send_text(
        &self,
        _key: &ThreadKey,
        action_id: &str,
        composed: &str,
        client_message_id: &str,
        _deadline: multi_agents_manager_lib::services::codex_desktop_ipc::budget::Deadline,
    ) -> multi_agents_manager_lib::services::codex_control::BoxDispatchFut {
        let _ = client_message_id; // P3-3 后恢复行派发被闸，无对照读者；保留参数形
        *self.pre_write_checks.lock().unwrap() += 1;
        if self.busy_before_write.load(Ordering::SeqCst) {
            // 生产语义：忙时**没有** start-turn 调用（sends 不记录）
            return Box::pin(async {
                DispatchOutcome {
                    delivery: Delivery::NotSent,
                    envelope_id: None,
                    turn_id: None,
                    reason_code: Some(
                        multi_agents_manager_lib::services::codex_control::BUSY_BEFORE_WRITE_REASON
                            .to_string(),
                    ),
                    revision: None,
                }
            });
        }
        self.sends.lock().unwrap().push(SendRecord {
            action_id: action_id.to_string(),
            composed: composed.to_string(),
        });
        let out = self.send_outcome.lock().unwrap().clone();
        let gate = self.send_gate.lock().unwrap().clone();
        let revoke_slot = self.revoke_check.clone();
        let writes_counter = self.writes.clone();
        let action = action_id.to_string();
        Box::pin(async move {
            // 探测/在飞等待（生产 = ensure 阶段；停在探测的人工继续场景）
            if let Some(g) = gate {
                g.notified().await;
            }
            // S4 写出边界（真正写出前最后一刻）：槽位**晚绑定**读取（撤销可能在
            // 本派发停在探测期间落账——「停在探测时人工继续」场景）
            let revoked = revoke_slot
                .lock()
                .unwrap()
                .as_ref()
                .is_some_and(|f| f(&action));
            if revoked {
                return DispatchOutcome {
                    delivery: Delivery::UnknownDelivery,
                    envelope_id: None,
                    turn_id: None,
                    reason_code: Some(
                        multi_agents_manager_lib::services::codex_control::MANUAL_RESUME_REVOKED_REASON
                            .to_string(),
                    ),
                    revision: None,
                };
            }
            *writes_counter.lock().unwrap() += 1;
            out
        })
    }

    fn respond(
        &self,
        key: &ThreadKey,
        request_key: &str,
        context_token: &str,
        _action: &PendingAction,
        _action_id: &str,
    ) -> multi_agents_manager_lib::services::codex_control::BoxDispatchFut {
        self.responds.lock().unwrap().push((
            key.clone(),
            request_key.to_string(),
            context_token.to_string(),
        ));
        let out = self.respond_outcome.lock().unwrap().clone();
        Box::pin(async move { out })
    }

    /// T04：带前台预算的派发——respond 门打开前挂起（模拟"写出后回执未回"）。
    fn respond_bounded(
        &self,
        key: &ThreadKey,
        request_key: &str,
        context_token: &str,
        _action: &PendingAction,
        _action_id: &str,
        _deadline: multi_agents_manager_lib::services::codex_desktop_ipc::budget::Deadline,
    ) -> multi_agents_manager_lib::services::codex_control::BoxDispatchFut {
        self.responds.lock().unwrap().push((
            key.clone(),
            request_key.to_string(),
            context_token.to_string(),
        ));
        let out = self.respond_outcome.lock().unwrap().clone();
        let gate = self.respond_gate.lock().unwrap().clone();
        Box::pin(async move {
            if let Some(gate) = gate {
                gate.notified().await;
            }
            out
        })
    }
}

struct Harness {
    conn: SharedConn,
    store: Arc<ActionStore>,
    dispatch: Arc<FakeDispatch>,
    /// S3：Arc 化——新预算用例需要把 router future 放进 tokio::spawn（'static）。
    router: Arc<CodexControlRouter>,
    key: ThreadKey,
}

const THREAD: &str = "0f1e2d3c-1111-4222-8333-444455556666";

fn harness() -> Harness {
    harness_with(FakeDispatch::accepted())
}

fn harness_with(dispatch: FakeDispatch) -> Harness {
    let conn = shared_conn();
    seed_device(&conn, "dev-1", "iPhone");
    let store = Arc::new(ActionStore::new(DeviceStore::Owned(conn.clone())));
    let dispatch = Arc::new(dispatch);
    let router = Arc::new(CodexControlRouter::new(store.clone(), dispatch.clone()));
    Harness {
        conn,
        store,
        dispatch,
        router,
        key: ThreadKey {
            host_id: "local".into(),
            thread_id: THREAD.into(),
        },
    }
}

impl Harness {
    /// 模拟进程重启：同一 DB 连接上重建 Store/Router（recover_reserved 在 Store 构造时执行）。
    fn restart(&mut self) {
        self.store = Arc::new(ActionStore::new(DeviceStore::Owned(self.conn.clone())));
        self.dispatch = Arc::new(FakeDispatch::accepted());
        self.router = Arc::new(CodexControlRouter::new(
            self.store.clone(),
            self.dispatch.clone(),
        ));
    }

    /// 同上，但替换派发器（重启后 A 路行为变化）。
    fn restart_with(&mut self, dispatch: FakeDispatch) {
        self.store = Arc::new(ActionStore::new(DeviceStore::Owned(self.conn.clone())));
        self.dispatch = Arc::new(dispatch);
        self.router = Arc::new(CodexControlRouter::new(
            self.store.clone(),
            self.dispatch.clone(),
        ));
    }

    fn ctx(&self, action_id: &str, kind: ActionKind) -> ActionContext {
        ActionContext {
            action_id: action_id.to_string(),
            device_id: "dev-1".into(),
            device_name: "iPhone".into(),
            kind,
            key: self.key.clone(),
        }
    }

    /// 直读动作行状态（绕过 router，观察账本真值）。
    fn state_of(&self, action_id: &str) -> ActionState {
        self.view_of(action_id).state
    }

    fn view_of(&self, action_id: &str) -> CodexActionView {
        self.store
            .get_for_device(action_id, "dev-1")
            .expect("store query")
            .expect("row exists")
    }

    /// 直读动作行 route 列。
    fn route_of(&self, action_id: &str) -> String {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT route FROM codex_actions WHERE action_id = ?1",
            [action_id],
            |r| r.get(0),
        )
        .unwrap()
    }
}

fn queued(view: RouterOutcome) -> (String, usize) {
    match view {
        RouterOutcome::Queued {
            action_id,
            position,
        } => (action_id, position),
        other => panic!("expected Queued, got {other:?}"),
    }
}

fn completed(view: RouterOutcome) -> ActionReceipt {
    match view {
        RouterOutcome::Completed(r) => r,
        // S2：UseFallback 变体已删除（Desktop 永不自动转 headless）——不存在第三去向
        RouterOutcome::Queued { .. } => panic!("unexpected Queued"),
    }
}

fn minimal_projection(thread_id: &str, status: &str) -> ThreadProjection {
    projection_with_rev(thread_id, status, 1)
}

fn projection_with_rev(thread_id: &str, status: &str, revision: u64) -> ThreadProjection {
    use multi_agents_manager_lib::services::codex_desktop_ipc::types::{
        CapabilitySet, ContentState, ControlAvailability, ProbeStatus, ProjectedMessage,
    };
    ThreadProjection {
        thread_id: thread_id.to_string(),
        revision: Some(revision),
        stale: false,
        availability: ControlAvailability::Controllable,
        capabilities: CapabilitySet {
            send: true,
            state: true,
            command: false,
            permissions: false,
            blocking_input: false,
            interrupt: false,
        },
        runtime_status: status.to_string(),
        content_state: ContentState::Ready,
        messages: vec![ProjectedMessage {
            id: "m1".into(),
            role: "user".into(),
            kind: "message".into(),
            text: "seed".into(),
            ts: None,
            turn_id: None,
            artifacts: Vec::new(),
            turn_image_count: 0,
            omitted_artifact_count: 0,
            interaction_answer: None,
        }],
        truncated: false,
        history_token: String::new(),
        pending: Vec::new(),
        async_questions: Vec::new(),
        effective_operations:
            multi_agents_manager_lib::services::codex_desktop_ipc::capability::EffectiveOperations::unknown(),
        probe_status: ProbeStatus::idle(),
        queue_paused: false,
        owner_epoch: 0,
        owner_generation: 0,
        local_seq: 0,
        // T02：版本束由远程层发射点盖章（测试夹具不伪造）。
        projection_version: None,
        active_turn_id: None,
    }
}

// ============================================================
// 计划场景 1：同 action 幂等 / 冲突
// ============================================================

/// 同 device/thread/action/payload 返回原状态，RPC write 计数恒 1；
/// queue_only 重复提交同样只入队一次；终端态重复提交不重派发。
#[tokio::test]
async fn same_action_never_sends_twice() {
    let h = harness();
    let ctx = h.ctx("act-1", ActionKind::Send);

    let r1 = completed(
        h.router
            .send(ctx.clone(), "你好".into(), false, false)
            .await,
    );
    assert_eq!(r1.delivery, Delivery::Accepted);
    assert_eq!(h.dispatch.send_count(), 1, "首次派发恰好一次");

    // 同 action_id + 同载荷重放：返回原状态，不再写
    let r2 = completed(
        h.router
            .send(ctx.clone(), "你好".into(), false, false)
            .await,
    );
    assert_eq!(r2.delivery, Delivery::Accepted);
    assert_eq!(r2, r1, "幂等重放返回同一回执");
    assert_eq!(h.dispatch.send_count(), 1, "重放零新增写入");

    // queue_only 重复：仍 Queued 原位，不派发
    let ctx2 = h.ctx("act-2", ActionKind::Send);
    let (id, pos) = queued(
        h.router
            .send(ctx2.clone(), "第二条".into(), true, false)
            .await,
    );
    assert_eq!(id, "act-2");
    assert_eq!(pos, 1);
    let (id2, pos2) = queued(h.router.send(ctx2, "第二条".into(), true, false).await);
    assert_eq!(id2, "act-2");
    assert_eq!(pos2, 1, "重复入队不产生第二个队列条目");
    assert_eq!(h.dispatch.send_count(), 1);
}

/// 相同 action_id 换正文/设备/thread 返回 conflict 且零新增写入。
#[tokio::test]
async fn conflicting_action_id_reuse_is_conflict_and_zero_writes() {
    let h = harness();
    let ctx = h.ctx("act-c", ActionKind::Send);
    let r = completed(
        h.router
            .send(ctx.clone(), "原文".into(), false, false)
            .await,
    );
    assert_eq!(r.delivery, Delivery::Accepted);

    // 换正文
    let rc = completed(
        h.router
            .send(ctx.clone(), "换成别的正文".into(), false, false)
            .await,
    );
    assert_eq!(rc.delivery, Delivery::Rejected);
    assert_eq!(rc.reason_code.as_deref(), Some("action_conflict"));
    assert_eq!(h.dispatch.send_count(), 1, "冲突零新增写入");

    // 换 thread（同 action_id）
    let mut ctx_other_thread = ctx.clone();
    ctx_other_thread.key = ThreadKey {
        host_id: "local".into(),
        thread_id: "99994444-1111-4222-8333-444455556666".into(),
    };
    let rt = completed(
        h.router
            .send(ctx_other_thread, "原文".into(), false, false)
            .await,
    );
    assert_eq!(rt.reason_code.as_deref(), Some("action_conflict"));
    assert_eq!(h.dispatch.send_count(), 1);

    // 换设备（同 action_id）
    let mut ctx_other_dev = h.ctx("act-c", ActionKind::Send);
    ctx_other_dev.device_id = "dev-2".into();
    ctx_other_dev.device_name = "iPad".into();
    let rd = completed(
        h.router
            .send(ctx_other_dev, "原文".into(), false, false)
            .await,
    );
    assert_eq!(rd.reason_code.as_deref(), Some("action_conflict"));
    assert_eq!(h.dispatch.send_count(), 1);
}

// ============================================================
// 计划场景 2：重启恢复
// ============================================================

/// writer 执行前先 queued→reserved，进程重启将 reserved→unknown，不能回 queued；
/// queued 恢复仍可按序发送（原文/设备/原 client_message_id 保留）。
#[tokio::test]
async fn restart_reserved_becomes_unknown() {
    let mut h = harness();
    // a 排队后手工置 reserved（模拟 writer 已接手、进程崩溃前）
    let a = h.ctx("act-a", ActionKind::Send);
    let (id, _) = queued(h.router.send(a, "第一条".into(), true, false).await);
    assert_eq!(id, "act-a");
    assert!(h.store.reserve("act-a", ControlRoute::DesktopIpc).unwrap());
    assert_eq!(h.state_of("act-a"), ActionState::Reserved);

    // 同线程还有一条 queued
    let b = h.ctx("act-b", ActionKind::Send);
    assert!(matches!(
        h.router.send(b, "第二条".into(), true, false).await,
        RouterOutcome::Queued { .. }
    ));

    // —— 进程重启 ——
    h.restart();

    assert_eq!(
        h.state_of("act-a"),
        ActionState::Unknown,
        "reserved 重启即 unknown"
    );
    let va = h.view_of("act-a");
    let receipt = va.receipt.expect("unknown 动作带回执");
    assert_eq!(receipt.delivery, Delivery::UnknownDelivery);

    // 2026-10-17 复审 P3-3：desktop_ipc 普通消息的恢复同时存**保守屏障**——
    // 原 Turn 可能已收到（RPC 已发出、ack/落账丢失），重启后顺序限制保守在册
    //（无 turn 身份不自动释放；与 finish_with_turn_barrier 的 Unknown 口径一致）
    assert!(
        h.store.has_compat_fence(&h.key).expect("query"),
        "unknown 恢复行存保守屏障"
    );

    // queued 行不受影响（仍可查、原文/设备/原 client_message_id 保留），
    // 但派发被保守屏障闸住：未知结果的动作后不得自动越队
    let head = h
        .store
        .next_queued(&h.key)
        .expect("query")
        .expect("b still queued");
    assert_eq!(head.ctx.action_id, "act-b");
    assert_eq!(head.raw_text, "第二条");
    assert_eq!(head.ctx.device_name, "iPhone");
    assert!(
        !head.client_message_id.is_empty(),
        "恢复保留账本持久化的原 client_message_id"
    );
    // P11（2026-10-31 用户裁决：「不能卡我其他流程」）：屏障**只保护它自己那一行**
    // ——`act-a` 是终态 unknown，本就不在队列里，永不会被重发；`act-b` 内容完全不同，
    // 必须能正常接力。
    // 变更说明：旧断言要求「整队冻结」，那是把单条疑点放大成整个会话停摆（实机
    // 表现为空闲也不发）。现改为验证真正要守的不变量：**act-a 不被重发**。
    let relayed = h.router.flush_thread(&h.key).await;
    assert!(
        relayed.is_some(),
        "历史 unknown 不得冻结队列接力（它自己不在队列、不可能被重发）"
    );
    assert_eq!(
        h.state_of("act-a"),
        ActionState::Unknown,
        "act-a 仍是 unknown，绝不因接力被重发或改状态"
    );
    let sent = h.dispatch.recorded_composed();
    assert_eq!(sent, vec!["第二条"], "派出的是队列里那条 act-b，不是 act-a");

    // 多轮对账仍不解锁"无 turn 身份"的保守屏障本身（C1.2 不变）
    for _ in 0..3 {
        h.router.converge_with_ledger();
    }
    assert!(
        !h.router
            .release_turn_if_complete(&h.key, &projection_with_rev(THREAD, "idle", 999)),
        "无 turn 身份的保守屏障不因任何投影释放"
    );
}

// ============================================================
// 计划场景 3：忙态 FIFO + 审批不堵
// ============================================================

/// active 时连续 a/b 两条消息落 SQLite FIFO，idle 后按序合为一回合；
/// 期间审批能取得短 action 锁响应原 Turn（不等待 Turn 结束）。
#[tokio::test]
async fn busy_fifo_allows_pending_response() {
    let h = harness();
    h.dispatch.set_busy(true);

    let a = h.ctx("fifo-a", ActionKind::Send);
    let b = h.ctx("fifo-b", ActionKind::Send);
    let (ida, posa) = queued(h.router.send(a, "消息甲".into(), false, false).await);
    let (idb, posb) = queued(h.router.send(b, "消息乙".into(), false, false).await);
    assert_eq!(ida, "fifo-a");
    assert_eq!(idb, "fifo-b");
    assert_eq!(posa, 1);
    assert_eq!(posb, 2, "FIFO 位置按入队序");
    assert_eq!(h.dispatch.send_count(), 0, "忙态不派发");

    // 忙态期间审批：短 action 锁响应，不被消息队列堵住
    let actx = h.ctx("appr-1", ActionKind::CommandApproval);
    let rappr = completed(
        h.router
            .respond(actx, "n:12".into(), "tok-1".into(), PendingAction::Approve)
            .await,
    );
    assert_eq!(rappr.delivery, Delivery::Accepted);
    assert_eq!(h.dispatch.respond_count(), 1, "审批不等待 Turn/队列");

    // idle 后合并 a/b
    h.dispatch.set_busy(false);
    let out_a = h.router.flush_thread(&h.key).await.expect("a dispatchable");
    assert_eq!(completed(out_a).delivery, Delivery::Accepted);
    {
        let sends = h.dispatch.sends.lock().unwrap();
        assert_eq!(sends.len(), 1);
        assert_eq!(sends[0].action_id, "fifo-a");
    }
    // b 与 a 同批受理；批次 Turn 未终态时不再开启回合
    assert_eq!(h.state_of("fifo-b"), ActionState::Accepted);
    assert!(
        h.router.flush_thread(&h.key).await.is_none(),
        "Turn 顺序限制未释放前不接力"
    );

    // turn a 终态（fresh idle 投影事件 + 绑定 turn 终态证据）→ 释放顺序限制 → flush
    // 本批次不再重复派发（R6.3：释放由完成证据驱动；C1：证据 = 绑定
    // turnId 本身以终态在场）
    h.dispatch.set_terminal_turn("turn-1");
    assert!(
        h.router
            .release_turn_if_complete(&h.key, &minimal_projection(THREAD, "idle")),
        "fresh idle 投影 + 绑定 turn 终态证据应释放顺序限制"
    );
    assert!(h.router.flush_thread(&h.key).await.is_none());
    let sends = h.dispatch.sends.lock().unwrap();
    assert_eq!(sends.len(), 1);
    assert_eq!(sends[0].composed, "消息甲\n\n消息乙");
}

// ============================================================
// 计划场景 4：fallback 只在派发前
// ============================================================

/// 【S2 重写（原 `fallback_only_before_dispatch`）】旧断言「A 路 NotSent（写前未
/// 投递）→ UseFallback 交接兼容兜底、permit 恰好消费一次」编码的是已废除的
/// 「Desktop 自动转 headless」契约（设计 §1.4：明确 NotSent 可自动转 headless 的
/// 旧口径被取代）。新契约 = **NotSent 是终态回执**（route 保持 DesktopIpc，零交接
/// 零兜底），Accepted/Rejected/UnknownDelivery 语义不变；审批永不降级。
#[tokio::test]
async fn not_sent_is_terminal_receipt_never_falls_back() {
    // NotSent → 终态回执（route=desktop_ipc、原因码保留），零兜底、零交接
    let h = harness_with(FakeDispatch::not_sent("ipc_disconnected"));
    let r = completed(
        h.router
            .send(
                h.ctx("fb-1", ActionKind::Send),
                "降级消息".into(),
                false,
                false,
            )
            .await,
    );
    assert_eq!(r.delivery, Delivery::NotSent);
    assert_eq!(
        r.route,
        ControlRoute::DesktopIpc,
        "Desktop 会话回执不换路由"
    );
    assert_eq!(r.reason_code.as_deref(), Some("ipc_disconnected"));
    assert_eq!(
        h.state_of("fb-1"),
        ActionState::Rejected,
        "NotSent 终态落账"
    );
    assert_eq!(h.dispatch.send_count(), 1, "写尝试恰好一次（无兜底二投）");

    // Accepted/Rejected/Unknown 语义原样（本来就不触发交接）
    for name in ["accepted", "rejected", "unknown"] {
        let delivery = match name {
            "accepted" => Delivery::Accepted,
            "rejected" => Delivery::Rejected,
            _ => Delivery::UnknownDelivery,
        };
        let h = harness_with(FakeDispatch::new(DispatchOutcome {
            delivery,
            envelope_id: Some("env".into()),
            turn_id: None,
            reason_code: None,
            revision: None,
        }));
        let ctx = h.ctx(&format!("nf-{name}"), ActionKind::Send);
        let out = h.router.send(ctx, "普通消息".into(), false, false).await;
        assert!(
            matches!(out, RouterOutcome::Completed(_)),
            "{name} 是明确回执"
        );
        assert_eq!(h.dispatch.send_count(), 1);
    }

    // 审批 NotSent 也不降级
    let h = harness_with(FakeDispatch::not_sent("ipc_disconnected"));
    let actx = h.ctx("appr-nf", ActionKind::PermissionsApproval);
    let r = completed(
        h.router
            .respond(
                actx,
                "s:req-9".into(),
                "tok-2".into(),
                PendingAction::Reject,
            )
            .await,
    );
    assert_eq!(r.delivery, Delivery::NotSent, "审批失败如实回执，不降级");
    assert_eq!(h.dispatch.respond_count(), 1);
}

/// 【S2 重写（原 `profile_mismatch_send_falls_back_with_capability_reason`）】旧断言
/// 「能力不匹配的 NotSent → UseFallback（reason=能力码）→ 兜底」随兜底臂废除。
/// 新契约 = 能力码 NotSent 一样是**终态回执**（Desktop 会话「发到 A 或诚实失败」，
/// 不存在「因能力关而改走兼容」的去向），reason 原样透出供排障。
#[tokio::test]
async fn capability_not_sent_is_terminal_with_reason() {
    let h = harness_with(FakeDispatch::not_sent("capability_unavailable"));
    let ctx = h.ctx("fb-prof", ActionKind::Send);
    let r = completed(h.router.send(ctx, "兼容消息".into(), false, false).await);
    assert_eq!(r.delivery, Delivery::NotSent);
    assert_eq!(r.route, ControlRoute::DesktopIpc);
    assert_eq!(r.reason_code.as_deref(), Some("capability_unavailable"));
    assert_eq!(h.state_of("fb-prof"), ActionState::Rejected);
}

// ============================================================
// 计划场景 5：相同毫秒按 seq
// ============================================================

/// 相同毫秒 a/b 仍按 seq（FIFO 按 seq 不按时间戳；相同 ts 的 DAO 级钉死在 lib 单测）。
#[tokio::test]
async fn same_millisecond_fifo_orders_by_seq() {
    let h = harness();
    let (ida, pa) = queued(
        h.router
            .send(h.ctx("seq-a", ActionKind::Send), "甲".into(), true, false)
            .await,
    );
    let (idb, pb) = queued(
        h.router
            .send(h.ctx("seq-b", ActionKind::Send), "乙".into(), true, false)
            .await,
    );
    assert_eq!(ida, "seq-a");
    assert_eq!(idb, "seq-b");
    assert_eq!(pa, 1);
    assert_eq!(pb, 2);
    // 队首 = 先注册者（seq 升序，与时间戳无关）
    let head = h.store.next_queued(&h.key).expect("query").expect("head");
    assert_eq!(head.ctx.action_id, "seq-a");
}

// ============================================================
// 计划场景 6：撤回竞态与已撤 actionId 重放
// ============================================================

/// 撤回与 reserve 竞态只有一个 CAS 成功；已撤 actionId 再提交不会重发；
/// 撤回保留 actionId 墓碑、清正文。
#[tokio::test]
async fn retract_reserve_race_single_winner_and_no_redispatch() {
    let h = harness();

    // 顺序一：reserve 先赢 → retract 失败（reserved 不可撤）
    h.router
        .send(
            h.ctx("race-a", ActionKind::Send),
            "竞态甲".into(),
            true,
            false,
        )
        .await;
    assert!(h.store.reserve("race-a", ControlRoute::DesktopIpc).unwrap());
    assert!(
        !h.store.retract("race-a", "dev-1").unwrap(),
        "reserved 不可撤"
    );
    assert_eq!(h.state_of("race-a"), ActionState::Reserved);

    // 顺序二：retract 先赢 → reserve 失败；保留墓碑、清正文
    h.router
        .send(
            h.ctx("race-b", ActionKind::Send),
            "竞态乙".into(),
            true,
            false,
        )
        .await;
    assert!(h.store.retract("race-b", "dev-1").unwrap(), "queued 可撤");
    assert!(
        !h.store.reserve("race-b", ControlRoute::DesktopIpc).unwrap(),
        "CAS 失败"
    );
    assert_eq!(h.state_of("race-b"), ActionState::Cancelled);
    let queued_text: Option<String> = {
        let conn = h.conn.lock().unwrap();
        conn.query_row(
            "SELECT queued_text FROM codex_actions WHERE action_id = 'race-b'",
            [],
            |r| r.get(0),
        )
        .unwrap()
    };
    assert_eq!(queued_text, None, "撤回清正文，墓碑保留");

    // 已撤 actionId 再提交：不重发、不再入队
    let rr = completed(
        h.router
            .send(
                h.ctx("race-b", ActionKind::Send),
                "竞态乙".into(),
                false,
                false,
            )
            .await,
    );
    assert_eq!(rr.delivery, Delivery::NotSent);
    assert_eq!(rr.reason_code.as_deref(), Some("action_cancelled"));
    assert_eq!(h.dispatch.send_count(), 0);
}

// ============================================================
// 计划场景 7：吊销设备
// ============================================================

/// 设备被吊销后的 queued 动作拒绝而不派发（零写入），拒绝后队列不永久卡死。
#[tokio::test]
async fn revoked_device_queued_action_refused_not_dispatched() {
    let h = harness();
    let q = h
        .router
        .send(
            h.ctx("rv-a", ActionKind::Send),
            "吊销前排队".into(),
            true,
            false,
        )
        .await;
    assert!(matches!(q, RouterOutcome::Queued { .. }));

    // 吊销设备（走同一连接，模拟服务端吊销）
    DeviceStore::Owned(h.conn.clone()).with(|c| {
        revoke_device(c, "dev-1").expect("revoke");
    });

    // flush：队首动作设备已吊销 → 拒绝落账、不派发
    let out = h.router.flush_thread(&h.key).await;
    assert!(out.is_none(), "无可派发队首（唯一动作被拒）");
    assert_eq!(h.dispatch.send_count(), 0, "吊销设备动作零派发");
    let v = h.view_of("rv-a");
    assert_eq!(v.state, ActionState::Rejected);
    let receipt = v.receipt.expect("refusal receipt");
    assert_eq!(receipt.delivery, Delivery::NotSent);
    assert_eq!(receipt.reason_code.as_deref(), Some("device_revoked"));
}

// ============================================================
// 2026-10-20 复审 P2：签名设置在登记时冻结
// ============================================================

/// 关（默认）＝三条路径都裸正文；开＝恰好一次 `[mobile 花名]`；入队后切设置/重启
/// 不改变既定正文；fallback permit 携带同一份冻结 wire（hash 同源）。
#[tokio::test]
async fn signature_setting_is_frozen_at_registration() {
    let raw = "带签名正文";

    // 默认关：立即派发 → 裸正文（不再恒加标记）
    {
        let h = harness();
        completed(
            h.router
                .send(h.ctx("sig-off", ActionKind::Send), raw.into(), false, false)
                .await,
        );
        assert_eq!(h.dispatch.recorded_composed(), vec![raw.to_string()]);
        assert!(!h.dispatch.recorded_composed()[0].contains("[mobile"));
        // 幂等指纹仍按原文（设置变化不产生 action_conflict）
        let stored_hash: String = {
            let conn = h.conn.lock().unwrap();
            conn.query_row(
                "SELECT payload_hash FROM codex_actions WHERE action_id = 'sig-off'",
                [],
                |r| r.get(0),
            )
            .unwrap()
        };
        assert_eq!(
            stored_hash,
            payload_hash(
                ActionKind::Send,
                &h.key,
                None,
                None,
                &ActionPayload::Text(raw.into())
            )
        );
    }

    // 开启：立即派发 → 恰好一次签名
    {
        let h = harness();
        set_signature(&h.conn, true);
        completed(
            h.router
                .send(h.ctx("sig-on", ActionKind::Send), raw.into(), false, false)
                .await,
        );
        let composed = h.dispatch.recorded_composed();
        assert_eq!(composed, vec![format!("{raw} [mobile iPhone]")]);
        assert_eq!(composed[0].matches("[mobile iPhone]").count(), 1);
    }

    // 入队后切设置 + 重启恢复：仍按**登记时**的签名形态派发（不是重启时设置）
    {
        let mut h = harness();
        set_signature(&h.conn, true);
        h.router
            .send(
                h.ctx("sig-queued", ActionKind::Send),
                raw.into(),
                true,
                false,
            )
            .await;
        set_signature(&h.conn, false);
        // 同 actionId 重试（设置已变）：仍返回既有 queued、零新写入/零重复派发
        let retry = h
            .router
            .send(
                h.ctx("sig-queued", ActionKind::Send),
                raw.into(),
                true,
                false,
            )
            .await;
        assert!(
            matches!(retry, RouterOutcome::Queued { ref action_id, .. } if action_id == "sig-queued"),
            "同 actionId 重试不得换正文/重复投递：{retry:?}"
        );
        h.restart();
        h.router.flush_thread(&h.key).await.expect("dispatch");
        assert_eq!(
            h.dispatch.recorded_composed(),
            vec![format!("{raw} [mobile iPhone]")],
            "排队期间切设置不得改写已登记动作的正文"
        );
    }

    // 旧行升级语义：signature_applied=NULL 的存量 queued 行 = 升级前恒签名
    {
        let h = harness();
        h.conn
            .lock()
            .unwrap()
            .execute(
                "INSERT INTO codex_actions (action_id, device_id, device_name, thread_id, kind, \
                     payload_hash, client_message_id, state, route, queued_text, created_at, updated_at) \
                 VALUES ('sig-legacy', 'dev-1', 'iPhone', ?1, 'send', 'h', 'cm', 'queued', 'none', ?2, 1, 1)",
                rusqlite::params![THREAD, raw],
            )
            .unwrap();
        // 旧行 + 写前忙回队：回填的是已签名 wire，标志必须同步钉住，
        // 否则下次派发会再拼一次（双签名）——2026-10-20 复审 Important。
        h.dispatch.set_busy_before_write(true);
        // 写前忙：flush 返回 None（留在队列等下一事件），不得 panic
        assert!(h.router.flush_thread(&h.key).await.is_none());
        assert_eq!(h.dispatch.send_count(), 0, "写前忙零 start-turn");
        let (text, signed): (Option<String>, Option<i64>) = {
            let conn = h.conn.lock().unwrap();
            conn.query_row(
                "SELECT queued_text, signature_applied FROM codex_actions WHERE action_id = 'sig-legacy'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap()
        };
        assert_eq!(
            text,
            Some(format!("{raw} [mobile iPhone]")),
            "回队保留最终 wire"
        );
        assert_eq!(signed, Some(1), "旧行回队后签名标志必须钉成 1（防双签名）");

        h.dispatch.set_busy_before_write(false);
        h.router.flush_thread(&h.key).await.expect("dispatch");
        assert_eq!(
            h.dispatch.recorded_composed(),
            vec![format!("{raw} [mobile iPhone]")],
            "旧行保持升级前语义（恒带签名且只一次）"
        );
    }

    // 【S2 重写】原「fallback：permit 携带冻结 wire」段随兜底臂删除。NotSent 终态
    // 回执侧：登记时冻结的 wire 就是派发侧实际尝试的正文（dispatch 记录同源）；
    // fence/证据字段作为 reconcile 的持久证据面语义不变（直写仍可设置）。
    {
        let h = harness_with(FakeDispatch::not_sent("ipc_disconnected"));
        set_signature(&h.conn, true);
        let r = completed(
            h.router
                .send(h.ctx("sig-fb", ActionKind::Send), raw.into(), false, false)
                .await,
        );
        assert_eq!(r.delivery, Delivery::NotSent);
        assert_eq!(
            h.dispatch.recorded_composed(),
            vec![format!("{raw} [mobile iPhone]")],
            "A 路实际派发的正文 = 冻结 wire（签名恰好一次）"
        );
        h.store
            .set_compat_fence("sig-fb", Some("compat-msg"), Some("compat-turn"))
            .expect("set fence");
        h.store
            .set_compat_evidence(
                "sig-fb",
                Some("/tmp/rollout.jsonl"),
                Some(64),
                Some(&wire_text_hash(&format!("{raw} [mobile iPhone]"))),
            )
            .expect("set evidence");
        let fences = h.store.compat_fences(&h.key).expect("fences");
        assert_eq!(
            fences[0].wire_hash.as_deref(),
            Some(wire_text_hash(&format!("{raw} [mobile iPhone]"))).as_deref(),
            "fence 证据 hash 与派发 wire 同一份"
        );
    }
}

// ============================================================
// 2026-10-20 复审 P1：写前忙回队 / 旧 Turn 回显
// ============================================================

/// A：router 判 idle 后、send_text 的 ensure 返回 active →
/// start-turn 调用数 0；动作仍排队、正文未丢、兼容派发 0；转闲后仍按原 actionId/正文派发。
#[tokio::test]
async fn busy_after_idle_check_requeues_without_start_turn() {
    let h = harness_with(FakeDispatch::accepted());
    h.dispatch.set_busy_before_write(true);
    let raw = "写前忙的消息";
    let out = h
        .router
        .send(h.ctx("busy-1", ActionKind::Send), raw.into(), false, false)
        .await;
    match out {
        RouterOutcome::Queued { action_id, .. } => assert_eq!(action_id, "busy-1"),
        other => panic!("写前忙必须保持排队：{other:?}"),
    }
    assert_eq!(h.dispatch.send_count(), 0, "零 start-turn 调用");
    assert_eq!(h.dispatch.pre_write_check_count(), 1, "写前复核发生过");
    let (state, text): (String, Option<String>) = {
        let conn = h.conn.lock().unwrap();
        conn.query_row(
            "SELECT state, queued_text FROM codex_actions WHERE action_id = 'busy-1'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap()
    };
    assert_eq!(state, "queued", "动作回到 queued（不消耗）");
    assert_eq!(text.as_deref(), Some(raw), "正文未丢");
    assert!(
        !h.store.has_compat_fence(&h.key).unwrap(),
        "写前忙不得制造兼容 fence/兜底"
    );

    // 转闲后按原 actionId/正文派发
    h.dispatch.set_busy_before_write(false);
    let flushed = h.router.flush_thread(&h.key).await.expect("flush");
    let receipt = completed(flushed);
    assert_eq!(receipt.delivery, Delivery::Accepted);
    assert_eq!(
        h.dispatch.recorded_composed(),
        vec![raw.to_string()],
        "同一 actionId 重派，正文未变"
    );
    assert_eq!(h.state_of("busy-1"), ActionState::Accepted);
}

/// B：start-turn success 返回派发前已存在的旧 Turn ID → 不落 Accepted、不绑定完成
/// Turn、进保守 Unknown 并保持屏障；旧 Turn 完成后仍不派发下一条；fallback 0。
#[tokio::test]
async fn stale_turn_echo_is_unknown_and_keeps_barrier() {
    let h = harness_with(FakeDispatch::accepted());
    h.dispatch.set_outcome(DispatchOutcome {
        delivery: Delivery::UnknownDelivery,
        envelope_id: Some("env-stale".into()),
        turn_id: None,
        reason_code: Some("stale_turn_echo".into()),
        revision: None,
    });
    let raw = "忙态伪成功";
    let receipt = completed(
        h.router
            .send(h.ctx("stale-1", ActionKind::Send), raw.into(), false, false)
            .await,
    );
    assert_eq!(
        receipt.delivery,
        Delivery::UnknownDelivery,
        "旧 Turn 回显不得记为 Accepted"
    );
    assert_eq!(h.state_of("stale-1"), ActionState::Unknown);
    assert!(
        h.store.has_compat_fence(&h.key).unwrap(),
        "保守屏障必须保留"
    );

    // 旧 Turn 以终态在场 + 投影 idle：证据门不认（没有绑定 turnId），不得释放
    h.dispatch.set_terminal_turn("turn-old");
    assert!(
        !h.router
            .release_turn_if_complete(&h.key, &projection_with_rev(THREAD, "idle", 9)),
        "旧 Turn 完成不是本动作的证据"
    );
    // 下一条仍只能排队；零新派发、零兼容
    let out = h
        .router
        .send(
            h.ctx("stale-2", ActionKind::Send),
            "下一条".into(),
            false,
            false,
        )
        .await;
    assert!(
        matches!(out, RouterOutcome::Queued { .. }),
        "下一条必须排队"
    );
    assert_eq!(h.dispatch.send_count(), 1, "零新派发");
    assert!(h
        .store
        .get_for_device("stale-1", "dev-1")
        .unwrap()
        .is_some());
}

// ============================================================

// ============================================================
// 计划场景 9：审计 trigger 回滚
// ============================================================

/// 审计表安装拒绝 INSERT 的 SQLite trigger → reserve 回滚、仍 queued、IPC 与 fallback 写入均 0。
#[tokio::test]
async fn audit_insert_rejection_rolls_back_reserve_and_keeps_queued() {
    let h = harness();
    {
        let conn = h.conn.lock().unwrap();
        conn.execute(
            "CREATE TRIGGER audit_reject_t7 BEFORE INSERT ON write_audit
             BEGIN SELECT RAISE(ABORT, 'audit rejected by test trigger'); END",
            [],
        )
        .unwrap();
    }

    // queue_only 入队（不经审计路径）后 flush：reserve 与审计起始行同事务 → 整体回滚
    h.router
        .send(
            h.ctx("aud-1", ActionKind::Send),
            "审计回滚".into(),
            true,
            false,
        )
        .await;
    let out = h.router.flush_thread(&h.key).await;
    assert!(out.is_none(), "DB 失败零投递（flush 无产出）");
    assert_eq!(h.dispatch.send_count(), 0, "IPC 写入 0");
    assert_eq!(
        h.state_of("aud-1"),
        ActionState::Queued,
        "reserve 回滚后仍 queued"
    );

    // 活动发送路径同样零投递（DB 失败零投递；队首 aud-1 未解锁，FIFO 如实拦住后来者）
    let q = h
        .router
        .send(
            h.ctx("aud-2", ActionKind::Send),
            "活动发送".into(),
            false,
            false,
        )
        .await;
    assert!(
        matches!(q, RouterOutcome::Queued { .. }),
        "队首卡住时后来者只排队"
    );
    assert_eq!(h.dispatch.send_count(), 0);
    assert_eq!(h.state_of("aud-2"), ActionState::Queued);
}

// ============================================================
// 计划场景 10（S2 重写）：NotSent 终态跨重启不重派发
// ============================================================

/// 【S2 重写（原 `fallback_handoff_exactly_once_then_crash_is_unknown`）】旧断言
/// 「handoff 恰好一次 + 交接后崩溃恢复 unknown + route=headless_compat 保留」随
/// 兜底交接删除。新契约 = NotSent 直接终态落账；跨重启重放同 actionId 返回**既有
/// 终态回执**，零新增派发（「投递结果恰好一次」的语义由账本终态承担，不再有
/// 「交接中的 reserved+headless_compat」中间态）。崩溃恢复的 reserved→unknown
/// 语义由 `restart_reserved_becomes_unknown` 继续覆盖。
#[tokio::test]
async fn not_sent_terminal_survives_restart_without_redispatch() {
    let mut h = harness_with(FakeDispatch::not_sent("ipc_disconnected"));
    let r = completed(
        h.router
            .send(
                h.ctx("ho-1", ActionKind::Send),
                "交接消息".into(),
                false,
                false,
            )
            .await,
    );
    assert_eq!(r.delivery, Delivery::NotSent);
    assert_eq!(h.state_of("ho-1"), ActionState::Rejected);
    assert_eq!(h.route_of("ho-1"), "desktop_ipc", "Desktop 会话路由不切");

    // —— 重启：同库重建 Store/Router（新派发器，send 计数从 0 起）——
    h.restart();
    // 重放同 actionId：既有终态回执，零新增派发
    let r = completed(
        h.router
            .send(
                h.ctx("ho-1", ActionKind::Send),
                "交接消息".into(),
                false,
                false,
            )
            .await,
    );
    assert_eq!(r.delivery, Delivery::NotSent, "重放返回账本既有回执");
    assert_eq!(
        h.dispatch.send_count(),
        0,
        "重启后不重派发（新派发器零调用）"
    );
    assert!(
        h.store.next_queued(&h.key).expect("query").is_none(),
        "终态不回队列"
    );
}

// ============================================================
// 计划场景 11：NotSent 清 fence 与队列解锁
// ============================================================

/// 【S2 重写（原 `finish_not_sent_clears_fence_and_unblocks_next`）】旧场景「兜底
/// bridge 设 fence → helper 拒发 → NotSent 同事务清 fence → 队列解锁」随兜底删除。
/// 新契约 = 队首 NotSent **终态出队**、fence 无残留、队列继续流动；fence 在册
/// （未知结果屏障/顺序绑定槽的等价形态）仍然冻结 flush——两段语义分开钉住。
#[tokio::test]
async fn not_sent_head_is_terminal_and_queue_keeps_flowing() {
    let mut h = harness_with(FakeDispatch::not_sent("ipc_disconnected"));
    // 队首直发 → NotSent 终态（route 保持 desktop_ipc）
    let r1 = completed(
        h.router
            .send(
                h.ctx("fence-1", ActionKind::Send),
                "第一条".into(),
                false,
                false,
            )
            .await,
    );
    assert_eq!(r1.delivery, Delivery::NotSent);
    assert_eq!(h.state_of("fence-1"), ActionState::Rejected);
    assert!(
        !h.store.has_compat_fence(&h.key).expect("fence query"),
        "NotSent 终态无屏障残留"
    );

    // 第二条排队；换可用派发器后 flush 接力（队列不被前一条 NotSent 堵死）
    let (id2, _pos) = queued(
        h.router
            .send(
                h.ctx("fence-2", ActionKind::Send),
                "第二条".into(),
                true,
                false,
            )
            .await,
    );
    assert_eq!(id2, "fence-2");
    h.restart_with(FakeDispatch::accepted());
    let out = h
        .router
        .flush_thread(&h.key)
        .await
        .expect("next dispatchable");
    assert_eq!(completed(out).delivery, Delivery::Accepted);
    {
        // clippy await_holding_lock：guard 作用域收进块内，任何 await 前出锁
        let sends = h.dispatch.sends.lock().unwrap();
        assert_eq!(sends.len(), 1);
        assert_eq!(sends[0].action_id, "fence-2", "下一条正常发送");
    }

    // 对照段：fence 在册（未知结果屏障等价形态）时 flush 仍然冻结
    let h2 = harness();
    h2.router
        .send(
            h2.ctx("fz-1", ActionKind::Send),
            "被闸消息".into(),
            true,
            false,
        )
        .await;
    h2.store
        .set_compat_fence("fz-1", Some("cm"), Some("ct"))
        .expect("set fence");
    assert!(
        h2.router.flush_thread(&h2.key).await.is_none(),
        "fence 在册不接力"
    );
    assert_eq!(h2.dispatch.send_count(), 0, "fence 在册零派发");
}

/// crash 残留 fence（动作 reserved）→ 重启恢复 unknown；reserved 态 clear 被 Conflict
/// 拒绝；终态后允许 clear；清 fence 前队列保持闸住。
#[tokio::test]
async fn compat_fence_blocks_and_clear_requires_terminal_state() {
    let mut h = harness();
    h.router
        .send(
            h.ctx("cf-1", ActionKind::Send),
            "被闸消息".into(),
            true,
            false,
        )
        .await;
    // 模拟 bridge：reserve + fence 在册（crash 前状态）
    h.store
        .reserve("cf-1", ControlRoute::HeadlessCompat)
        .unwrap();
    h.store
        .set_compat_fence("cf-1", Some("cm"), Some("ct"))
        .expect("set fence");

    // reserved（未终态）直接 clear 被 Conflict 拒绝
    assert!(matches!(
        h.store.clear_compat_fence("cf-1"),
        Err(StoreError::Conflict)
    ));

    // —— crash 重启：recover → unknown，fence 残留仍闸住队列 ——
    h.restart();
    assert_eq!(h.state_of("cf-1"), ActionState::Unknown);
    assert!(h.store.has_compat_fence(&h.key).expect("fence kept"));
    assert!(
        h.router.flush_thread(&h.key).await.is_none(),
        "证据不足保持留队"
    );
    assert_eq!(h.dispatch.send_count(), 0);

    // 终态后 clear 可用（真实核对在 Task 8 经 reconcile 只读证据决定）
    h.store
        .clear_compat_fence("cf-1")
        .expect("unknown 终态允许 clear");
    assert!(!h.store.has_compat_fence(&h.key).expect("query"));
}

// ============================================================
// 计划场景 12：unknown 阻止重放
// ============================================================

/// 未知操作阻止相同 action 重放：unknown 后同 action_id 再提交返回原 unknown，零写入。
#[tokio::test]
async fn unknown_replay_blocked() {
    let h = harness_with(FakeDispatch::new(DispatchOutcome {
        delivery: Delivery::UnknownDelivery,
        envelope_id: Some("env-u".into()),
        turn_id: None,
        reason_code: None,
        revision: None,
    }));
    let ctx = h.ctx("unk-1", ActionKind::Send);
    let r1 = completed(
        h.router
            .send(ctx.clone(), "未知结果消息".into(), false, false)
            .await,
    );
    assert_eq!(r1.delivery, Delivery::UnknownDelivery);
    assert_eq!(h.dispatch.send_count(), 1);

    // 重放：原状态、零写入
    let r2 = completed(
        h.router
            .send(ctx, "未知结果消息".into(), false, false)
            .await,
    );
    assert_eq!(r2, r1);
    assert_eq!(h.dispatch.send_count(), 1);
    assert_eq!(h.state_of("unk-1"), ActionState::Unknown);
}

// ============================================================
// 计划场景 13：reconcile 只读核对
// ============================================================

/// 只给 idle 证据不解锁；给原消息对应完成证据（唯一 user-input hash + 对应 Turn 终态）
/// 才解锁；队列副本仍在 → 不解锁。
#[tokio::test]
async fn reconcile_requires_completion_evidence_not_idle() {
    let dir = tempfile::tempdir().expect("tempdir");
    // rollout 文件名形态（thread_id_of 权威：rollout* 前缀 + .jsonl + 内嵌 thread UUID）
    let rollout = dir
        .path()
        .join(format!("rollout-2026-10-16T00-00-00-{THREAD}.jsonl"));
    let composed = "第一条 [mobile iPhone]";
    // 基线段：历史旧输入（不同文本）+ 旧 turn 终态
    let base = concat!(
        r#"{"timestamp":"2026-10-16T00:00:00Z","ordinal":1,"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"旧输入"}]}}"#,
        "\n",
        r#"{"timestamp":"2026-10-16T00:00:01Z","ordinal":2,"type":"event_msg","payload":{"type":"task_complete","turn_id":"old"}}"#,
        "\n",
    );
    std::fs::write(&rollout, base).unwrap();
    let baseline = std::fs::metadata(&rollout).unwrap().len();

    let fence = CompatFence {
        action_id: "rc-1".into(),
        queued_message_id: Some("msg-1".into()),
        turn_id: Some("t9".into()),
        rollout_path: Some(rollout.to_string_lossy().to_string()),
        rollout_baseline: Some(baseline),
        wire_hash: Some(wire_text_hash(composed)),
    };
    let idle = minimal_projection(THREAD, "idle");
    let no_queue = |_t: &str, _m: &str| -> Option<QueueEvidence> { None };
    assert!(
        !reconcile_compat_fence_with(&fence, &idle, &no_queue).unwrap(),
        "仅 idle 投影、无追加段 → 不解锁"
    );

    // 追加段：我们的输入 + 对应 turn 完成 → 解锁
    let appended = format!(
        concat!(
            r#"{{"timestamp":"2026-10-16T00:01:00Z","ordinal":3,"type":"event_msg","payload":{{"type":"task_started","turn_id":"t9"}}}}"#,
            "\n",
            r#"{{"timestamp":"2026-10-16T00:01:01Z","ordinal":4,"type":"response_item","payload":{{"type":"message","role":"user","content":[{{"type":"input_text","text":"{composed}"}}]}}}}"#,
            "\n",
            r#"{{"timestamp":"2026-10-16T00:01:05Z","ordinal":5,"type":"event_msg","payload":{{"type":"task_complete","turn_id":"t9"}}}}"#,
            "\n",
        ),
        composed = composed
    );
    {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&rollout)
            .unwrap();
        f.write_all(appended.as_bytes()).unwrap();
    }
    assert!(
        reconcile_compat_fence_with(&fence, &idle, &no_queue).unwrap(),
        "唯一匹配输入 + 对应 Turn 终态 → 解锁"
    );

    // 队列副本仍在（我们的消息未消费）→ 不解锁
    let still_queued = |_t: &str, m: &str| -> Option<QueueEvidence> {
        (m == "msg-1").then_some(QueueEvidence::Ours)
    };
    assert!(!reconcile_compat_fence_with(&fence, &idle, &still_queued).unwrap());
}

/// 读失败、多义（重复文本）、已被尾窗截断、投影归属不一致均返回 false。
#[tokio::test]
async fn reconcile_false_on_ambiguous_truncated_or_read_failure() {
    let dir = tempfile::tempdir().expect("tempdir");
    let composed = "第二条 [mobile iPhone]";
    let proj = minimal_projection(THREAD, "idle");
    let no_queue = |_t: &str, _m: &str| -> Option<QueueEvidence> { None };

    // 读失败：路径不存在（文件名形态合法但文件缺失）
    let fence_missing = CompatFence {
        action_id: "rc-x".into(),
        queued_message_id: None,
        turn_id: None,
        rollout_path: Some(
            dir.path()
                .join(format!("rollout-2026-10-16T00-00-00-{THREAD}.jsonl"))
                .to_string_lossy()
                .to_string(),
        ),
        rollout_baseline: Some(0),
        wire_hash: Some(wire_text_hash(composed)),
    };
    assert!(!reconcile_compat_fence_with(&fence_missing, &proj, &no_queue).unwrap());

    // 多义：基线之后同一文本出现两次
    let rollout = dir
        .path()
        .join(format!("rollout-2026-10-16T00-01-00-{THREAD}.jsonl"));
    let line = |o: u32, turn: &str| {
        format!(
            concat!(
                r#"{{"timestamp":"2026-10-16T00:00:{o:02}Z","ordinal":{o},"type":"response_item","payload":{{"type":"message","role":"user","content":[{{"type":"input_text","text":"{composed}"}}]}}}}"#,
                "\n",
                r#"{{"timestamp":"2026-10-16T00:01:{o:02}Z","ordinal":{o},"type":"event_msg","payload":{{"type":"task_complete","turn_id":"{turn}"}}}}"#,
                "\n",
            ),
            o = o,
            composed = composed,
            turn = turn,
        )
    };
    std::fs::write(&rollout, line(1, "t1")).unwrap();
    let baseline = std::fs::metadata(&rollout).unwrap().len();
    let fence_dup = CompatFence {
        action_id: "rc-d".into(),
        queued_message_id: None,
        turn_id: None,
        rollout_path: Some(rollout.to_string_lossy().to_string()),
        rollout_baseline: Some(baseline),
        wire_hash: Some(wire_text_hash(composed)),
    };
    {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&rollout)
            .unwrap();
        f.write_all(line(3, "t3").as_bytes()).unwrap();
        f.write_all(line(4, "t4").as_bytes()).unwrap();
    }
    assert!(
        !reconcile_compat_fence_with(&fence_dup, &proj, &no_queue).unwrap(),
        "多义匹配返回 false"
    );

    // 截断：追加段超过 2 MiB 尾窗 → 无法证明唯一性 → false
    let big = dir
        .path()
        .join(format!("rollout-2026-10-16T00-02-00-{THREAD}.jsonl"));
    std::fs::write(&big, "").unwrap();
    let fence_big = CompatFence {
        action_id: "rc-b".into(),
        queued_message_id: None,
        turn_id: None,
        rollout_path: Some(big.to_string_lossy().to_string()),
        rollout_baseline: Some(0),
        wire_hash: Some(wire_text_hash(composed)),
    };
    {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new().append(true).open(&big).unwrap();
        let filler = "x".repeat(1024);
        for _ in 0..(2 * 1024 * 1024 / 1025 + 2) {
            f.write_all(format!("{filler}\n").as_bytes()).unwrap();
        }
    }
    assert!(
        !reconcile_compat_fence_with(&fence_big, &proj, &no_queue).unwrap(),
        "已被尾窗截断返回 false"
    );

    // 投影归属不一致：rollout 文件名 UUID 与投影 thread 不符 → false
    let other_rollout = dir
        .path()
        .join("rollout-2026-10-16T00-03-00-aaaaaaaa-bbbb-4ccc-8ddd-eeeeffff0000.jsonl");
    std::fs::write(&other_rollout, "").unwrap();
    let fence_mismatch = CompatFence {
        action_id: "rc-m".into(),
        queued_message_id: None,
        turn_id: None,
        rollout_path: Some(other_rollout.to_string_lossy().to_string()),
        rollout_baseline: Some(0),
        wire_hash: Some(wire_text_hash(composed)),
    };
    assert!(
        !reconcile_compat_fence_with(&fence_mismatch, &proj, &no_queue).unwrap(),
        "投影与 rollout 归属不一致返回 false"
    );
}

// 保持 ProtocolError 类型可达（reconcile 错误面在 Task 8 接线；锁定 re-export 存在）
#[allow(dead_code)]
fn _protocol_error_reachable(e: ProtocolError) -> ProtocolError {
    e
}

// ============================================================
// R6.1：普通 FIFO 只收 send——交互行（审批/回答 register→reserve 之间，或审计
// INSERT 失败回滚后）残留 queued 时，不得被调度器当普通消息取走。
// ============================================================

/// 登记一个 UserInput 交互行（模拟 register→reserve 之间 / 审计回滚后残留 queued），
/// flush 取出的队首必须是 send 行；交互行原样留队、零派发。
#[tokio::test]
async fn interaction_row_left_queued_never_becomes_send_head() {
    let h = harness();

    // 交互行：UserInput 登记（payload hash 真实形态；queued_text 恒空——账本只存 send 原文）
    let ictx = h.ctx("act-inter-1", ActionKind::UserInput);
    let ih = payload_hash(
        ActionKind::UserInput,
        &h.key,
        Some("n:7"),
        Some("tok"),
        &ActionPayload::Text("approve".into()),
    );
    let iv = h
        .store
        .register(
            &ictx,
            ActionKind::UserInput,
            Some("n:7"),
            Some("tok"),
            &ih,
            None,
        )
        .expect("register interaction row");
    assert_eq!(iv.state, ActionState::Queued, "交互行残留 queued");

    // 后到的普通消息（seq 更大）
    let q = h
        .router
        .send(
            h.ctx("act-send-1", ActionKind::Send),
            "普通消息".into(),
            true,
            false,
        )
        .await;
    assert!(matches!(q, RouterOutcome::Queued { .. }));

    // 队首必须是 send 行，不是交互行
    let head = h.store.next_queued(&h.key).expect("query").expect("head");
    assert_eq!(
        head.ctx.action_id, "act-send-1",
        "普通 FIFO 队首不得取交互行（R6.1）"
    );
    assert_eq!(head.ctx.kind, ActionKind::Send);

    // flush 只派 send 行；交互行原样留队
    let out = h
        .router
        .flush_thread(&h.key)
        .await
        .expect("send dispatchable");
    assert_eq!(completed(out).delivery, Delivery::Accepted);
    let sends = h.dispatch.sends.lock().unwrap();
    assert_eq!(sends.len(), 1);
    assert_eq!(sends[0].action_id, "act-send-1", "派发的必须是普通消息");
    drop(sends);
    assert_eq!(
        h.state_of("act-inter-1"),
        ActionState::Queued,
        "交互行不得被当普通消息派发/落账"
    );

    // 交互行也不出现在队列位置视图（position 只对 send 计）
    let mine = h.store.list_queued(&h.key, "dev-1").expect("list");
    assert!(
        !mine.iter().any(|v| v.action_id == "act-inter-1"),
        "队列视图只收普通消息：{mine:?}"
    );
}

// ============================================================
// R6.3：Turn 终态绑定顺序限制——flush/释放必须以「对应 turnId 的完成证据」为
// 条件；Accepted 前后的旧 idle 更新不释放；缺 turnId 保守留队；重启后仍等待原
// Turn 终态。
// ============================================================

/// 回执 turnId + 基线 revision 绑定后：旧 idle（revision ≤ 基线）、active、stale、
/// **绑定 turn 缺终态证据（另一 turn 完成/本 turn inProgress/不在场）**一律不释放；
/// 「投影新鲜 + 目标 turn 终态在场 + revision 严格更新」齐备才释放并解锁接力；
/// 绑定期间队列视图如实显示屏障在册。（2026-10-17 复审 C1：泛 idle 不再是证据）
#[tokio::test]
async fn turn_restriction_needs_matching_completion_evidence() {
    let h = harness_with(FakeDispatch::accepted_turn("turn-9", Some(7)));
    let a = h.ctx("trn-a", ActionKind::Send);
    let _ = completed(h.router.send(a, "第一条".into(), false, false).await);
    assert_eq!(h.dispatch.send_count(), 1);

    // 绑定在册：flush 不接力；持久屏障在册（队列视图 blockedReason 同源）
    h.router
        .send(
            h.ctx("trn-b", ActionKind::Send),
            "第二条".into(),
            true,
            false,
        )
        .await;
    assert!(
        h.router.flush_thread(&h.key).await.is_none(),
        "Turn 顺序限制在册不接力"
    );
    assert!(
        h.store.has_compat_fence(&h.key).expect("query"),
        "Turn 绑定以持久屏障在册"
    );

    // 负例面（**证据未登记**）：任何投影都不释放——旧 idle（rev == 基线）、active、
    // stale、实体不在场、别的 turnId 有证据，全部保守保留（第三轮复审 B1：负例
    // 语义现在完全由证据门承载）
    assert!(
        !h.router
            .release_turn_if_complete(&h.key, &projection_with_rev(THREAD, "idle", 7)),
        "证据未登记：旧 idle 更新不得释放"
    );
    assert!(
        !h.router
            .release_turn_if_complete(&h.key, &projection_with_rev(THREAD, "active", 8)),
        "runtime 未归 idle 不释放"
    );
    let mut stale = projection_with_rev(THREAD, "idle", 9);
    stale.stale = true;
    assert!(
        !h.router.release_turn_if_complete(&h.key, &stale),
        "stale 投影不释放（owner 换代/断线限制保留）"
    );
    assert!(h.router.flush_thread(&h.key).await.is_none());
    assert!(
        !h.router
            .release_turn_if_complete(&h.key, &projection_with_rev(THREAD, "idle", 9)),
        "证据未登记：泛 idle（即使 revision 更新）不释放（C1：目标身份与终态证据不能省略）"
    );
    h.dispatch.set_terminal_turn("turn-other");
    assert!(
        !h.router
            .release_turn_if_complete(&h.key, &projection_with_rev(THREAD, "idle", 9)),
        "别的 turnId 有证据也救不了本绑定（按绑定 turnId 精确核对）"
    );
    h.dispatch.unset_terminal_turn("turn-other");

    // 绑定 turn 自身以终态在场（完成快照已先于 ACK 在场，rev == 基线）+ fresh
    // idle：释放 + 清持久屏障 + 接力发下一条（第三轮复审 B1 场景 1：revision
    // 基线门不得阻止合法完成）
    h.dispatch.set_terminal_turn("turn-9");
    assert!(
        h.router
            .release_turn_if_complete(&h.key, &projection_with_rev(THREAD, "idle", 7)),
        "目标 turn 终态证据成立：rev == 基线（完成快照先到）也必须释放"
    );
    assert!(
        !h.store.has_compat_fence(&h.key).expect("query"),
        "释放同事务清持久屏障"
    );
    let out = h.router.flush_thread(&h.key).await.expect("b dispatchable");
    assert_eq!(completed(out).delivery, Delivery::Accepted);
    assert_eq!(h.dispatch.send_count(), 2);
}

/// 重启后仍等待原 Turn 终态：绑定持久化在账本，重建 Router 后 flush 依旧不放行；
/// 完成证据（fresh idle）到达即恢复接力。
#[tokio::test]
async fn turn_restriction_survives_restart_until_completion() {
    let mut h = harness_with(FakeDispatch::accepted_turn("turn-p", Some(7)));
    let _ = completed(
        h.router
            .send(
                h.ctx("trn-r-a", ActionKind::Send),
                "重启前已发".into(),
                false,
                false,
            )
            .await,
    );
    h.router
        .send(
            h.ctx("trn-r-b", ActionKind::Send),
            "重启后排队".into(),
            true,
            false,
        )
        .await;
    assert!(h.store.has_compat_fence(&h.key).expect("binding fence"));

    // —— 进程重启（同一 DB 重建 Store/Router）——
    h.restart_with(FakeDispatch::accepted());
    assert!(
        h.router.flush_thread(&h.key).await.is_none(),
        "重启后仍等待原 Turn 终态（绑定从账本重建）"
    );
    // 泛 idle 不再是证据（C1）：绑定 turn-p 未以终态在场不释放
    assert!(
        !h.router
            .release_turn_if_complete(&h.key, &projection_with_rev(THREAD, "idle", 100)),
        "重启重建 baseline=None：仍需目标 turn 终态证据，泛 idle 不释放"
    );
    // 完成证据到达（绑定 turn-p 以终态在场）：释放并按序接力
    h.dispatch.set_terminal_turn("turn-p");
    assert!(
        h.router
            .release_turn_if_complete(&h.key, &projection_with_rev(THREAD, "idle", 100)),
        "重启重建 baseline=None：首个新鲜 idle + 目标 turn 终态证据即当前真值"
    );
    let out = h
        .router
        .flush_thread(&h.key)
        .await
        .expect("queued row dispatchable");
    assert_eq!(completed(out).delivery, Delivery::Accepted);
    let sends = h.dispatch.sends.lock().unwrap();
    assert_eq!(sends.len(), 1);
    assert_eq!(sends[0].action_id, "trn-r-b");
}

/// 回执缺 turnId（真实回执可能拿不到）→ 保守保留顺序限制，回执如实携带 None；
/// 任何投影都不释放。
#[tokio::test]
async fn receipt_without_turn_id_keeps_restriction() {
    let h = harness_with(FakeDispatch::new(DispatchOutcome {
        delivery: Delivery::Accepted,
        envelope_id: Some("env-nt".into()),
        turn_id: None,
        reason_code: None,
        revision: Some(7),
    }));
    let _ = completed(
        h.router
            .send(
                h.ctx("trn-nt", ActionKind::Send),
                "无回执消息".into(),
                false,
                false,
            )
            .await,
    );
    let v = h.view_of("trn-nt");
    assert_eq!(v.state, ActionState::Accepted);
    assert!(v.receipt.unwrap().turn_id.is_none(), "缺 turnId 如实回执");
    h.router
        .send(
            h.ctx("trn-nt-2", ActionKind::Send),
            "后续消息".into(),
            true,
            false,
        )
        .await;
    // 缺 turnId：任何投影（含更新的 fresh idle）都不视为完成
    assert!(
        !h.router
            .release_turn_if_complete(&h.key, &projection_with_rev(THREAD, "idle", 99)),
        "缺 turnId 保守不释放"
    );
    assert!(
        h.router.flush_thread(&h.key).await.is_none(),
        "缺 turnId 保持留队"
    );
}

/// 第三轮复审 B1 场景 1：目标 Turn 的 completed snapshot **先于 ACK** 在场（基线
/// == 完成快照 revision，之后无新增量）——精确终态证据成立即必须释放，revision
/// 不得阻止合法完成（旧实现 7 > 7 恒假 → 永久堵队）。
#[tokio::test]
async fn release_turn_on_completion_snapshot_already_before_ack() {
    let h = harness_with(FakeDispatch::accepted_turn("turn-9", Some(7)));
    let a = h.ctx("b1-a", ActionKind::Send);
    let _ = completed(h.router.send(a, "第一条".into(), false, false).await);
    assert_eq!(h.dispatch.send_count(), 1);
    // 完成快照先于 ACK 落镜像（证据已成立），ACK 采集的基线 == 同一 revision
    h.dispatch.set_terminal_turn("turn-9");
    h.router
        .send(
            h.ctx("b1-b", ActionKind::Send),
            "第二条".into(),
            true,
            false,
        )
        .await;
    assert!(
        h.router
            .release_turn_if_complete(&h.key, &projection_with_rev(THREAD, "idle", 7)),
        "证据成立 + rev == 基线：完成快照先到也必须释放（B1 场景 1）"
    );
    let out = h.router.flush_thread(&h.key).await.expect("b dispatchable");
    assert_eq!(completed(out).delivery, Delivery::Accepted);
    assert_eq!(h.dispatch.send_count(), 2);
}

/// 第三轮复审 B1 场景 2：Desktop 重启/owner 换代——旧基线 300，新 owner 重新从
/// revision 1 发布 snapshot 且目标 Turn 已 completed。跨世代比较 1 > 300 恒假，
/// 精确证据成立即必须释放（禁止「任意 idle 即放行」——证据仍是必要条件）。
#[tokio::test]
async fn release_turn_after_owner_generation_with_lower_revision() {
    let h = harness_with(FakeDispatch::accepted_turn("turn-gen", Some(300)));
    let a = h.ctx("b1-gen", ActionKind::Send);
    let _ = completed(h.router.send(a, "换代前已发".into(), false, false).await);
    h.router
        .send(
            h.ctx("b1-gen-2", ActionKind::Send),
            "换代后排队".into(),
            true,
            false,
        )
        .await;
    // 新 owner 世界：镜像换代重置后 revision 1，目标 Turn 已 completed
    h.dispatch.set_terminal_turn("turn-gen");
    assert!(
        h.router
            .release_turn_if_complete(&h.key, &projection_with_rev(THREAD, "idle", 1)),
        "新 owner revision 1 < 旧基线 300：精确证据成立必须释放（B1 场景 2）"
    );
    let out = h
        .router
        .flush_thread(&h.key)
        .await
        .expect("queued dispatchable");
    assert_eq!(completed(out).delivery, Delivery::Accepted);
}

/// 2026-10-17 复审 C1.2：Unknown 不是完成——多轮周期对账（converge_with_ledger）
/// 不得解除顺序限制，同 thread 后续消息只排队、零新增派发；任何投影（含严格更新
/// 的 fresh idle）都不释放。Original Turn 可能已收到（ack/落账丢失），「到点了」
/// 不是解除屏障的定时器条件。
#[tokio::test]
async fn unknown_delivery_never_unlocks_turn_restriction() {
    let h = harness_with(FakeDispatch::new(DispatchOutcome {
        delivery: Delivery::UnknownDelivery,
        envelope_id: Some("env-uk".into()),
        turn_id: None,
        reason_code: None,
        revision: None,
    }));
    let r = completed(
        h.router
            .send(
                h.ctx("unk-a", ActionKind::Send),
                "未知投递".into(),
                false,
                false,
            )
            .await,
    );
    assert_eq!(r.delivery, Delivery::UnknownDelivery);
    assert_eq!(h.state_of("unk-a"), ActionState::Unknown);
    assert!(
        h.store.has_compat_fence(&h.key).expect("query"),
        "Unknown 的保守屏障随落账在册"
    );

    // 多轮对账 + 严格更新的 fresh idle 投影：一律不解锁、不派发
    for round in 0..3 {
        h.router.converge_with_ledger();
        assert!(
            h.router.flush_thread(&h.key).await.is_none(),
            "第 {round} 轮对账不得解除 Unknown 的顺序限制"
        );
        assert!(
            !h.router
                .release_turn_if_complete(&h.key, &projection_with_rev(THREAD, "idle", 999)),
            "Unknown 无目标 Turn 身份，任何投影都不构成完成证据"
        );
    }
    // 后续消息只排队（顺序限制在册）
    let q = h
        .router
        .send(
            h.ctx("unk-b", ActionKind::Send),
            "后续消息".into(),
            false,
            false,
        )
        .await;
    assert!(
        matches!(q, RouterOutcome::Queued { .. }),
        "Unknown 后续消息留队"
    );
    assert_eq!(h.dispatch.send_count(), 1, "零新增派发（不重发不越队）");
}

/// 2026-10-17 复审 C1.3：Accepted（缺 turnId）与 UnknownDelivery 的落账与 Turn
/// 顺序屏障在派发独立任务内**同一 UPDATE** 原子持久化——等待者取消/进程退出不再
/// 产生「已接受但无屏障」窗口；重启重建后保守屏障仍在册，flush 闸住、任何投影
/// 都不释放。
#[tokio::test]
async fn accepted_and_unknown_persist_turn_barrier_atomically() {
    let mut h = harness_with(FakeDispatch::new(DispatchOutcome {
        delivery: Delivery::Accepted,
        envelope_id: Some("env-nt".into()),
        turn_id: None,
        reason_code: None,
        revision: Some(7),
    }));
    // ① Accepted 缺 turnId：屏障随落账持久化（TDD 红：旧实现只留内存限制）
    let _ = completed(
        h.router
            .send(
                h.ctx("bar-a", ActionKind::Send),
                "缺ID已发".into(),
                false,
                false,
            )
            .await,
    );
    assert_eq!(h.state_of("bar-a"), ActionState::Accepted);
    assert!(
        h.store.has_compat_fence(&h.key).expect("query"),
        "Accepted 缺 turnId 也存保守屏障"
    );

    // ② UnknownDelivery（另一 thread，避免①的内存限制拦住派发）：保守屏障同样持久
    let key2 = ThreadKey {
        host_id: "local".into(),
        thread_id: format!("{THREAD}2"),
    };
    let ctx2 = ActionContext {
        action_id: "bar-u".into(),
        device_id: "dev-1".into(),
        device_name: "iPhone".into(),
        kind: ActionKind::Send,
        key: key2.clone(),
    };
    h.restart_with(FakeDispatch::new(DispatchOutcome {
        delivery: Delivery::UnknownDelivery,
        envelope_id: Some("env-u2".into()),
        turn_id: None,
        reason_code: None,
        revision: None,
    }));
    let r2 = completed(h.router.send(ctx2, "未知投递".into(), false, false).await);
    assert_eq!(r2.delivery, Delivery::UnknownDelivery);
    assert!(
        h.store.has_compat_fence(&key2).expect("query"),
        "Unknown 存保守屏障"
    );

    // —— 重启：两道保守屏障都从账本重建，flush 闸住、任何投影都不释放 ——
    h.restart();
    assert!(
        h.router.flush_thread(&h.key).await.is_none(),
        "重启后 Accepted-缺ID 的保守屏障仍闸住队列"
    );
    assert!(
        !h.router
            .release_turn_if_complete(&h.key, &projection_with_rev(THREAD, "idle", 999)),
        "无 turnId 的保守屏障不因任何投影释放（缺身份不算完成）"
    );
    assert!(
        h.router.flush_thread(&key2).await.is_none(),
        "重启后 Unknown 的保守屏障仍闸住队列"
    );
}

/// R9.3：终态持久化失败（trigger 拒绝 accepted 落账）→ 不对外虚称完成（unknown
/// 口径回执）、保持 reserved、顺序限制不释放；重启 recover 成 unknown。
#[tokio::test]
async fn finish_persist_failure_keeps_reserved_reports_unknown() {
    let mut h = harness();
    {
        let conn = h.conn.lock().unwrap();
        conn.execute(
            "CREATE TRIGGER accepted_reject BEFORE UPDATE ON codex_actions
             WHEN NEW.state = 'accepted'
             BEGIN SELECT RAISE(ABORT, 'accepted rejected by test trigger'); END",
            [],
        )
        .unwrap();
    }
    h.router
        .send(
            h.ctx("pf-a", ActionKind::Send),
            "落账失败消息".into(),
            true,
            false,
        )
        .await;
    let out = h.router.flush_thread(&h.key).await.expect("dispatchable");
    let receipt = completed(out);
    assert_eq!(
        receipt.delivery,
        Delivery::UnknownDelivery,
        "不对外虚称完成"
    );
    assert_eq!(
        receipt.reason_code.as_deref(),
        Some("finish_persist_failed")
    );
    assert_eq!(
        h.state_of("pf-a"),
        ActionState::Reserved,
        "保持 reserved（下次 recover 成 unknown）"
    );
    // 顺序限制不释放：队列闸住
    h.router
        .send(
            h.ctx("pf-b", ActionKind::Send),
            "后续消息".into(),
            true,
            false,
        )
        .await;
    assert!(
        h.router.flush_thread(&h.key).await.is_none(),
        "持久化失败不释放顺序限制"
    );

    // —— 重启：reserved → unknown ——
    h.restart();
    assert_eq!(h.state_of("pf-a"), ActionState::Unknown);
}

// ============================================================
// 人工解除保守屏障（2026-10-19 无出口屏障修复，PR #165 复审 Important）：
// route=desktop_ipc 且 state=unknown（尤其缺 turnId）的屏障在 release_turn_if_complete
//（要求 turnId 绑定）与 reconcile（要求 rollout 三证据）两处自动出口都到不了——
// 人工解除端点 = store CAS 清屏障 + router 清内存顺序限制 + 接力 flush。
// ============================================================

/// 无出口屏障的人工解除端到端（router/store 面）：Unknown 屏障闸住队列 →
/// force_release_barrier（CAS 清持久屏障）+ force_clear_turn_limit（清内存限制）
/// → flush 接力下一条；重启后限制不复活（turn_bindings 不再枚举该行）；
/// 幂等（重复解除 AlreadyReleased、零重复审计）；非属主 NotFound。
#[tokio::test]
async fn force_release_unlocks_unknown_barrier_and_queue_resumes() {
    let mut h = harness_with(FakeDispatch::new(DispatchOutcome {
        delivery: Delivery::UnknownDelivery,
        envelope_id: Some("env-fc".into()),
        turn_id: None,
        reason_code: None,
        revision: None,
    }));
    let _ = completed(
        h.router
            .send(
                h.ctx("fc-a", ActionKind::Send),
                "未知投递".into(),
                false,
                false,
            )
            .await,
    );
    assert_eq!(h.state_of("fc-a"), ActionState::Unknown);
    assert!(
        h.store.has_compat_fence(&h.key).expect("query"),
        "保守屏障在册"
    );
    h.router
        .send(
            h.ctx("fc-b", ActionKind::Send),
            "被困后续".into(),
            true,
            false,
        )
        .await;
    assert!(
        h.router.flush_thread(&h.key).await.is_none(),
        "解除前队列闸住（无出口屏障）"
    );

    // 非属主：NotFound（不给存在性预言机）
    assert!(matches!(
        h.store
            .force_release_barrier("fc-a", "dev-9")
            .expect("query"),
        BarrierRelease::NotFound
    ));

    // 人工解除：CAS 清持久屏障 + 清内存顺序限制
    match h
        .store
        .force_release_barrier("fc-a", "dev-1")
        .expect("release")
    {
        BarrierRelease::Released { view, .. } => {
            assert_eq!(view.state, ActionState::Unknown, "状态不被改写（只清屏障）");
        }
        other => panic!("应 Released：{other:?}"),
    }
    assert!(
        h.router.force_clear_turn_limit(&h.key, "fc-a"),
        "内存顺序限制被清"
    );
    assert!(
        !h.store.has_compat_fence(&h.key).expect("query"),
        "持久屏障已清"
    );
    // 接力恢复：被困后续立即派发
    let out = h.router.flush_thread(&h.key).await.expect("unblocked");
    assert_eq!(completed(out).delivery, Delivery::UnknownDelivery);
    assert_eq!(h.dispatch.send_count(), 2, "fc-b 被接力派发");

    // 幂等：重复解除 AlreadyReleased（零新写入），审计恰一条
    assert!(matches!(
        h.store
            .force_release_barrier("fc-a", "dev-1")
            .expect("again"),
        BarrierRelease::AlreadyReleased { .. }
    ));
    let audits = {
        let conn = h.conn.lock().unwrap();
        multi_agents_manager_lib::database::dao::write_audit::recent_conn(&conn, 100)
    };
    let force_audits = audits
        .iter()
        .filter(|a| a.action == "force_continue")
        .count();
    assert_eq!(force_audits, 1, "解除审计恰好一条（重复调用不重复写）");
    assert!(
        audits.iter().any(|a| a.action == "force_continue"
            && a.result == "barrier_force_released"
            && a.summary.contains("fc-a")),
        "审计行携带动作与结果词"
    );

    // 重启：解除过的行不再进 turn_bindings，限制不复活（fc-b 接力派发后按 Unknown
    // 口径存的是**它自己的新**保守屏障——不是 fc-a 的回置）
    h.restart();
    let ids = h.store.barrier_action_ids(&h.key).expect("query");
    assert!(
        !ids.contains(&"fc-a".to_string()),
        "解除过的行屏障不复活（单调性）：{ids:?}"
    );
    assert!(
        ids.contains(&"fc-b".to_string()),
        "fc-b 派发后自己的保守屏障在册（新屏障，非回置）：{ids:?}"
    );
}

/// queued 行走既有撤回：人工解除端点语义拒绝（NotEligible），屏障与队列不被误触。
/// queued 行屏障不在册 → `barrier_in_register=false`（HTTP 层回 no_barrier 之前的
/// 撤回指引臂，不涉 barrier_in_flight）。
#[tokio::test]
async fn force_release_rejects_queued_rows() {
    let h = harness();
    h.router
        .send(
            h.ctx("fc-q", ActionKind::Send),
            "仍在排队".into(),
            true,
            false,
        )
        .await;
    match h
        .store
        .force_release_barrier("fc-q", "dev-1")
        .expect("query")
    {
        BarrierRelease::NotEligible {
            view,
            barrier_in_register,
        } => {
            assert_eq!(view.state, ActionState::Queued, "409 指引走撤回的判据");
            assert!(!barrier_in_register, "queued 行屏障不在册");
        }
        other => panic!("queued 应 NotEligible：{other:?}"),
    }
    // 动作保持 queued 可撤（撤回路径不受影响）
    assert!(h.store.retract("fc-q", "dev-1").expect("retract"));
}

/// reserved 在飞 fence（PR #165 复审 Minor）：屏障在册但不在人工解除范围——
/// NotEligible 带 `barrier_in_register=true`（HTTP 层据此回 `barrier_in_flight`
/// 而非误导性的 `no_barrier`），屏障不被触碰（自动落账路径负责）。
#[tokio::test]
async fn force_release_rejects_reserved_in_flight_fence() {
    let h = harness();
    h.router
        .send(
            h.ctx("fc-r", ActionKind::Send),
            "派发在飞".into(),
            true,
            false,
        )
        .await;
    // CAS queued → reserved 并武装在飞 fence（不经派发任务，直摆中间态）
    assert!(
        h.store
            .reserve("fc-r", ControlRoute::DesktopIpc)
            .expect("reserve"),
        "queued → reserved CAS 命中"
    );
    h.store.set_compat_fence("fc-r", None, None).expect("fence");
    match h
        .store
        .force_release_barrier("fc-r", "dev-1")
        .expect("query")
    {
        BarrierRelease::NotEligible {
            view,
            barrier_in_register,
        } => {
            assert_eq!(view.state, ActionState::Reserved);
            assert!(
                barrier_in_register,
                "屏障在册：HTTP 层应回 barrier_in_flight"
            );
        }
        other => panic!("reserved+fence 应 NotEligible：{other:?}"),
    }
    assert!(
        h.store.has_compat_fence(&h.key).expect("query"),
        "在飞 fence 不被人工解除触碰（有自动路径）"
    );
}

/// force_clear_turn_limit 精确匹配负例（PR #165 复审 Minor）：传入不同 action_id
/// 不清除、返回 false——防误清同 Thread 上新一轮派发的顺序限制。观察法：先清持久
/// 屏障（端点两步之间的中间态），此时唯一闸 = 内存顺序限制——负例调用后 fcx-b 仍
/// 接不上 = 限制确实未被误清；精确匹配调用后立即接力。
#[tokio::test]
async fn force_clear_turn_limit_requires_exact_action_match() {
    let h = harness_with(FakeDispatch::new(DispatchOutcome {
        delivery: Delivery::UnknownDelivery,
        envelope_id: Some("env-fcx".into()),
        turn_id: None,
        reason_code: None,
        revision: None,
    }));
    let _ = completed(
        h.router
            .send(
                h.ctx("fcx-a", ActionKind::Send),
                "未知投递".into(),
                false,
                false,
            )
            .await,
    );
    h.router
        .send(
            h.ctx("fcx-b", ActionKind::Send),
            "被困后续".into(),
            true,
            false,
        )
        .await;
    // 只清持久屏障、不动内存限制（模拟解除端点 CAS 步与清内存步之间的中间态）
    assert!(matches!(
        h.store
            .force_release_barrier("fcx-a", "dev-1")
            .expect("release"),
        BarrierRelease::Released { .. }
    ));
    // 负例：不同 action_id → 返回 false（返回值即「是否清除」契约）
    assert!(
        !h.router.force_clear_turn_limit(&h.key, "fcx-other"),
        "非精确匹配必须返回 false"
    );
    assert!(
        h.router.flush_thread(&h.key).await.is_none(),
        "限制未被误清：fcx-b 仍被 fcx-a 的内存顺序限制闸住"
    );
    // 精确匹配 → true，被困的 fcx-b 立即接力
    assert!(h.router.force_clear_turn_limit(&h.key, "fcx-a"));
    let out = h.router.flush_thread(&h.key).await.expect("unblocked");
    assert_eq!(completed(out).action_id, "fcx-b");
}

// ============================================================
// S2：打断后队列持久暂停 + 一次性越队授权（router/账本级）
// ============================================================

/// 暂停的旧队列冻结 flush（不接受接力）；重启后仍在；用户显式「继续发送」
/// （clear_queue_pause）后恢复 FIFO 接力。
#[tokio::test]
async fn queue_pause_blocks_flush_and_survives_restart() {
    let mut h = harness();
    // 两条旧队列（busy 下排队）
    h.dispatch.set_busy(true);
    let (a, _p) = queued(
        h.router
            .send(h.ctx("qa", ActionKind::Send), "旧一".into(), true, false)
            .await,
    );
    let (b, _p) = queued(
        h.router
            .send(h.ctx("qb", ActionKind::Send), "旧二".into(), true, false)
            .await,
    );
    h.dispatch.set_busy(false);
    // 打断受理 → 暂停（生产由打断端点写；此处直写同一持久层）
    assert!(h
        .store
        .set_queue_pause(
            &h.key,
            "interrupt_accepted",
            "test-token",
            Some("turn-pause")
        )
        .expect("pause"));
    // 未暂停前可接力；暂停后冻结
    assert!(h.store.queue_pause(&h.key).expect("query").is_some());
    assert!(
        h.router.flush_thread(&h.key).await.is_none(),
        "暂停的旧队列不接力"
    );
    assert_eq!(h.dispatch.send_count(), 0, "冻结期零派发");

    // —— 重启：同库重建，暂停仍在 ——
    h.restart();
    assert!(
        h.store.queue_pause(&h.key).expect("query").is_some(),
        "重启后暂停仍在"
    );
    assert!(
        h.router.flush_thread(&h.key).await.is_none(),
        "重启后仍冻结"
    );

    // 用户显式「继续发送」→ 恢复 FIFO 接力
    assert!(h.store.clear_queue_pause(&h.key).expect("resume"));
    let out = h.router.flush_thread(&h.key).await.expect("resumed relay");
    assert_eq!(completed(out).action_id, a, "恢复后按 FIFO 派发队首");
    assert_eq!(
        h.state_of(&b),
        ActionState::Accepted,
        "明确恢复合并发送第二条"
    );
}

/// 暂停 + 旧队列有内容：带授权的新意见**恰好派发一次**，旧队列两条原样不动；
/// 无授权的新意见**入队但不派发**（C05：暂停是派发保护闩，不是入队否决）；
/// 暂停但旧队列已空 → 正常派发无需授权。
#[tokio::test]
async fn bypass_grant_sends_new_action_once_old_queue_untouched() {
    let h = harness();
    // 两条旧队列
    h.dispatch.set_busy(true);
    queued(
        h.router
            .send(h.ctx("old-a", ActionKind::Send), "旧一".into(), true, false)
            .await,
    );
    queued(
        h.router
            .send(h.ctx("old-b", ActionKind::Send), "旧二".into(), true, false)
            .await,
    );
    h.dispatch.set_busy(false);
    h.store
        .set_queue_pause(
            &h.key,
            "interrupt_accepted",
            "test-token",
            Some("turn-pause"),
        )
        .unwrap();

    // 新意见（无授权）→ 入队但不派发（C05）：旧暂停不被穿透，派发权归用户
    let (id, pos) = queued(
        h.router
            .send(
                h.ctx("new-1", ActionKind::Send),
                "新意见一".into(),
                false,
                false,
            )
            .await,
    );
    assert_eq!(id, "new-1");
    assert!(pos >= 3, "排在旧队列之后：{pos}");
    assert_eq!(h.state_of("new-1"), ActionState::Queued, "留在排队态");
    assert_eq!(h.dispatch.send_count(), 0, "暂停下零派发");

    // 新意见（带授权）→ 越队派发恰好一次；旧两条原样 queued
    let r = completed(
        h.router
            .send(
                h.ctx("new-2", ActionKind::Send),
                "新意见二".into(),
                false,
                true,
            )
            .await,
    );
    assert_eq!(r.delivery, Delivery::Accepted, "授权动作直接 A 路派发");
    assert_eq!(h.dispatch.send_count(), 1);
    assert_eq!(
        h.dispatch.sends.lock().unwrap()[0].action_id,
        "new-2",
        "派发的是授权的新意见"
    );
    assert_eq!(h.state_of("old-a"), ActionState::Queued, "旧队列不动");
    assert_eq!(h.state_of("old-b"), ActionState::Queued, "旧队列不动");
    assert_eq!(h.state_of("new-2"), ActionState::Accepted);

    // 授权消费即失效：同 action 重放返回既有终态（Accepted），零新增派发
    let r = completed(
        h.router
            .send(
                h.ctx("new-2", ActionKind::Send),
                "新意见二".into(),
                false,
                true,
            )
            .await,
    );
    assert_eq!(r.delivery, Delivery::Accepted, "重放返回账本既有回执");
    assert_eq!(h.dispatch.send_count(), 1, "重放零新增派发");

    // queue_only（只入队）在暂停 + 旧队列有内容时同样入队不派发
    let (id, _pos) = queued(
        h.router
            .send(
                h.ctx("new-3", ActionKind::Send),
                "新意见三".into(),
                true,
                false,
            )
            .await,
    );
    assert_eq!(id, "new-3");
    assert_eq!(h.state_of("new-3"), ActionState::Queued);
    assert_eq!(h.dispatch.send_count(), 1, "只入队不再新增派发");
}

/// C05（队列展示收纳复审 Q01）：暂停 + 旧队列在场时的**普通新消息**必须能入队
/// ——真实 `codex_queued`、零 start-turn、旧暂停原样保留，用户随后在统一队列区
/// 对任一条显式「先处理这条」。修复前该路径 `finish_not_sent(queue_paused)` 把新
/// 消息终态拒绝，队列里根本没有它，前端「先处理这条」无从点起（复审 P2）。
#[tokio::test]
async fn paused_send_enqueues_without_dispatch_then_prioritize_sends_only_it() {
    use multi_agents_manager_lib::services::codex_control::{Deadline, PrioritizeOutcome};
    let h = harness();
    // 打断前已排队的旧消息（暂停的保护对象）
    h.dispatch.set_busy(true);
    queued(
        h.router
            .send(h.ctx("old-a", ActionKind::Send), "旧一".into(), true, false)
            .await,
    );
    h.dispatch.set_busy(false);
    h.store
        .set_queue_pause(
            &h.key,
            "interrupt_accepted",
            "test-token",
            Some("turn-pause"),
        )
        .unwrap();

    // ① 普通发送（无越队授权）→ 留在 queued，零派发
    let (id, pos) = queued(
        h.router
            .send(
                h.ctx("new-q", ActionKind::Send),
                "新意见".into(),
                false,
                false,
            )
            .await,
    );
    assert_eq!(id, "new-q", "新消息进入队列（不是 not_sent 终态）");
    assert!(pos >= 2, "排位在旧队列之后：{pos}");
    assert_eq!(h.state_of("new-q"), ActionState::Queued, "账本留在排队态");
    assert_eq!(h.dispatch.send_count(), 0, "暂停下零派发");
    assert!(
        h.router.flush_thread(&h.key).await.is_none(),
        "调度循环不得穿透暂停自动接力"
    );
    assert_eq!(h.dispatch.send_count(), 0, "接力仍零派发");
    assert!(
        h.store.queue_pause(&h.key).unwrap().is_some(),
        "旧队列暂停原样保留"
    );

    // ② 用户显式「先处理这条」→ 只派该条；旧队列继续暂停
    let lock = h
        .router
        .thread_dispatch_lock(&h.key)
        .try_lock_owned()
        .unwrap();
    let receipt = match h
        .router
        .prioritize_queued("new-q", &h.key, Deadline::none(), lock)
        .await
    {
        PrioritizeOutcome::Completed(r) => r,
        other => panic!("expected Completed, got {other:?}"),
    };
    assert_eq!(receipt.delivery, Delivery::Accepted, "{receipt:?}");
    assert_eq!(
        h.dispatch.recorded_composed(),
        vec!["新意见"],
        "只派出被点的那一条"
    );
    assert_eq!(h.state_of("old-a"), ActionState::Queued, "旧队列不动");
    assert!(
        h.store.queue_pause(&h.key).unwrap().is_some(),
        "优先处理不解除暂停"
    );
}

/// 越队授权不能穿透 pending（零投递明确拒绝）；已被消费的授权重复提交拒绝
/// （grant_consumed）；授权跨 thread 无效（另一 thread 正常按自身状态派发）。
#[tokio::test]
async fn bypass_cannot_pierce_pending_or_replay() {
    let h = harness();
    h.dispatch.set_busy(true);
    queued(
        h.router
            .send(h.ctx("old-a", ActionKind::Send), "旧一".into(), true, false)
            .await,
    );
    h.dispatch.set_busy(false);
    h.store
        .set_queue_pause(
            &h.key,
            "interrupt_accepted",
            "test-token",
            Some("turn-pause"),
        )
        .unwrap();

    // pending 在场：授权不穿透 → native_pending（零投递明确拒绝，动作终态）
    h.dispatch.set_send_gate(SendGate::NativePending);
    let r = completed(
        h.router
            .send(
                h.ctx("np-1", ActionKind::Send),
                "带授权撞 pending".into(),
                false,
                true,
            )
            .await,
    );
    assert_eq!(
        r.reason_code.as_deref(),
        Some("native_pending"),
        "pending 不被授权穿透"
    );
    assert_eq!(h.dispatch.send_count(), 0, "pending 在场零派发");
    assert_eq!(h.state_of("np-1"), ActionState::Rejected, "拒绝入队即终态");
    h.dispatch.set_send_gate(SendGate::Clear);

    // 冻结（Busy）不消费授权：守卫冻结 → 动作留队；守卫恢复后同 action 带同一
    // 授权重试 → 正常派发（授权在写出前一刻才消费）
    h.dispatch.set_send_gate(SendGate::Busy);
    let (q, _pos) = queued(
        h.router
            .send(
                h.ctx("frozen-1", ActionKind::Send),
                "冻结在队".into(),
                false,
                true,
            )
            .await,
    );
    assert_eq!(q, "frozen-1");
    assert_eq!(h.dispatch.send_count(), 0);
    assert_eq!(
        h.state_of(&q),
        ActionState::Queued,
        "冻结动作留队（授权未消费）"
    );
    h.dispatch.set_send_gate(SendGate::Clear);
    let r = completed(
        h.router
            .send(
                h.ctx("frozen-1", ActionKind::Send),
                "冻结在队".into(),
                false,
                true,
            )
            .await,
    );
    assert_eq!(r.delivery, Delivery::Accepted, "重试带同一授权正常派发");
    assert_eq!(h.dispatch.send_count(), 1);
    // 收尾：frozen-1 受理绑定了顺序限制（FakeDispatch 回执 turn-1）——释放之，
    // 免得它冻住本用例后续的 dup-1（顺序屏障语义由专项用例钉住）
    h.dispatch.set_terminal_turn("turn-1");
    assert!(h
        .router
        .release_turn_if_complete(&h.key, &minimal_projection(THREAD, "idle")));

    // 重复消费拒绝：授权已被消费（busy 回队后重放带授权——该 action 的授权已用过）
    h.dispatch.set_send_gate(SendGate::Busy);
    let (d, _pos) = queued(
        h.router
            .send(
                h.ctx("dup-1", ActionKind::Send),
                "已用过授权".into(),
                false,
                true,
            )
            .await,
    );
    h.dispatch.set_send_gate(SendGate::Clear);
    // 授权在写出前被消费过（直接 CAS 模拟「已用」——正常路径由派发消费）
    assert!(
        h.store
            .consume_bypass_grant("dup-1", &h.key)
            .expect("consume"),
        "授权在册（未消费过才可消费）"
    );
    let r = completed(
        h.router
            .send(
                h.ctx("dup-1", ActionKind::Send),
                "已用过授权".into(),
                false,
                true,
            )
            .await,
    );
    assert_eq!(
        r.reason_code.as_deref(),
        Some("grant_consumed"),
        "重复消费拒绝"
    );
    assert_eq!(
        h.state_of(&d),
        ActionState::Rejected,
        "拒绝即终态（不二投）"
    );
    assert_eq!(h.dispatch.send_count(), 1, "零新增派发");

    // 跨 thread：另一 thread 的暂停/授权与本题无关——正常状态直接派发（无需授权）
    let other = ThreadKey {
        host_id: "local".into(),
        thread_id: "9f1e2d3c-9999-4222-8333-444455556666".into(),
    };
    let ctx = ActionContext {
        action_id: "x-thread".into(),
        device_id: "dev-1".into(),
        device_name: "iPhone".into(),
        kind: ActionKind::Send,
        key: other,
    };
    let r = completed(h.router.send(ctx, "别的会话".into(), false, true).await);
    assert_eq!(r.delivery, Delivery::Accepted, "未暂停的 thread 正常派发");
}

/// 【终审 round 1 收尾（S6 修正轮）】越队授权在写出边界被**真实消费**后遭遇写前忙
/// （busy_before_write）→ 动作冻结回队、授权不退还（区别于既有用例的 CAS 模拟——
/// 这里消费发生在真实派发路径上）；同 action 再带 bypass 重试必命 grant_consumed
/// 终态拒绝；队列恢复后，回队的越队动作按 FIFO 排在旧队列之后派发——**不二次越队**
/// （定序：一次授权只买一次插队，写前忙不算写出，也不重放授权）。
#[tokio::test]
async fn bypass_grant_consumed_then_busy_requeue_keeps_fifo_order() {
    let h = harness_with(FakeDispatch::accepted());
    // ① 旧队列两条（忙态 FIFO 入队——越队语义的对照面）
    h.dispatch.set_busy(true);
    for (id, text) in [("old-a", "旧一"), ("old-b", "旧二")] {
        queued(
            h.router
                .send(h.ctx(id, ActionKind::Send), text.into(), true, false)
                .await,
        );
    }
    h.dispatch.set_busy(false);
    h.store
        .set_queue_pause(
            &h.key,
            "interrupt_accepted",
            "test-token",
            Some("turn-pause"),
        )
        .unwrap();

    // ② 写前忙在册：带授权新意见 → 越队入队 → 写出边界授权**真实消费** →
    //    busy_before_write 冻结回队（回执=Queued，授权不退还）
    h.dispatch.set_busy_before_write(true);
    let (byp1, _pos) = queued(
        h.router
            .send(
                h.ctx("byp-1", ActionKind::Send),
                "带授权撞写前忙".into(),
                false,
                true,
            )
            .await,
    );
    assert_eq!(byp1, "byp-1");
    assert_eq!(h.dispatch.send_count(), 0, "写前忙零 start-turn");
    assert_eq!(h.state_of("byp-1"), ActionState::Queued, "冻结动作回队留队");

    // ③ 同 action 再带 bypass 重试 → grant_consumed（证明 ② 的消费是真实路径：
    //    授权在写出边界已被消费、busy 回队不退还；未做任何测试内 CAS 模拟）
    let r = completed(
        h.router
            .send(
                h.ctx("byp-1", ActionKind::Send),
                "带授权撞写前忙".into(),
                false,
                true,
            )
            .await,
    );
    assert_eq!(
        r.reason_code.as_deref(),
        Some("grant_consumed"),
        "真实消费后 busy 回队不退还授权——重放终态拒绝"
    );
    assert_eq!(
        h.state_of("byp-1"),
        ActionState::Rejected,
        "拒绝即终态（不二投）"
    );

    // ④ 对照：另一条带授权新意见同样真实消费 + busy 回队（授权 #2 各自单次有效）
    let (byp2, _pos2) = queued(
        h.router
            .send(
                h.ctx("byp-2", ActionKind::Send),
                "第二条越队也撞写前忙".into(),
                false,
                true,
            )
            .await,
    );
    assert_eq!(byp2, "byp-2");
    assert_eq!(h.state_of("byp-2"), ActionState::Queued);

    // ⑤ 写前忙解除 + 显式恢复队列 → 回队动作按 FIFO 排在旧队列之后派发
    //    （byp-2 不凭「曾经越队」二次插队——一次授权只买一次插队）
    h.dispatch.set_busy_before_write(false);
    assert!(h.store.clear_queue_pause(&h.key).expect("resume"));
    let out = h.router.flush_thread(&h.key).await.expect("恢复后批次接力");
    assert_eq!(completed(out).delivery, Delivery::Accepted);
    assert_eq!(
        h.dispatch.recorded_composed(),
        vec!["旧一\n\n旧二\n\n第二条越队也撞写前忙"]
    );
    assert_eq!(h.state_of("byp-1"), ActionState::Rejected);
    assert_eq!(h.state_of("byp-2"), ActionState::Accepted);
}

/// R06 反例：优先处理的授权在首次写出前被真实消费后遭遇写前忙（busy 回队、授权
/// 不退还）→ **完全相同的重试**不得把原消息终态拒绝（修复前：重试 reserve 后命中
/// `grant_consumed` → finish → 原消息 rejected、queued=false）。契约：busy 零投递
/// 后的下一次用户授权 = 独立、可 CAS 的新 attempt；原消息最终最多投递一次；旧
/// grant 永不复活（未重新铸造前，已消费授权授权不了任何写出）。
#[tokio::test]
async fn prioritize_retry_after_busy_requeue_mints_new_attempt_and_delivers_once() {
    use multi_agents_manager_lib::services::codex_control::{
        Deadline, PrioritizeOutcome, BUSY_BEFORE_WRITE_REASON,
    };
    let h = harness_with(FakeDispatch::accepted());
    // ① 旧队列两条 + 目标一条（忙态 FIFO 入队——越队语义的对照面）
    h.dispatch.set_busy(true);
    for (id, text) in [("old-a", "旧一"), ("old-b", "旧二")] {
        queued(
            h.router
                .send(h.ctx(id, ActionKind::Send), text.into(), true, false)
                .await,
        );
    }
    queued(
        h.router
            .send(h.ctx("q3", ActionKind::Send), "目标三".into(), true, false)
            .await,
    );
    h.dispatch.set_busy(false);
    // ② 队列已暂停（R04：越队只允许「空闲且已暂停」）
    h.store
        .set_queue_pause(
            &h.key,
            "interrupt_accepted",
            "test-token",
            Some("turn-pause"),
        )
        .unwrap();

    // ③ 首次优先：授权在写出边界真实消费 → 写前忙 → 冻结回队（零 start-turn）
    h.dispatch.set_busy_before_write(true);
    {
        let lock = h
            .router
            .thread_dispatch_lock(&h.key)
            .try_lock_owned()
            .unwrap();
        let out = h
            .router
            .prioritize_queued("q3", &h.key, Deadline::none(), lock)
            .await;
        match out {
            PrioritizeOutcome::StillQueued(reason) => {
                assert_eq!(reason, BUSY_BEFORE_WRITE_REASON, "首次 = 写前忙回队");
            }
            other => panic!("expected StillQueued(busy_before_write), got {other:?}"),
        }
    }
    assert_eq!(h.dispatch.send_count(), 0, "写前忙零 start-turn");
    assert_eq!(h.state_of("q3"), ActionState::Queued, "冻结动作回队留队");
    assert!(
        !h.store.consume_bypass_grant("q3", &h.key).unwrap(),
        "前置：授权已随首次派发在写出前真实消费（不退还）"
    );

    // ④ 完全相同的重试（同一 actionId、下一次用户授权）：不得 grant_consumed
    //    终态拒绝；原消息最终最多投递一次；旧队列零帧
    h.dispatch.set_busy_before_write(false);
    let receipt = {
        let lock = h
            .router
            .thread_dispatch_lock(&h.key)
            .try_lock_owned()
            .unwrap();
        match h
            .router
            .prioritize_queued("q3", &h.key, Deadline::none(), lock)
            .await
        {
            PrioritizeOutcome::Completed(r) => r,
            other => panic!("retry should complete, got {other:?}"),
        }
    };
    assert_eq!(
        receipt.delivery,
        Delivery::Accepted,
        "重试经新 attempt 恰好一次投递（不得 grant_consumed 终态拒绝）：{receipt:?}"
    );
    assert_eq!(
        h.state_of("q3"),
        ActionState::Accepted,
        "原消息不被旧授权已消费作废"
    );
    assert_eq!(h.dispatch.writes(), 1, "原消息最终最多投递一次");
    assert_eq!(
        h.dispatch.recorded_composed(),
        vec!["目标三"],
        "只有目标被越队写出（旧队列零帧）"
    );
    assert_eq!(h.state_of("old-a"), ActionState::Queued, "旧队列不受影响");
    assert_eq!(h.state_of("old-b"), ActionState::Queued, "旧队列不受影响");
}

/// 暂停期间回合终态证据照常释放顺序屏障（release_turn_if_complete = true），
/// 但**队列不自动恢复**——flush 仍为 None，只有显式 resume 解除。
#[tokio::test]
async fn pause_never_auto_resumes_on_turn_completion() {
    let h = harness_with(FakeDispatch::accepted_turn("turn-1", None));
    // T1 派发受理（绑定 turn-1 顺序限制）
    let r = completed(
        h.router
            .send(h.ctx("t1", ActionKind::Send), "第一条".into(), false, false)
            .await,
    );
    assert_eq!(r.delivery, Delivery::Accepted);
    // 打断受理 → 暂停
    h.store
        .set_queue_pause(
            &h.key,
            "interrupt_accepted",
            "test-token",
            Some("turn-pause"),
        )
        .unwrap();
    // 回合完成证据齐备（终态 + idle 新鲜投影）→ 屏障释放
    h.dispatch.set_terminal_turn("turn-1");
    assert!(
        h.router
            .release_turn_if_complete(&h.key, &minimal_projection(THREAD, "idle")),
        "屏障照常经终态证据释放"
    );
    // 但队列不自动恢复
    let (q, _p) = queued(
        h.router
            .send(
                h.ctx("t2", ActionKind::Send),
                "打断后排队".into(),
                true,
                false,
            )
            .await,
    );
    assert!(
        h.router.flush_thread(&h.key).await.is_none(),
        "暂停仍冻结队列"
    );
    assert_eq!(h.state_of(&q), ActionState::Queued);
    // 显式 resume 后接力
    h.store.clear_queue_pause(&h.key).unwrap();
    let out = h.router.flush_thread(&h.key).await.expect("resumed");
    assert_eq!(completed(out).action_id, q);
}

// ============================================================
// PR #165 复审三项修复
// ============================================================

/// 事项①（复审 Important）：T1 派发在飞（慢派发挂起，Thread guard 由 T1 持到落账）
/// 时，同 thread 新 send（T2）**立即**返回 Queued（HTTP 响应不阻塞），行在账排队；
/// T1 落账并释放 Turn 屏障后 T2 被调度循环同款流程（release → flush_thread）接力。
/// 改前 T2 会挂在 `lock_owned().await` 直到 T1 落账（分钟级）。
#[tokio::test]
async fn send_returns_queued_immediately_while_dispatch_in_flight() {
    let gate = Arc::new(tokio::sync::Notify::new());
    let h = harness_with(FakeDispatch::accepted_gated(gate.clone()));
    let Harness {
        store,
        dispatch,
        router,
        key,
        ..
    } = h;
    let router = Arc::new(router);

    // T1：队首 send——进入 A 路独立派发任务后在慢派发内挂起
    let ctx1 = ActionContext {
        action_id: "t1".into(),
        device_id: "dev-1".into(),
        device_name: "iPhone".into(),
        kind: ActionKind::Send,
        key: key.clone(),
    };
    let r1 = {
        let router = router.clone();
        tokio::spawn(async move { router.send(ctx1, "first".into(), false, false).await })
    };
    wait_for_state_for(&store, "t1", ActionState::Reserved).await;

    // T2：新 send 撞在飞派发——必须立即返回 Queued，不等待 T1 落账
    let ctx2 = ActionContext {
        action_id: "t2".into(),
        device_id: "dev-1".into(),
        device_name: "iPhone".into(),
        kind: ActionKind::Send,
        key: key.clone(),
    };
    let outcome = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        router.send(ctx2, "second".into(), false, false),
    )
    .await
    .expect("T2 send 不得被在飞派发阻塞（改前此处超时）");
    let (aid2, pos) = queued(outcome);
    assert_eq!(aid2, "t2");
    assert_eq!(
        pos, 1,
        "position 用既有队列视图（T1 已 reserved 出队，T2 是唯一 queued 行）"
    );
    assert_eq!(
        store.get_for_device("t2", "dev-1").unwrap().unwrap().state,
        ActionState::Queued,
        "T2 行在账排队"
    );

    // 开门放行 T1 → A 路落账 accepted（guard 随 send 返回释放）
    dispatch.release_send();
    let receipt1 = completed(r1.await.unwrap());
    assert_eq!(receipt1.delivery, Delivery::Accepted);
    assert_eq!(
        store.get_for_device("t1", "dev-1").unwrap().unwrap().state,
        ActionState::Accepted
    );

    // 接力（调度循环同款流程）：Turn 终态证据 → release → flush_thread 派 T2
    dispatch.set_terminal_turn("turn-1");
    dispatch.disarm_send();
    assert!(
        router.release_turn_if_complete(&key, &minimal_projection(THREAD, "idle")),
        "T1 的 Turn 屏障经终态证据释放"
    );
    let receipt2 = completed(router.flush_thread(&key).await.expect("T2 被接力"));
    assert_eq!(receipt2.delivery, Delivery::Accepted);
    assert_eq!(
        store.get_for_device("t2", "dev-1").unwrap().unwrap().state,
        ActionState::Accepted
    );
    assert_eq!(dispatch.send_count(), 2, "T2 恰好一次真派发");
}

/// 轮询等待动作到达指定状态（当前线程运行时下，await 让 spawned 任务推进）。
async fn wait_for_state_for(store: &Arc<ActionStore>, action_id: &str, want: ActionState) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        if store
            .get_for_device(action_id, "dev-1")
            .unwrap()
            .is_some_and(|v| v.state == want)
        {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "等待 {action_id} → {want:?} 超时"
        );
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
}

// 【S2】原 `send_returns_queued_while_fallback_permit_outstanding`（兜底 permit 在外
// 时新 send 立即返回 Queued）随兜底删除；其「在飞派发不阻塞新 send」的等价语义由
// 既有 `send_returns_queued_immediately_while_dispatch_in_flight`（A 路慢派发挂起
// + try-lock 让路）继续钉住，不再重复造例。

/// 事项②（复审 Important）：超期终态行被启动墓碑化——重载荷置 NULL 但**行保留**；
/// actionId 重放（同载荷）经 payload_hash 幂等比对命中既有终态：如实返回 accepted
///（receipt_missing 合成口径），绝不回 queued、绝不重派发。
#[tokio::test]
async fn tombstoned_terminal_replay_stays_terminal() {
    let h = harness();
    // 真实生命周期造 accepted 行（finish_with_turn_barrier 在册屏障 → 终态证据释放清屏障）
    let outcome = h
        .router
        .send(
            h.ctx("t-old", ActionKind::Send),
            "老消息".into(),
            false,
            false,
        )
        .await;
    assert_eq!(completed(outcome).delivery, Delivery::Accepted);
    h.dispatch.set_terminal_turn("turn-1");
    assert!(h
        .router
        .release_turn_if_complete(&h.key, &minimal_projection(THREAD, "idle")));
    // 手工把行推到保留期之外（updated_at 改老）
    {
        let conn = h.conn.lock().unwrap();
        conn.execute(
            "UPDATE codex_actions SET updated_at = 1000 WHERE action_id = 't-old'",
            [],
        )
        .unwrap();
    }
    // 新一轮「启动」：ActionStore::new 构造即墓碑化
    let store2 = Arc::new(ActionStore::new(DeviceStore::Owned(h.conn.clone())));
    {
        let conn = h.conn.lock().unwrap();
        let receipt: Option<String> = conn
            .query_row(
                "SELECT receipt_json FROM codex_actions WHERE action_id = 't-old'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(receipt, None, "回执重载荷已被墓碑化");
    }
    assert_eq!(
        store2
            .get_for_device("t-old", "dev-1")
            .unwrap()
            .unwrap()
            .state,
        ActionState::Accepted,
        "墓碑行保留（不删行，状态列保全）"
    );
    // 重放：同 actionId 同载荷 → 幂等命中既有终态，不回 queued、不重派发
    let router2 = CodexControlRouter::new(store2, h.dispatch.clone());
    let outcome = router2
        .send(
            h.ctx("t-old", ActionKind::Send),
            "老消息".into(),
            false,
            false,
        )
        .await;
    let receipt = completed(outcome);
    assert_eq!(
        receipt.delivery,
        Delivery::Accepted,
        "墓碑行重放仍返回既有终态"
    );
    assert_eq!(receipt.reason_code.as_deref(), Some("receipt_missing"));
    assert_eq!(h.dispatch.send_count(), 1, "绝不重派发");
}

// ============================================================
// S3：统一预算与有界后台收尾（前台等待者与派发动作解耦，设计 §5.2/§5.3）
// ============================================================

use multi_agents_manager_lib::services::codex_control::{
    FRONT_BUDGET_ELAPSED_REASON, WRAP_UP_ELAPSED_REASON,
};
use multi_agents_manager_lib::services::codex_desktop_ipc::budget::Deadline as S3Deadline;
use std::time::Duration;

/// R1c：前台预算到期返回「待确认」后，派发任务**已分离**——HTTP future 被取消
/// 也不丢动作、不二次写入；账本最终收敛 accepted。
#[tokio::test]
async fn front_expiry_detached_task_keeps_action_and_single_write() {
    let gate = Arc::new(tokio::sync::Notify::new());
    let h = harness_with(FakeDispatch::accepted_gated(gate.clone()));
    h.router.set_wrap_up_ms(3_000);
    let ctx = h.ctx("act-front-detached", ActionKind::Send);
    let router = h.router.clone();
    let task = tokio::spawn(async move {
        router
            .send_bounded(
                ctx,
                "前台分离消息".into(),
                false,
                false,
                S3Deadline::after_ms(200),
            )
            .await
    });
    let r = tokio::time::timeout(Duration::from_secs(3), task)
        .await
        .expect("前台预算必须结束等待")
        .unwrap();
    let receipt = completed(r);
    assert_eq!(receipt.delivery, Delivery::UnknownDelivery, "{receipt:?}");
    assert_eq!(
        receipt.reason_code.as_deref(),
        Some(FRONT_BUDGET_ELAPSED_REASON)
    );
    // 账本仍是 reserved（临时回执不落账；派发任务继续收尾）
    assert_eq!(h.state_of("act-front-detached"), ActionState::Reserved);
    // 模拟等待者彻底丢失后，派发完成：动作收敛 + 恰好一次写入
    gate.notify_one();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    loop {
        if h.state_of("act-front-detached") == ActionState::Accepted {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "分离派发任务必须完成落账"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(
        h.dispatch.send_count(),
        1,
        "取消/超时路径不得触发第二次写入"
    );
}

/// S3：RPC 超时后在收尾窗口内迟到的有效回执——派发回执收敛为真实终态
/// （`late_result`），原动作落 accepted，不重发（send 恰好一次）。
#[tokio::test]
async fn late_result_within_wrapup_converges_original_action() {
    let d = FakeDispatch::new(DispatchOutcome {
        delivery: Delivery::UnknownDelivery,
        envelope_id: Some("env-late".into()),
        turn_id: None,
        reason_code: Some("timeout".into()),
        revision: None,
    });
    d.set_late_poll_ms(10);
    let h = harness_with(d);
    h.router.set_wrap_up_ms(2_000);
    // 迟到回执在 T 进入迟到等待后投放（100ms < wrap 2s）
    let dispatch = h.dispatch.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        dispatch.deliver_late(
            "env-late",
            DispatchOutcome {
                delivery: Delivery::Accepted,
                envelope_id: Some("env-late".into()),
                turn_id: Some("turn-late".into()),
                reason_code: None,
                revision: None,
            },
        );
    });
    let out = h
        .router
        .send_bounded(
            h.ctx("act-late-ctl", ActionKind::Send),
            "迟到收敛".into(),
            false,
            false,
            S3Deadline::none(),
        )
        .await;
    let receipt = completed(out);
    assert_eq!(receipt.delivery, Delivery::Accepted, "{receipt:?}");
    assert_eq!(receipt.reason_code.as_deref(), Some("late_result"));
    assert_eq!(receipt.envelope_id.as_deref(), Some("env-late"));
    assert_eq!(h.state_of("act-late-ctl"), ActionState::Accepted);
    assert_eq!(h.dispatch.send_count(), 1, "迟到收敛不重发");
}

/// R2b：后台主动收尾预算（自实际派发起算）到点**有界结束**——动作按 unknown 终态
/// 落账（可能已写出 → 绝不 NotSent），持久保留、单次写入。
#[tokio::test]
async fn wrapup_budget_ends_bounded_and_keeps_persistent_action() {
    let gate = Arc::new(tokio::sync::Notify::new());
    let h = harness_with(FakeDispatch::accepted_gated(gate));
    h.router.set_wrap_up_ms(300);
    let ctx = h.ctx("act-wrapup", ActionKind::Send);
    let started = std::time::Instant::now();
    let out = tokio::time::timeout(
        Duration::from_secs(3),
        h.router
            .send_bounded(ctx, "收尾消息".into(), false, false, S3Deadline::none()),
    )
    .await
    .expect("收尾预算必须有界结束（红=无预算时挂在派发门上）");
    let elapsed = started.elapsed();
    let receipt = completed(out);
    assert_eq!(receipt.delivery, Delivery::UnknownDelivery, "{receipt:?}");
    assert_eq!(receipt.reason_code.as_deref(), Some(WRAP_UP_ELAPSED_REASON));
    assert!(
        elapsed >= Duration::from_millis(250) && elapsed <= Duration::from_millis(1_500),
        "收尾应发生在预算量级，实际 {elapsed:?}"
    );
    // 持久动作保留：unknown 终态（不 NotSent、不永久 reserved）
    assert_eq!(h.state_of("act-wrapup"), ActionState::Unknown);
    assert_eq!(h.dispatch.send_count(), 1, "收尾不触发第二次写入");
}

/// R2c：队列驻留超过前台预算窗口**不清除、照常派发**；后台收尾预算自**实际派发
/// 开始**计（排队驻留不占——设计 §5.3）。
#[tokio::test]
async fn queue_residence_exceeds_front_then_dispatch_with_fresh_wrap_budget() {
    let gate = Arc::new(tokio::sync::Notify::new());
    let h = harness_with(FakeDispatch::accepted_gated(gate.clone()));
    h.router.set_wrap_up_ms(400);
    // 忙态 → 排队（「已排队」返回即结束前台等待；预算不约束排队驻留）
    h.dispatch.set_busy(true);
    let (id, _) = queued(
        h.router
            .send_bounded(
                h.ctx("q-reside", ActionKind::Send),
                "排队驻留".into(),
                false,
                false,
                S3Deadline::after_ms(200),
            )
            .await,
    );
    // 驻留超过前台预算窗口（200ms）——动作必须原样保留
    tokio::time::sleep(Duration::from_millis(450)).await;
    assert_eq!(
        h.state_of(&id),
        ActionState::Queued,
        "队列驻留不因前台预算被清除"
    );
    // 解除忙态 → flush 派发队首；此刻才开始计收尾预算（400ms）
    h.dispatch.set_busy(false);
    let flush_started = std::time::Instant::now();
    let out = tokio::time::timeout(Duration::from_secs(3), h.router.flush_thread(&h.key))
        .await
        .expect("收尾预算必须有界结束（红=无预算时挂在派发门上）")
        .expect("队首可派发");
    let wrap_elapsed = flush_started.elapsed();
    let receipt = completed(out);
    assert_eq!(receipt.delivery, Delivery::UnknownDelivery, "{receipt:?}");
    assert_eq!(receipt.reason_code.as_deref(), Some(WRAP_UP_ELAPSED_REASON));
    assert!(
        wrap_elapsed >= Duration::from_millis(350) && wrap_elapsed <= Duration::from_millis(1_200),
        "收尾预算必须自实际派发起算（若从入队计，此刻已到期会立即结束）；实际 {wrap_elapsed:?}"
    );
    assert_eq!(h.state_of(&id), ActionState::Unknown);
    assert_eq!(h.dispatch.send_count(), 1);
}

// ============================================================
// S4：Unknown 的人工继续（设计 §6.2）——撤销旧派发权 + 解除屏障 + 唯一后继一致完成；
// 旧编排恢复零首次写出；并发一后继；同 ID 异载荷拒；确认期间收敛返回新状态；
// 重启/迟到结果不复活写出权。
// ============================================================

use multi_agents_manager_lib::services::codex_control::ManualResume;

/// 造「unknown + 保守屏障」行（真实 payload hash，供人工继续的正文核验）：
/// register(原文) → reserve → finish_with_turn_barrier(Unknown, 缺 turnId)。
fn seed_unknown_with_text(h: &Harness, action_id: &str, text: &str) -> String {
    let hash = payload_hash(
        ActionKind::Send,
        &h.key,
        None,
        None,
        &ActionPayload::Text(text.into()),
    );
    h.store
        .register(
            &h.ctx(action_id, ActionKind::Send),
            ActionKind::Send,
            None,
            None,
            &hash,
            Some(text),
        )
        .expect("register");
    assert!(h
        .store
        .reserve(action_id, ControlRoute::DesktopIpc)
        .unwrap());
    h.store
        .finish_with_turn_barrier(
            &ActionReceipt {
                action_id: action_id.to_string(),
                delivery: Delivery::UnknownDelivery,
                route: ControlRoute::DesktopIpc,
                turn_id: None,
                envelope_id: Some("env-old".into()),
                reason_code: Some("wrapup_elapsed".into()),
            },
            None,
        )
        .expect("barrier");
    hash
}

/// 【红测 R-F2】旧请求停在探测（reserved、send_text 挂在派发门上）时人工继续：
/// 旧编排恢复后**零首次写出**；旧动作定格 Unknown（不写 NotSent/Cancelled）、
/// 屏障不复活；后继正常登记。
#[tokio::test]
async fn manual_resume_on_parked_dispatch_yields_zero_first_write() {
    let gate = Arc::new(tokio::sync::Notify::new());
    let h = harness_with(FakeDispatch::accepted_gated(gate.clone()));
    // 旧请求真实派发、停在探测：T 的 send_text 挂在派发门上，动作保持 reserved
    let ctx = h.ctx("act-old", ActionKind::Send);
    let router = h.router.clone();
    let send_task = tokio::spawn(async move {
        router
            .send_bounded(
                ctx,
                "停在探测的原文".into(),
                false,
                false,
                S3Deadline::none(),
            )
            .await
    });
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    loop {
        // 派发任务异步登记：行可能尚未落库，缺行不算失败
        if h.store
            .get_for_device("act-old", "dev-1")
            .ok()
            .flatten()
            .is_some_and(|v| v.state == ActionState::Reserved)
        {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "旧动作应已进入派发（reserved）"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // 人工继续：撤销尚未取得的首次写出权 + 定格 unknown + 建后继（同一持久事务）
    let r = h
        .store
        .manual_resume(
            "act-old",
            "dev-1",
            "iPhone",
            "act-new-1",
            "停在探测的原文",
            Some("m"),
        )
        .expect("store call");
    let ManualResume::Resumed {
        view,
        successor_action_id,
        ..
    } = &r
    else {
        panic!("人工继续必须成功建后继，实际 {r:?}");
    };
    assert_eq!(
        view.state,
        ActionState::Unknown,
        "旧动作保持 Unknown 不改写"
    );
    assert_eq!(successor_action_id, "act-new-1");
    assert!(
        h.store.has_write_revoke("act-old"),
        "旧动作的写出权必须已撤销"
    );
    assert!(
        !h.store.has_compat_fence(&h.key).unwrap(),
        "屏障已解除（reserved 定格臂不复活屏障）"
    );

    // 旧派发任务恢复（门开）：写出边界检查命中撤销标记 → 零首次写出
    h.dispatch.set_write_revoke_check({
        let store = h.store.clone();
        Arc::new(move |aid: &str| store.has_write_revoke(aid))
    });
    gate.notify_one();
    let _ = tokio::time::timeout(Duration::from_secs(3), send_task)
        .await
        .expect("旧派发任务必须有界收尾");

    // 旧动作仍 Unknown（不被任何迟到/恢复臂改写成 NotSent/Cancelled）；零首次写出
    assert_eq!(h.state_of("act-old"), ActionState::Unknown);
    assert!(!h.store.has_compat_fence(&h.key).unwrap(), "收尾不复活屏障");
    assert_eq!(h.dispatch.writes(), 0, "旧编排恢复零首次写出（帧计数）");
    assert_eq!(
        h.dispatch.send_count(),
        1,
        "尝试记录在案（send_count = 尝试数，真实写出为 0）"
    );
    // 后继已登记为 queued（尚未派发——派发由端点组合的发送路径承担）
    assert_eq!(h.state_of("act-new-1"), ActionState::Queued);
}

/// 【红测 R-C1】人工确认期间原动作已收敛 → 返回最新结果，不按过期确认重发、
/// 不建后继。
#[tokio::test]
async fn manual_resume_converged_returns_latest_without_resend() {
    let h = harness();
    seed_unknown_with_text(&h, "act-c", "已收敛的原文");

    // 确认期间：迟到真实回执收敛原动作（accepted + turnId）
    h.dispatch.deliver_late(
        "env-old",
        DispatchOutcome {
            delivery: Delivery::Accepted,
            envelope_id: Some("env-old".into()),
            turn_id: Some("turn-late".into()),
            reason_code: None,
            revision: None,
        },
    );
    // 等价复刻调度循环对账兜底（T 之外的证据消费口）
    let receipt = ActionReceipt {
        action_id: "act-c".into(),
        delivery: Delivery::Accepted,
        route: ControlRoute::DesktopIpc,
        turn_id: Some("turn-late".into()),
        envelope_id: Some("env-old".into()),
        reason_code: Some("late_result".into()),
    };
    assert!(
        h.store.finish_late_result(&receipt).expect("consume"),
        "迟到证据归还原动作"
    );
    assert_eq!(h.state_of("act-c"), ActionState::Accepted);

    // 过期的人工继续确认：返回最新状态，零发送、零后继
    let r = h
        .store
        .manual_resume(
            "act-c",
            "dev-1",
            "iPhone",
            "act-c-new",
            "已收敛的原文",
            Some("m"),
        )
        .expect("store call");
    let ManualResume::Converged { view } = &r else {
        panic!("确认期间已收敛必须返回最新状态，实际 {r:?}");
    };
    assert_eq!(view.state, ActionState::Accepted);
    assert!(h.store.next_queued(&h.key).unwrap().is_none(), "不建后继");
}

/// 【红测 R-C2】两设备并发人工继续最多一个后继：持久 CAS 保证第二个设备得到
/// 既有后继（最新状态），它自带的新 actionId 不登记。
#[tokio::test]
async fn manual_resume_concurrent_devices_single_successor() {
    let h = harness();
    seed_device(&h.conn, "dev-2", "iPad");
    seed_unknown_with_text(&h, "act-race", "并发原文");

    // 两台设备并发（同一持久账本上由 DeviceStore 锁 + 主键 CAS 串行化）
    let (r1, r2) = {
        let s1 = h.store.clone();
        let s2 = h.store.clone();
        let (a, b) = tokio::join!(
            tokio::task::spawn_blocking(move || {
                s1.manual_resume(
                    "act-race",
                    "dev-1",
                    "iPhone",
                    "succ-a",
                    "并发原文",
                    Some("d1"),
                )
            }),
            tokio::task::spawn_blocking(move || {
                s2.manual_resume(
                    "act-race",
                    "dev-2",
                    "iPad",
                    "succ-b",
                    "并发原文",
                    Some("d2"),
                )
            }),
        );
        (a.unwrap().unwrap(), b.unwrap().unwrap())
    };
    let (winner, loser) = match (&r1, &r2) {
        (
            ManualResume::Resumed {
                successor_action_id,
                ..
            },
            other,
        )
        | (
            other,
            ManualResume::Resumed {
                successor_action_id,
                ..
            },
        ) => (successor_action_id.clone(), other),
        _ => panic!("必须恰有一台设备成功建后继：{r1:?} / {r2:?}"),
    };
    let ManualResume::AlreadyResumed {
        successor_action_id: loser_succ,
        ..
    } = loser
    else {
        panic!("输家必须拿到既有后继：{loser:?}");
    };
    assert_eq!(loser_succ, &winner, "输家收到赢家的后继（最新状态）");
    // 胜者后继直读（胜者可能是 dev-2 的请求，get_for_device 的隔离视角不适用）
    let winner_state: String = {
        let conn = h.conn.lock().unwrap();
        conn.query_row(
            "SELECT state FROM codex_actions WHERE action_id = ?1",
            [&winner],
            |r| r.get(0),
        )
        .unwrap()
    };
    assert_eq!(winner_state, "queued", "唯一后继在队");
    let loser_id = match loser_succ.as_str() {
        "succ-a" => "succ-b",
        _ => "succ-a",
    };
    assert!(
        h.store.get_for_device(loser_id, "dev-1").unwrap().is_none(),
        "输家自带的新 actionId 不登记（不产生第二个后继）"
    );
}

/// 【红测 R-C3】后继仍须通过目标/pending 等守卫（不强行穿透）；同 ID 异载荷拒绝
/// 不留残渣（事务回滚后可携正确载荷重试）；原动作保持 Unknown。
#[tokio::test]
async fn manual_resume_successor_passes_guards_and_mismatch_rolls_back() {
    let h = harness();
    seed_unknown_with_text(&h, "act-g", "守卫原文");

    // 同 ID 异载荷：正文与原动作 payload 不符 → 409 语义（TextMismatch），零写入
    let r = h
        .store
        .manual_resume(
            "act-g",
            "dev-1",
            "iPhone",
            "succ-x",
            "被调包的正文",
            Some("m"),
        )
        .expect("store call");
    assert!(
        matches!(r, ManualResume::TextMismatch { .. }),
        "异载荷拒绝：{r:?}"
    );
    assert!(h.store.has_compat_fence(&h.key).unwrap(), "拒绝不触碰屏障");
    assert!(!h.store.has_write_revoke("act-g"), "拒绝不撤销写出权");
    assert!(
        h.store.next_queued(&h.key).unwrap().is_none(),
        "拒绝不建后继"
    );

    // 正常继续 + 后继撞 pending：守卫拒绝（native_pending），后继终态、不二投
    h.dispatch.set_send_gate(SendGate::NativePending);
    let r = h
        .store
        .manual_resume("act-g", "dev-1", "iPhone", "succ-g", "守卫原文", Some("m"))
        .expect("store call");
    let ManualResume::Resumed {
        successor_action_id,
        ..
    } = &r
    else {
        panic!("人工继续成功：{r:?}");
    };
    assert_eq!(successor_action_id, "succ-g");
    h.router.force_clear_turn_limit(&h.key, "act-g");
    let out = h
        .router
        .send_bounded(
            h.ctx("succ-g", ActionKind::Send),
            "守卫原文".into(),
            false,
            false,
            S3Deadline::none(),
        )
        .await;
    let receipt = completed(out);
    assert_eq!(receipt.reason_code.as_deref(), Some("native_pending"));
    assert_eq!(h.dispatch.send_count(), 0, "pending 不被人工继续穿透");
    assert_eq!(
        h.state_of("act-g"),
        ActionState::Unknown,
        "原动作保持 Unknown"
    );

    // 后继已定（唯一）：重复人工继续幂等返回既有后继
    h.dispatch.set_send_gate(SendGate::Clear);
    let r2 = h
        .store
        .manual_resume("act-g", "dev-1", "iPhone", "succ-g2", "守卫原文", Some("m"))
        .expect("store call");
    let ManualResume::AlreadyResumed {
        successor_action_id: s2,
        ..
    } = &r2
    else {
        panic!("重复继续必须幂等：{r2:?}");
    };
    assert_eq!(s2, "succ-g", "仍指向唯一后继");
    assert!(
        h.store
            .get_for_device("succ-g2", "dev-1")
            .unwrap()
            .is_none(),
        "不建立重复后继"
    );
}

/// 【红测 R-C4】人工继续后：重启与旧动作迟到结果都不得复活写出权——旧动作可补记
/// 真实结果（迟到最后仍能收敛 accepted），但绝不重新派发、不复活屏障、不撤销后继。
#[tokio::test]
async fn manual_resume_restart_and_late_result_never_revive_write() {
    let mut h = harness();
    seed_unknown_with_text(&h, "act-rev", "复活原文");
    let r = h
        .store
        .manual_resume(
            "act-rev",
            "dev-1",
            "iPhone",
            "succ-rev",
            "复活原文",
            Some("m"),
        )
        .expect("store call");
    assert!(matches!(r, ManualResume::Resumed { .. }));

    // —— 重启（同库重建 Store/Router）——
    h.restart();

    // 重启后旧动作迟到真实回执：补记真实结果（unknown → accepted），零新增派发、
    // 屏障不复活（barrier_force_released_at 单调守卫）
    h.dispatch.deliver_late(
        "env-old",
        DispatchOutcome {
            delivery: Delivery::Accepted,
            envelope_id: Some("env-old".into()),
            turn_id: Some("turn-revived".into()),
            reason_code: None,
            revision: None,
        },
    );
    let receipt = ActionReceipt {
        action_id: "act-rev".into(),
        delivery: Delivery::Accepted,
        route: ControlRoute::DesktopIpc,
        turn_id: Some("turn-revived".into()),
        envelope_id: Some("env-old".into()),
        reason_code: Some("late_result".into()),
    };
    assert!(
        h.store.finish_late_result(&receipt).expect("consume"),
        "人工解除后迟到真实结果仍可补记（不被单调守卫挡死）"
    );
    assert_eq!(h.state_of("act-rev"), ActionState::Accepted, "补记真实结果");
    assert!(
        !h.store.has_compat_fence(&h.key).unwrap(),
        "迟到补记不复活屏障"
    );
    assert_eq!(h.dispatch.send_count(), 0, "迟到结果不重新派发旧动作");

    // 重放旧 actionId：返回账本既有状态（accepted），零新增派发；写出权不复活
    let replay = completed(
        h.router
            .send(
                h.ctx("act-rev", ActionKind::Send),
                "复活原文".into(),
                false,
                false,
            )
            .await,
    );
    assert_eq!(replay.delivery, Delivery::Accepted, "重放返回既有终态");
    assert_eq!(h.dispatch.send_count(), 0, "重放零新增派发（写出权不复活）");
    assert!(h.store.has_write_revoke("act-rev"), "撤销标记永不删除");
    assert_eq!(h.state_of("succ-rev"), ActionState::Queued, "后继不受影响");
}

/// 【红测 R-C5】旧动作迟到后补记真实结果时不得重新取得新动作的派发权、不得撤销
/// 用户的队列暂停选择：暂停在册时后继照常被队列守卫拒绝（queue_paused）。
#[tokio::test]
async fn manual_resume_respects_queue_pause_choice() {
    let h = harness();
    seed_unknown_with_text(&h, "act-p", "暂停原文");
    // 旧队列里再排一条（有内容可「越」的形态）
    h.router
        .send(
            h.ctx("act-p-older", ActionKind::Send),
            "更早排队".into(),
            true,
            false,
        )
        .await;
    h.store
        .set_queue_pause(
            &h.key,
            "interrupt_accepted",
            "test-token",
            Some("turn-pause"),
        )
        .unwrap();

    let r = h
        .store
        .manual_resume("act-p", "dev-1", "iPhone", "succ-p", "暂停原文", Some("m"))
        .expect("store call");
    assert!(matches!(r, ManualResume::Resumed { .. }));
    // 用户队列暂停选择不被人工继续撤销
    assert!(
        h.store.queue_pause(&h.key).unwrap().is_some(),
        "人工继续不撤销队列暂停"
    );
    // 后继经正常发送路径：暂停 + 旧队列有内容 → 入队但不派发（C05），不越队
    h.router.force_clear_turn_limit(&h.key, "act-p");
    let out = h
        .router
        .send_bounded(
            h.ctx("succ-p", ActionKind::Send),
            "暂停原文".into(),
            false,
            false,
            S3Deadline::none(),
        )
        .await;
    assert_eq!(
        queued(out).0,
        "succ-p",
        "留在排队态等用户处置（不是终态拒绝）"
    );
    assert_eq!(h.state_of("succ-p"), ActionState::Queued);
    assert_eq!(h.dispatch.send_count(), 0, "暂停不因人工继续失效");
}

// ============================================================
// T04：前台预算到期 → 分离的有界落账（不取消任务、不假报 NotSent）
// ============================================================

/// 前台 30ms 到期时回 Unknown；落账任务继续（门打开后按真结果收尾）——
/// HTTP 断开/前台到期都不得把"可能已写出"写成 NotSent，也不得丢账。
#[tokio::test]
async fn f08p_t04_front_timeout_detaches_bounded_settlement() {
    use multi_agents_manager_lib::services::codex_desktop_ipc::budget::Deadline;
    let gate = Arc::new(tokio::sync::Notify::new());
    let dispatch = FakeDispatch::accepted();
    *dispatch.respond_gate.lock().unwrap() = Some(gate.clone());
    let h = harness_with(dispatch);
    let ctx = h.ctx("act-t04-front", ActionKind::CommandApproval);

    let outcome = h
        .router
        .respond_bounded(
            ctx.clone(),
            "n:18".into(),
            "tok".into(),
            PendingAction::Approve,
            Deadline::after_ms(30),
            Default::default(),
        )
        .await;
    let receipt = match outcome {
        RouterOutcome::Completed(r) => r,
        _ => panic!("unexpected outcome"),
    };
    assert_eq!(
        receipt.delivery,
        Delivery::UnknownDelivery,
        "前台到期必须如实未知（不得记 NotSent / 不得宣称成功）"
    );
    assert_eq!(receipt.reason_code.as_deref(), Some("delivery_unknown"));
    // 任务仍在册：门打开后按真结果落账（Accepted）。
    gate.notify_waiters();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let view = h
            .store
            .get_for_device(&ctx.action_id, &ctx.device_id)
            .unwrap();
        if let Some(view) = view {
            if view.state != ActionState::Queued && view.state != ActionState::Reserved {
                assert_eq!(view.state, ActionState::Accepted, "分离收尾按真结果落账");
                break;
            }
        }
        assert!(
            std::time::Instant::now() < deadline,
            "分离收尾未在有界时间内落账"
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
}

/// 前台预算为 0 → **零派发**（不登记、不 reserve、不出帧）。
#[tokio::test]
async fn f08p_t04_zero_front_budget_means_zero_dispatch() {
    use multi_agents_manager_lib::services::codex_desktop_ipc::budget::Deadline;
    let h = harness();
    let ctx = h.ctx("act-t04-zero", ActionKind::CommandApproval);
    let outcome = h
        .router
        .respond_bounded(
            ctx.clone(),
            "n:18".into(),
            "tok".into(),
            PendingAction::Approve,
            Deadline::none(),
            Default::default(),
        )
        .await;
    // Deadline::none() 不是"到期"，这里验证正常路径不被误伤（Approved）。
    assert!(matches!(outcome, RouterOutcome::Completed(_)));
    assert_eq!(h.dispatch.responds.lock().unwrap().len(), 1);

    // 已过期 → 零派发。
    let expired = Deadline::after_ms(0);
    tokio::time::sleep(std::time::Duration::from_millis(2)).await;
    let h2 = harness();
    let ctx2 = h2.ctx("act-t04-zero-2", ActionKind::CommandApproval);
    let outcome = h2
        .router
        .respond_bounded(
            ctx2.clone(),
            "n:18".into(),
            "tok".into(),
            PendingAction::Approve,
            expired,
            Default::default(),
        )
        .await;
    let receipt = match outcome {
        RouterOutcome::Completed(r) => r,
        _ => panic!("unexpected"),
    };
    assert_eq!(receipt.delivery, Delivery::NotSent);
    assert_eq!(
        receipt.reason_code.as_deref(),
        Some("front_budget_exhausted")
    );
    assert!(
        h2.dispatch.responds.lock().unwrap().is_empty(),
        "剩余预算为 0 时不得派发"
    );
}

/// 自然空闲接力必须把冻结正文按 FIFO 合为一次 start-turn。
#[tokio::test]
async fn idle_flush_batches_frozen_fifo_texts() {
    let h = harness();
    for (id, text) in [
        ("batch-a", "第一条"),
        ("batch-b", "第二条"),
        ("batch-c", "第三条"),
    ] {
        queued(
            h.router
                .send(h.ctx(id, ActionKind::Send), text.into(), true, false)
                .await,
        );
    }
    h.router
        .flush_thread(&h.ctx("batch-a", ActionKind::Send).key)
        .await
        .unwrap();
    let sends = h.dispatch.sends.lock().unwrap();
    assert_eq!(sends.len(), 1);
    assert_eq!(sends[0].composed, "第一条\n\n第二条\n\n第三条");
}

#[tokio::test]
async fn idle_batch_members_settle_and_replay_without_second_turn() {
    let mut h = harness();
    for id in ["batch-a", "batch-b"] {
        queued(
            h.router
                .send(h.ctx(id, ActionKind::Send), id.into(), true, false)
                .await,
        );
    }
    h.router.flush_thread(&h.key).await.unwrap();
    for id in ["batch-a", "batch-b"] {
        assert_eq!(h.state_of(id), ActionState::Accepted);
        let receipt = completed(
            h.router
                .send(h.ctx(id, ActionKind::Send), id.into(), false, false)
                .await,
        );
        assert_eq!(receipt.action_id, id);
        assert_eq!(receipt.turn_id.as_deref(), Some("turn-1"));
        assert!(!h.store.retract(id, "dev-1").unwrap());
    }
    assert_eq!(h.dispatch.send_count(), 1);
    assert_eq!(
        h.store.canonical_batch_action_id("batch-b").unwrap(),
        "batch-a"
    );
    h.restart();
    assert!(h.router.flush_thread(&h.key).await.is_none());
    assert_eq!(h.dispatch.send_count(), 0);
}

#[tokio::test]
async fn idle_batch_busy_restores_each_frozen_text_once() {
    let h = harness();
    for id in ["batch-a", "batch-b"] {
        queued(
            h.router
                .send(h.ctx(id, ActionKind::Send), id.into(), true, false)
                .await,
        );
    }
    h.dispatch.set_busy_before_write(true);
    assert!(h.router.flush_thread(&h.key).await.is_none());
    for id in ["batch-a", "batch-b"] {
        assert_eq!(h.state_of(id), ActionState::Queued);
        assert_eq!(
            h.store.queued_action_by_id(id).unwrap().unwrap().raw_text,
            id
        );
    }
    h.dispatch.set_busy_before_write(false);
    h.router.flush_thread(&h.key).await.unwrap();
    assert_eq!(
        h.dispatch.sends.lock().unwrap()[0].composed,
        "batch-a\n\nbatch-b"
    );
}

#[tokio::test]
async fn idle_batch_retracted_member_never_enters_wire() {
    let h = harness();
    for id in ["batch-a", "batch-b", "batch-c"] {
        queued(
            h.router
                .send(h.ctx(id, ActionKind::Send), id.into(), true, false)
                .await,
        );
    }
    assert!(h.store.retract("batch-b", "dev-1").unwrap());
    h.router.flush_thread(&h.key).await.unwrap();
    assert_eq!(
        h.dispatch.sends.lock().unwrap()[0].composed,
        "batch-a\n\nbatch-c"
    );
    assert_eq!(h.state_of("batch-b"), ActionState::Cancelled);
}

#[tokio::test]
async fn idle_batch_claim_failure_rolls_back_all_members() {
    let h = harness();
    for id in ["batch-a", "batch-b"] {
        queued(
            h.router
                .send(h.ctx(id, ActionKind::Send), id.into(), true, false)
                .await,
        );
    }
    h.conn.lock().unwrap().execute_batch("CREATE TRIGGER fail_batch_claim BEFORE UPDATE ON codex_actions WHEN NEW.action_id='batch-b' AND NEW.state='reserved' BEGIN SELECT RAISE(ABORT, 'test claim failure'); END;").unwrap();
    assert!(h.router.flush_thread(&h.key).await.is_none());
    assert_eq!(h.dispatch.send_count(), 0);
    assert_eq!(h.state_of("batch-a"), ActionState::Queued);
    assert_eq!(h.state_of("batch-b"), ActionState::Queued);
}

#[tokio::test]
async fn idle_batch_unknown_members_survive_restart_without_resend() {
    let mut h = harness_with(FakeDispatch::new(DispatchOutcome {
        delivery: Delivery::UnknownDelivery,
        envelope_id: Some("batch-env".into()),
        turn_id: None,
        reason_code: Some("disconnected".into()),
        revision: None,
    }));
    for id in ["batch-a", "batch-b"] {
        queued(
            h.router
                .send(h.ctx(id, ActionKind::Send), id.into(), true, false)
                .await,
        );
    }
    h.router.flush_thread(&h.key).await.unwrap();
    assert_eq!(h.state_of("batch-a"), ActionState::Unknown);
    assert_eq!(h.state_of("batch-b"), ActionState::Unknown);
    h.restart();
    assert!(h.router.flush_thread(&h.key).await.is_none());
    assert_eq!(h.dispatch.send_count(), 0);
}

#[tokio::test]
async fn idle_batch_reserved_crash_and_late_result_keep_one_canonical_barrier() {
    let mut h = harness();
    for id in ["batch-a", "batch-b"] {
        queued(
            h.router
                .send(h.ctx(id, ActionKind::Send), id.into(), true, false)
                .await,
        );
    }
    assert_eq!(
        h.store.reserve_send_batch("batch-a").unwrap().unwrap(),
        "batch-a\n\nbatch-b"
    );
    h.restart();
    for id in ["batch-a", "batch-b"] {
        assert_eq!(h.state_of(id), ActionState::Unknown);
    }
    let receipt = ActionReceipt {
        action_id: "batch-a".into(),
        delivery: Delivery::Accepted,
        route: ControlRoute::DesktopIpc,
        turn_id: Some("late-turn".into()),
        envelope_id: Some("late-env".into()),
        reason_code: None,
    };
    assert!(h.store.finish_late_result(&receipt).unwrap());
    assert!(!h.store.finish_late_result(&receipt).unwrap());
    for id in ["batch-a", "batch-b"] {
        assert_eq!(h.state_of(id), ActionState::Accepted);
    }
    let barriers: i64 = h
        .conn
        .lock()
        .unwrap()
        .query_row(
            "SELECT COUNT(*) FROM codex_actions WHERE compat_outstanding=1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(barriers, 1);
    assert!(h.router.flush_thread(&h.key).await.is_none());
    assert_eq!(h.dispatch.send_count(), 0);
}

#[tokio::test]
async fn idle_batch_busy_preserves_unsigned_literal_suffix_for_edit() {
    let h = harness();
    let text = "原文 [mobile iPhone]";
    for id in ["batch-a", "batch-b"] {
        queued(
            h.router
                .send(h.ctx(id, ActionKind::Send), text.into(), true, false)
                .await,
        );
    }
    h.dispatch.set_busy_before_write(true);
    assert!(h.router.flush_thread(&h.key).await.is_none());
    assert_eq!(
        h.store
            .retract_for_edit("batch-b", "dev-1", &h.key)
            .unwrap()
            .as_deref(),
        Some(text)
    );
}

#[tokio::test]
async fn idle_batch_ignores_residual_non_fifo_action_without_mixing_body() {
    let h = harness();
    queued(
        h.router
            .send(
                h.ctx("interaction", ActionKind::Send),
                "不能进入普通正文".into(),
                true,
                false,
            )
            .await,
    );
    h.conn
        .lock()
        .unwrap()
        .execute(
            "UPDATE codex_actions SET kind='user_input' WHERE action_id='interaction'",
            [],
        )
        .unwrap();
    for id in ["batch-a", "batch-b"] {
        queued(
            h.router
                .send(h.ctx(id, ActionKind::Send), id.into(), true, false)
                .await,
        );
    }
    h.router.flush_thread(&h.key).await.unwrap();
    assert_eq!(
        h.dispatch.sends.lock().unwrap()[0].composed,
        "batch-a\n\nbatch-b"
    );
    assert_eq!(h.state_of("interaction"), ActionState::Queued);
}

#[tokio::test]
async fn idle_batch_size_budget_keeps_suffix_queued_for_next_turn() {
    let h = harness();
    let text = "x".repeat(30 * 1024);
    for id in ["batch-a", "batch-b", "batch-c"] {
        queued(
            h.router
                .send(h.ctx(id, ActionKind::Send), text.clone(), true, false)
                .await,
        );
    }
    h.router.flush_thread(&h.key).await.unwrap();
    assert_eq!(
        h.dispatch.sends.lock().unwrap()[0].composed,
        format!("{text}\n\n{text}")
    );
    assert_eq!(h.state_of("batch-a"), ActionState::Accepted);
    assert_eq!(h.state_of("batch-b"), ActionState::Accepted);
    assert_eq!(h.state_of("batch-c"), ActionState::Queued);
    assert!(h.router.flush_thread(&h.key).await.is_none());
    h.dispatch.set_terminal_turn("turn-1");
    assert!(h
        .router
        .release_turn_if_complete(&h.key, &minimal_projection(THREAD, "idle")));
    h.router.flush_thread(&h.key).await.unwrap();
    assert_eq!(h.dispatch.send_count(), 2);
    assert_eq!(h.state_of("batch-c"), ActionState::Accepted);
}

// ============================================================
// P5（2026-10-31）：陈旧停顿记录不得把普通发送变成排队
// ============================================================

/// 用户实机反馈：线程明明空闲，所有普通消息却一律进队列。
///
/// 根因 A：迁移前的停止类暂停行 `pause_turn_id` 为空——上一轮 P3 的自动解除
/// 对这个形态明确 `continue`（无绑定回合 → 证据无从核对），于是**永不解除**。
/// 叠加历史遗留的 queued 行后，`send_inner` 的 `paused_with_older` 恒真，线程被
/// 永久判为「已暂停且有更早排队」，每条新消息都留在队列。
/// 契约：会话空闲（无在飞回合、无原生 pending）时，**无回合绑定的陈旧停止暂停行**
/// 必须自动清除并按正常路径派发；有绑定且回合未终态时才保持冻结。
#[tokio::test]
async fn stale_stop_pause_without_turn_binding_does_not_force_queueing() {
    let h = harness(); // FakeDispatch: idle、send_gate Clear
                       // 迁移前形态：停止类暂停行，无 pause_turn_id
    h.store
        .set_queue_pause(&h.key, "interrupt_pending", "legacy-token", None)
        .unwrap();
    // 历史遗留的排队行：直接写账本（不经 send——send 会先触发清理，掩盖本用例要
    // 验证的「陈旧行把新消息扣住」现场；实机就是上轮卡住留下的这条）
    h.store
        .register(
            &h.ctx("legacy-q", ActionKind::Send),
            ActionKind::Send,
            None,
            None,
            &payload_hash(
                ActionKind::Send,
                &h.key,
                None,
                None,
                &ActionPayload::Text("上次卡住的".into()),
            ),
            Some("上次卡住的"),
        )
        .expect("register legacy");
    assert!(
        h.store.queue_pause(&h.key).unwrap().is_some(),
        "前置：陈旧暂停行在册"
    );

    // 空闲线程发送普通消息 → 陈旧暂停必须被判定为可清除
    let _out = h
        .router
        .send(
            h.ctx("fresh-1", ActionKind::Send),
            "直接发".into(),
            false,
            false,
        )
        .await;
    assert!(
        h.store.queue_pause(&h.key).unwrap().is_none(),
        "无回合绑定的陈旧停止暂停行必须清除（否则空闲线程永远排队）"
    );
    // 清除后队列必须能接力（此前被暂停冻死；FIFO 先派遗留队首 legacy-q）
    let relayed = h.router.flush_thread(&h.key).await;
    assert!(
        relayed.is_some(),
        "陈旧暂停清除后队列必须开始派发（此前被它永久冻死）"
    );
    assert!(h.dispatch.send_count() > 0, "确实发生了真实派发");
}

/// 反例：停止类暂停行**有**回合绑定且该回合未终态 → 暂停保持，消息仍入队
///（「停止在途」的保护不被上面的清理误伤）。
#[tokio::test]
async fn live_stop_pause_with_turn_binding_still_freezes() {
    let h = harness();
    h.dispatch.set_busy(true);
    queued(
        h.router
            .send(
                h.ctx("older", ActionKind::Send),
                "更早的".into(),
                true,
                false,
            )
            .await,
    );
    h.store
        .set_queue_pause(&h.key, "interrupt_pending", "live-token", Some("t-running"))
        .unwrap();
    let (id, _pos) = queued(
        h.router
            .send(
                h.ctx("frozen-1", ActionKind::Send),
                "先别发".into(),
                false,
                false,
            )
            .await,
    );
    assert_eq!(id, "frozen-1");
    assert_eq!(
        h.state_of("frozen-1"),
        ActionState::Queued,
        "停止在途：入队不派发"
    );
    assert_eq!(h.dispatch.send_count(), 0, "停止在途零派发");
    assert!(
        h.store.queue_pause(&h.key).unwrap().is_some(),
        "有绑定且未终态 → 暂停保持（不被陈旧清理误伤）"
    );
    h.dispatch.set_busy(false);
}

/// 根因 B：陈旧 Unknown 屏障（无 turnId、无任何核对证据）让 `send_inner` 的
/// `has_compat_fence` 门对**每一条**新消息生效，即使线程早已空闲。用户看到的
/// 「所有普通消息都排队」正是它。契约：屏障存在但**本线程没有在飞回合**、
/// 且该屏障是「无回合绑定的 Unknown 残留」时，新普通消息不得被它扣在队列里
/// （屏障本身保留待核对，不影响自动派发新消息）。
#[tokio::test]
async fn unresolved_unknown_barrier_does_not_block_fresh_sends_on_idle_thread() {
    let h = harness();
    seed_unknown_with_text(&h, "old-unknown", "旧消息");
    assert!(
        h.store.has_compat_fence(&h.key).unwrap(),
        "前置：Unknown 屏障在册"
    );

    let out = h
        .router
        .send(
            h.ctx("fresh-2", ActionKind::Send),
            "新消息".into(),
            false,
            false,
        )
        .await;
    let receipt = completed(out);
    assert_eq!(
        receipt.delivery,
        Delivery::Accepted,
        "无在飞回合时，陈旧 Unknown 屏障不得把新消息扣在队列：{receipt:?}"
    );
    assert_eq!(h.dispatch.send_count(), 1);
    assert!(
        h.store.has_compat_fence(&h.key).unwrap(),
        "屏障本身保留（不因放行新消息而假装核对通过）"
    );
}

// ============================================================
// P6（2026-10-31 第二次实机反馈）：重启重建的在飞限制也不得堵死空闲会话
// ============================================================

/// 用户第二次实机反馈：构建已更新，普通消息**仍然**进等待队列。
///
/// 上一轮只改了 `send_inner` 的暂停门与屏障门，漏了两处同源门：
/// ① `router.new()` 从账本重建 `active_turns`——`compat_outstanding=1 ∧
///    route=desktop_ipc ∧ state IN (accepted, unknown)` 的行（本机就是 fuz273390
///    那条 unknown），即使 `compat_turn_id` 为空也照样入册；`send_inner` 与
///    `dispatch_head` 的 `is_turn_active` 门随即对每条新消息生效。
/// ② `dispatch_head` 自身还有一道 `has_compat_fence` 门（上一轮只改了 send_inner
///    的那道），队首真正派发时仍会被拦成 Busy。
///
/// 契约：**未绑定回合**的 accepted/unknown 残留不得在重启后重建为在飞限制；
/// 队首派发前的屏障门与 send 门同源判定（未被绑定的 Unknown 残留不拦派发）。
#[tokio::test]
async fn restart_rebuild_skips_unbound_turn_bindings() {
    let mut h = harness();
    // 造本机同形态：unknown 保守屏障，无 turnId（实机 fd273390）
    seed_unknown_with_text(&h, "legacy-unknown", "旧消息");
    // 重启：router.new() 从账本重建 active_turns
    h.restart();

    assert!(
        !h.router.is_turn_active_public(&h.key),
        "未绑定回合的 Unknown 残留不得在重启后重建为在飞限制（否则空闲会话永远排队）"
    );

    // 空闲会话发送 → 必须直发
    let out = h
        .router
        .send(
            h.ctx("fresh-3", ActionKind::Send),
            "重启后直发".into(),
            false,
            false,
        )
        .await;
    let receipt = completed(out);
    assert_eq!(
        receipt.delivery,
        Delivery::Accepted,
        "重启后空闲会话必须直发：{receipt:?}"
    );
    assert_eq!(h.dispatch.send_count(), 1);
}

/// 反例：**绑定回合**的 accepted 屏障必须在重启后仍重建为在飞限制（等待其 Turn
/// 终态证据释放）——上面的跳过不能把真实顺序保护一起丢掉。
#[tokio::test]
async fn restart_rebuild_keeps_bound_turn_bindings() {
    let mut h = harness_with(FakeDispatch::accepted_turn("turn-bound", None));
    let r = completed(
        h.router
            .send(
                h.ctx("bound-1", ActionKind::Send),
                "已受理".into(),
                false,
                false,
            )
            .await,
    );
    assert_eq!(r.delivery, Delivery::Accepted);
    h.restart_with(FakeDispatch::accepted_turn("turn-bound", None));
    assert!(
        h.router.is_turn_active_public(&h.key),
        "绑定回合的屏障重启后必须保留为在飞限制"
    );
    // 该保护仍生效：新消息排队，直到终态证据释放
    let (id, _) = queued(
        h.router
            .send(
                h.ctx("after-bound", ActionKind::Send),
                "受保护".into(),
                false,
                false,
            )
            .await,
    );
    assert_eq!(id, "after-bound");
    assert_eq!(h.state_of("after-bound"), ActionState::Queued);
}

/// P7（2026-10-31 第三次实机：点插队仍被拒「桌面正忙」）：用户库里只剩一条
/// `unknown` + 屏障在册 + **无 turnId** 的历史行，队列里两条 queued，桌面空闲。
/// 插队必须放行这一条（先停止/或无需停止 → 再把该条正文打出去），不得以
/// `barrier_outstanding` 拒绝——那道屏障是已收尾的未知投递，不是「进行中的派发」。
#[tokio::test]
async fn p7_prioritize_passes_through_unbound_unknown_barrier_residue() {
    use multi_agents_manager_lib::services::codex_control::{Deadline, PrioritizeOutcome};
    let h = harness();
    // 用户库同形态：一条 unknown + 屏障在册 + 无 turnId
    seed_unknown_with_text(&h, "legacy-unknown", "旧消息");
    assert!(h.store.has_compat_fence(&h.key).unwrap(), "前置：屏障在册");
    // 队列里两条（模拟用户截图里的第 1/2 位）
    h.dispatch.set_busy(true);
    queued(
        h.router
            .send(h.ctx("q1", ActionKind::Send), "你好".into(), true, false)
            .await,
    );
    h.dispatch.set_busy(false);
    // 队列已暂停（插队的前置：空闲 + 已暂停）
    h.store
        .set_queue_pause(&h.key, "interrupt_accepted", "tok", Some("t-past"))
        .unwrap();

    let lock = h
        .router
        .thread_dispatch_lock(&h.key)
        .try_lock_owned()
        .unwrap();
    let out = h
        .router
        .prioritize_queued("q1", &h.key, Deadline::none(), lock)
        .await;
    match out {
        PrioritizeOutcome::Completed(r) => {
            assert_eq!(
                r.delivery,
                Delivery::Accepted,
                "插队必须真的把这条打出去：{r:?}"
            );
        }
        PrioritizeOutcome::StillQueued(reason) => {
            panic!("插队不得被历史未知残留拒绝，收到 reason={reason}");
        }
        other => panic!("unexpected {other:?}"),
    }
    assert_eq!(
        h.dispatch.recorded_composed(),
        vec!["你好"],
        "只有目标被派出"
    );
    assert!(
        h.store.has_compat_fence(&h.key).unwrap(),
        "屏障本体保留（不假装核对通过）"
    );
}

/// P9b（2026-10-31 第五次实机）：同线程多条已受理屏障时，终态释放只清内存里
/// **最后一条**，其余永久残留并把后续插队/接力锁死。
///
/// `active_turns` 是 `HashMap<ThreadKey, ActiveTurn>`——一个线程只存一条。连续两次
/// 受理后，第一条的身份被覆盖；`release_turn_if_complete` 只能清最后一条的屏障，
/// 前面那条的持久屏障再无人清（实机账本里 accepted+turnId 的屏障越积越多）。
/// 契约：投影驱动的终态释放必须清掉**该线程所有**满足终态证据的已受理屏障，
/// 而不只是内存里的那一条。
#[tokio::test]
async fn p9b_release_clears_all_terminal_accepted_barriers_on_thread() {
    let h = harness_with(FakeDispatch::accepted_turn("turn-a", None));
    // 第一次受理 → 屏障 a
    let r1 = completed(
        h.router
            .send(
                h.ctx("act-a", ActionKind::Send),
                "第一条".into(),
                false,
                false,
            )
            .await,
    );
    assert_eq!(r1.delivery, Delivery::Accepted);
    // 第一条回合先终态 → 释放限制，第二条才能受理
    h.dispatch.set_terminal_turn("turn-a");
    assert!(h
        .router
        .release_turn_if_complete(&h.key, &projection_with_rev(THREAD, "idle", 5)));
    assert!(
        h.store.compat_fences(&h.key).unwrap().is_empty(),
        "A 释放后无残留"
    );
    // 第二次受理（不同动作，同线程）→ 屏障 b；内存限制被 b 覆盖
    h.dispatch.set_outcome(DispatchOutcome {
        delivery: Delivery::Accepted,
        envelope_id: Some("env-b".into()),
        turn_id: Some("turn-b".into()),
        reason_code: None,
        revision: None,
    });
    let r2 = completed(
        h.router
            .send(
                h.ctx("act-b", ActionKind::Send),
                "第二条".into(),
                false,
                false,
            )
            .await,
    );
    assert_eq!(r2.delivery, Delivery::Accepted);
    // 再造一条"更早的、已终态但未清"的屏障（模拟历史残留：旧实现只会清最后一条）
    {
        let hash = payload_hash(
            ActionKind::Send,
            &h.key,
            None,
            None,
            &ActionPayload::Text("残留旧条".into()),
        );
        h.store
            .register_signed(
                &h.ctx("act-old", ActionKind::Send),
                ActionKind::Send,
                None,
                None,
                &hash,
                Some("残留旧条"),
                Some(false),
            )
            .expect("register old");
        assert!(h
            .store
            .reserve("act-old", ControlRoute::DesktopIpc)
            .unwrap());
        h.store
            .finish_with_turn_barrier(
                &ActionReceipt {
                    action_id: "act-old".into(),
                    delivery: Delivery::Accepted,
                    route: ControlRoute::DesktopIpc,
                    turn_id: Some("turn-old".into()),
                    envelope_id: Some("env-old".into()),
                    reason_code: None,
                },
                Some("turn-old"),
            )
            .expect("finish old barrier");
    }
    assert_eq!(
        h.store.compat_fences(&h.key).unwrap().len(),
        2,
        "前置：两条屏障在册（b + 旧残留）"
    );

    // 两个回合都以终态在场 + 投影 idle → 释放
    h.dispatch.set_terminal_turn("turn-old");
    h.dispatch.set_terminal_turn("turn-b");
    let projection = projection_with_rev(THREAD, "idle", 9);
    h.router.release_turn_if_complete(&h.key, &projection);

    assert!(
        h.store.compat_fences(&h.key).unwrap().is_empty(),
        "所有回合都已终态的屏障必须全部释放（只剩最后一条 = 前面的永久残留并锁死后续操作）"
    );
}

/// P11（2026-10-31 用户裁决）：「哪怕它都发送出去了也无所谓，不能卡我其他流程」。
///
/// 现状：`flush_thread` 只要发现有未清屏障就整队不接力。但屏障只可能挂在**终态**
/// 动作上（unknown/accepted）——`next_queued` 只取 `state='queued'`，所以那条可疑
/// 消息**永远不会**从队列里被重发。这道门防不住它（它不在队列），却冻结了队列里
/// 所有**内容完全不同**的消息。
///
/// 契约：残留屏障不再冻结队列接力；真实安全由 `send_gate`（实时看桌面忙/待审批）
/// 与 `is_turn_active`（有界、靠终态证据释放）承担。屏障本身保留在账本里，
/// 该条仍标记为未知、不被重发。
#[tokio::test]
async fn p11_residual_barrier_does_not_freeze_queue_relay() {
    let h = harness();
    // 一条历史 unknown（终态、屏障在册、不在队列）
    seed_unknown_with_text(&h, "legacy-unknown", "老消息");
    assert!(h.store.has_compat_fence(&h.key).unwrap(), "前置：屏障在册");
    // 队列里另有一条完全不同的消息
    h.dispatch.set_busy(true);
    queued(
        h.router
            .send(
                h.ctx("q-fresh", ActionKind::Send),
                "新消息".into(),
                true,
                false,
            )
            .await,
    );
    h.dispatch.set_busy(false);

    let out = h.router.flush_thread(&h.key).await;
    assert!(
        out.is_some(),
        "历史 unknown 不得冻结队列接力（它自己不在队列、永不会被重发）"
    );
    assert_eq!(
        h.dispatch.recorded_composed(),
        vec!["新消息"],
        "接力的必须是队列里那条新消息"
    );
    assert_eq!(
        h.state_of("legacy-unknown"),
        ActionState::Unknown,
        "那条老记录状态不变、不被重发"
    );
}
