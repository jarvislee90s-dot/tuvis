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
    StoreError,
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

impl DesktopDispatch for FakeDispatch {
    fn thread_busy(&self, _key: &ThreadKey) -> bool {
        self.busy.load(Ordering::SeqCst)
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
        Box::pin(async move {
            if let Some(g) = gate {
                g.notified().await;
            }
            out
        })
    }

    fn respond(
        &self,
        key: &ThreadKey,
        request_key: &str,
        context_token: &str,
        _action: &PendingAction,
    ) -> multi_agents_manager_lib::services::codex_control::BoxDispatchFut {
        self.responds.lock().unwrap().push((
            key.clone(),
            request_key.to_string(),
            context_token.to_string(),
        ));
        let out = self.respond_outcome.lock().unwrap().clone();
        Box::pin(async move { out })
    }
}

struct Harness {
    conn: SharedConn,
    store: Arc<ActionStore>,
    dispatch: Arc<FakeDispatch>,
    router: CodexControlRouter,
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
    let router = CodexControlRouter::new(store.clone(), dispatch.clone());
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

fn not_sent_receipt(action_id: &str, route: ControlRoute, reason: &str) -> ActionReceipt {
    ActionReceipt {
        action_id: action_id.to_string(),
        delivery: Delivery::NotSent,
        route,
        turn_id: None,
        envelope_id: None,
        reason_code: Some(reason.to_string()),
    }
}

fn accepted_compat_receipt(action_id: &str) -> ActionReceipt {
    ActionReceipt {
        action_id: action_id.to_string(),
        delivery: Delivery::Accepted,
        route: ControlRoute::HeadlessCompat,
        turn_id: Some("compat-turn".into()),
        envelope_id: None,
        reason_code: None,
    }
}

impl Harness {
    /// 模拟进程重启：同一 DB 连接上重建 Store/Router（recover_reserved 在 Store 构造时执行）。
    fn restart(&mut self) {
        self.store = Arc::new(ActionStore::new(DeviceStore::Owned(self.conn.clone())));
        self.dispatch = Arc::new(FakeDispatch::accepted());
        self.router = CodexControlRouter::new(self.store.clone(), self.dispatch.clone());
    }

    /// 同上，但替换派发器（重启后 A 路行为变化）。
    fn restart_with(&mut self, dispatch: FakeDispatch) {
        self.store = Arc::new(ActionStore::new(DeviceStore::Owned(self.conn.clone())));
        self.dispatch = Arc::new(dispatch);
        self.router = CodexControlRouter::new(self.store.clone(), self.dispatch.clone());
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
        RouterOutcome::UseFallback { .. } => panic!("unexpected UseFallback"),
        RouterOutcome::Queued { .. } => panic!("unexpected Queued"),
    }
}

fn use_fallback(
    view: RouterOutcome,
) -> multi_agents_manager_lib::services::codex_control::FallbackPermit {
    match view {
        RouterOutcome::UseFallback { permit, .. } => permit,
        other => panic!("expected UseFallback, got {other:?}"),
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
        },
        runtime_status: status.to_string(),
        content_state: ContentState::Ready,
        messages: vec![ProjectedMessage {
            id: "m1".into(),
            role: "user".into(),
            kind: "message".into(),
            text: "seed".into(),
            ts: None,
        }],
        truncated: false,
        pending: Vec::new(),
        probe_status: ProbeStatus::idle(),
        owner_epoch: 0,
        owner_generation: 0,
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

    let r1 = completed(h.router.send(ctx.clone(), "你好".into(), false).await);
    assert_eq!(r1.delivery, Delivery::Accepted);
    assert_eq!(h.dispatch.send_count(), 1, "首次派发恰好一次");

    // 同 action_id + 同载荷重放：返回原状态，不再写
    let r2 = completed(h.router.send(ctx.clone(), "你好".into(), false).await);
    assert_eq!(r2.delivery, Delivery::Accepted);
    assert_eq!(r2, r1, "幂等重放返回同一回执");
    assert_eq!(h.dispatch.send_count(), 1, "重放零新增写入");

    // queue_only 重复：仍 Queued 原位，不派发
    let ctx2 = h.ctx("act-2", ActionKind::Send);
    let (id, pos) = queued(h.router.send(ctx2.clone(), "第二条".into(), true).await);
    assert_eq!(id, "act-2");
    assert_eq!(pos, 1);
    let (id2, pos2) = queued(h.router.send(ctx2, "第二条".into(), true).await);
    assert_eq!(id2, "act-2");
    assert_eq!(pos2, 1, "重复入队不产生第二个队列条目");
    assert_eq!(h.dispatch.send_count(), 1);
}

/// 相同 action_id 换正文/设备/thread 返回 conflict 且零新增写入。
#[tokio::test]
async fn conflicting_action_id_reuse_is_conflict_and_zero_writes() {
    let h = harness();
    let ctx = h.ctx("act-c", ActionKind::Send);
    let r = completed(h.router.send(ctx.clone(), "原文".into(), false).await);
    assert_eq!(r.delivery, Delivery::Accepted);

    // 换正文
    let rc = completed(
        h.router
            .send(ctx.clone(), "换成别的正文".into(), false)
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
    let rt = completed(h.router.send(ctx_other_thread, "原文".into(), false).await);
    assert_eq!(rt.reason_code.as_deref(), Some("action_conflict"));
    assert_eq!(h.dispatch.send_count(), 1);

    // 换设备（同 action_id）
    let mut ctx_other_dev = h.ctx("act-c", ActionKind::Send);
    ctx_other_dev.device_id = "dev-2".into();
    ctx_other_dev.device_name = "iPad".into();
    let rd = completed(h.router.send(ctx_other_dev, "原文".into(), false).await);
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
    let (id, _) = queued(h.router.send(a, "第一条".into(), true).await);
    assert_eq!(id, "act-a");
    assert!(h.store.reserve("act-a", ControlRoute::DesktopIpc).unwrap());
    assert_eq!(h.state_of("act-a"), ActionState::Reserved);

    // 同线程还有一条 queued
    let b = h.ctx("act-b", ActionKind::Send);
    assert!(matches!(
        h.router.send(b, "第二条".into(), true).await,
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
    assert!(
        h.router.flush_thread(&h.key).await.is_none(),
        "保守屏障闸住接力（队列视图 blockedReason 如实说明）"
    );
    assert_eq!(h.dispatch.send_count(), 0, "屏障在册零派发");

    // 多轮对账也不解锁（C1.2：Unknown 不是完成，对账不是定时器条件）
    for _ in 0..3 {
        h.router.converge_with_ledger();
        assert!(h.router.flush_thread(&h.key).await.is_none());
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

/// active 时连续 a/b 两条消息落 SQLite FIFO，idle 后只发 a，等它终态再发 b；
/// 期间审批能取得短 action 锁响应原 Turn（不等待 Turn 结束）。
#[tokio::test]
async fn busy_fifo_allows_pending_response() {
    let h = harness();
    h.dispatch.set_busy(true);

    let a = h.ctx("fifo-a", ActionKind::Send);
    let b = h.ctx("fifo-b", ActionKind::Send);
    let (ida, posa) = queued(h.router.send(a, "消息甲".into(), false).await);
    let (idb, posb) = queued(h.router.send(b, "消息乙".into(), false).await);
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

    // idle 后只发 a
    h.dispatch.set_busy(false);
    let out_a = h.router.flush_thread(&h.key).await.expect("a dispatchable");
    assert_eq!(completed(out_a).delivery, Delivery::Accepted);
    {
        let sends = h.dispatch.sends.lock().unwrap();
        assert_eq!(sends.len(), 1);
        assert_eq!(sends[0].action_id, "fifo-a");
    }
    // b 仍在队列（turn a 未终态）——R6.3：顺序限制在册，flush 不再无条件接力
    assert_eq!(h.state_of("fifo-b"), ActionState::Queued);
    assert!(
        h.router.flush_thread(&h.key).await.is_none(),
        "Turn 顺序限制未释放前不接力"
    );

    // turn a 终态（fresh idle 投影事件 + 绑定 turn 终态证据）→ 释放顺序限制 → flush
    // 接力发 b（R6.3：释放由完成证据驱动，不再靠手工 flush 清位；C1：证据 = 绑定
    // turnId 本身以终态在场）
    h.dispatch.set_terminal_turn("turn-1");
    assert!(
        h.router
            .release_turn_if_complete(&h.key, &minimal_projection(THREAD, "idle")),
        "fresh idle 投影 + 绑定 turn 终态证据应释放顺序限制"
    );
    let out_b = h.router.flush_thread(&h.key).await.expect("b dispatchable");
    assert_eq!(completed(out_b).delivery, Delivery::Accepted);
    let sends = h.dispatch.sends.lock().unwrap();
    assert_eq!(sends.len(), 2);
    assert_eq!(sends[1].action_id, "fifo-b", "FIFO 接力按序");
}

// ============================================================
// 计划场景 4：fallback 只在派发前
// ============================================================

/// IPC NotSent 且仍未尝试写可 UseFallback；Accepted/Rejected/UnknownDelivery 不得触发
/// fallback；审批永不降级（NotSent 如实回执）。
#[tokio::test]
async fn fallback_only_before_dispatch() {
    // NotSent → UseFallback
    let h = harness_with(FakeDispatch::not_sent("ipc_disconnected"));
    let permit = use_fallback(
        h.router
            .send(h.ctx("fb-1", ActionKind::Send), "降级消息".into(), false)
            .await,
    );
    assert_eq!(permit.action_id(), "fb-1");
    assert_eq!(permit.wire_text(), "降级消息", "permit 携带原文");
    assert_eq!(permit.context().device_name, "iPhone");
    permit
        .finish(accepted_compat_receipt("fb-1"))
        .expect("finish");
    assert_eq!(h.state_of("fb-1"), ActionState::Accepted);

    // Accepted/Rejected/Unknown 不触发 fallback
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
        let out = h.router.send(ctx, "普通消息".into(), false).await;
        assert!(
            matches!(out, RouterOutcome::Completed(_)),
            "{name} 不得触发 fallback"
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

/// version/profile 不匹配（capability 关闭）允许普通消息兼容——同样走
/// NotSent → UseFallback，但 reason 是能力码，不能解释成「旧 RPC 未投递」。
#[tokio::test]
async fn profile_mismatch_send_falls_back_with_capability_reason() {
    let h = harness_with(FakeDispatch::not_sent("capability_unavailable"));
    let ctx = h.ctx("fb-prof", ActionKind::Send);
    match h.router.send(ctx, "兼容消息".into(), false).await {
        RouterOutcome::UseFallback {
            permit,
            reason_code,
        } => {
            assert_eq!(reason_code, "capability_unavailable");
            assert_eq!(permit.wire_text(), "兼容消息");
            permit
                .finish(not_sent_receipt(
                    "fb-prof",
                    ControlRoute::HeadlessCompat,
                    "helper_refused",
                ))
                .expect("finish");
            assert_eq!(h.state_of("fb-prof"), ActionState::Rejected);
        }
        other => panic!("expected UseFallback, got {other:?}"),
    }
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
            .send(h.ctx("seq-a", ActionKind::Send), "甲".into(), true)
            .await,
    );
    let (idb, pb) = queued(
        h.router
            .send(h.ctx("seq-b", ActionKind::Send), "乙".into(), true)
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
        .send(h.ctx("race-a", ActionKind::Send), "竞态甲".into(), true)
        .await;
    assert!(h.store.reserve("race-a", ControlRoute::DesktopIpc).unwrap());
    assert!(
        !h.store.retract("race-a", "dev-1").unwrap(),
        "reserved 不可撤"
    );
    assert_eq!(h.state_of("race-a"), ActionState::Reserved);

    // 顺序二：retract 先赢 → reserve 失败；保留墓碑、清正文
    h.router
        .send(h.ctx("race-b", ActionKind::Send), "竞态乙".into(), true)
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
            .send(h.ctx("race-b", ActionKind::Send), "竞态乙".into(), false)
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
        .send(h.ctx("rv-a", ActionKind::Send), "吊销前排队".into(), true)
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
                .send(h.ctx("sig-off", ActionKind::Send), raw.into(), false)
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
                .send(h.ctx("sig-on", ActionKind::Send), raw.into(), false)
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
            .send(h.ctx("sig-queued", ActionKind::Send), raw.into(), true)
            .await;
        set_signature(&h.conn, false);
        // 同 actionId 重试（设置已变）：仍返回既有 queued、零新写入/零重复派发
        let retry = h
            .router
            .send(h.ctx("sig-queued", ActionKind::Send), raw.into(), true)
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

    // fallback：permit 携带冻结 wire；wire hash 与 fence 证据同源
    {
        let h = harness_with(FakeDispatch::not_sent("ipc_disconnected"));
        set_signature(&h.conn, true);
        let permit = use_fallback(
            h.router
                .send(h.ctx("sig-fb", ActionKind::Send), raw.into(), false)
                .await,
        );
        let wire = permit.wire_text().to_string();
        assert_eq!(wire, format!("{raw} [mobile iPhone]"));
        h.store
            .set_compat_fence("sig-fb", Some("compat-msg"), Some("compat-turn"))
            .expect("set fence");
        h.store
            .set_compat_evidence(
                "sig-fb",
                Some("/tmp/rollout.jsonl"),
                Some(64),
                Some(&wire_text_hash(&wire)),
            )
            .expect("set evidence");
        let fences = h.store.compat_fences(&h.key).expect("fences");
        assert_eq!(
            fences[0].wire_hash.as_deref(),
            Some(wire_text_hash(&wire)).as_deref(),
            "fence 证据 hash 与 permit wire 同一份"
        );
        permit
            .finish(accepted_compat_receipt("sig-fb"))
            .expect("finish");
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
        .send(h.ctx("busy-1", ActionKind::Send), raw.into(), false)
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
            .send(h.ctx("stale-1", ActionKind::Send), raw.into(), false)
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
        .send(h.ctx("stale-2", ActionKind::Send), "下一条".into(), false)
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
        .send(h.ctx("aud-1", ActionKind::Send), "审计回滚".into(), true)
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
        .send(h.ctx("aud-2", ActionKind::Send), "活动发送".into(), false)
        .await;
    assert!(
        matches!(q, RouterOutcome::Queued { .. }),
        "队首卡住时后来者只排队"
    );
    assert_eq!(h.dispatch.send_count(), 0);
    assert_eq!(h.state_of("aud-2"), ActionState::Queued);
}

// ============================================================
// 计划场景 10：fallback 恰好一次交接 + 交接时崩溃恢复 unknown
// ============================================================

/// IPC 已 reserved、writer 前 NotSent → handoff → fallback 恰好一次；交接时崩溃
/// （进程消失，permit 无 Drop）恢复为 unknown，绝不回 queued，不自动重试。
#[tokio::test]
async fn fallback_handoff_exactly_once_then_crash_is_unknown() {
    let mut h = harness_with(FakeDispatch::not_sent("ipc_disconnected"));
    let permit = use_fallback(
        h.router
            .send(h.ctx("ho-1", ActionKind::Send), "交接消息".into(), false)
            .await,
    );
    assert_eq!(h.view_of("ho-1").state, ActionState::Reserved);
    assert_eq!(h.route_of("ho-1"), "headless_compat", "handoff 已切路由");

    // handoff CAS 只能成功一次（router 已 handoff，再来一次必 false）
    assert!(
        !h.store.handoff_not_sent("ho-1").expect("handoff"),
        "恰好一次交接"
    );

    // —— 交接时崩溃：进程消失，permit 不走 Drop（forget 模拟）——
    std::mem::forget(permit);
    h.restart();
    assert_eq!(
        h.state_of("ho-1"),
        ActionState::Unknown,
        "交接后崩溃恢复为 unknown"
    );
    assert_eq!(h.route_of("ho-1"), "headless_compat", "路由保留供核对");
    // 未知动作不自动重试：flush 无产出、零写入
    assert!(h.router.flush_thread(&h.key).await.is_none());
    assert_eq!(h.dispatch.send_count(), 0, "unknown 不自动重试");
}

/// 未 finish 的 permit 被 drop → reserved 记 unknown（Drop 守卫），不自动回 queued。
#[tokio::test]
async fn dropped_permit_marks_unknown_never_requeued() {
    let h = harness_with(FakeDispatch::not_sent("ipc_disconnected"));
    let permit = use_fallback(
        h.router
            .send(h.ctx("dp-1", ActionKind::Send), "将被丢弃".into(), false)
            .await,
    );
    assert_eq!(h.view_of("dp-1").state, ActionState::Reserved);
    drop(permit);
    let v = h.view_of("dp-1");
    assert_eq!(
        v.state,
        ActionState::Unknown,
        "丢弃未完成 permit 记 unknown"
    );
    let receipt = v.receipt.expect("unknown receipt");
    assert_eq!(receipt.delivery, Delivery::UnknownDelivery);
    // 原始派发尝试（NotSent 那次）计数 1；之后任何 flush/查询不得再派发
    assert!(
        h.store.next_queued(&h.key).expect("query").is_none(),
        "unknown 不回 queued"
    );
    assert!(h.router.flush_thread(&h.key).await.is_none());
    assert_eq!(h.dispatch.send_count(), 1, "无新增派发");
}

// ============================================================
// 计划场景 11：NotSent 清 fence 与队列解锁
// ============================================================

/// 明确零投递（CLI 缺失 / 探针拒绝等 spawn/write 前失败）→ finish 与清 fence 同事务；
/// A 恢复后下一条正常发送；fence 在册时队列被闸住。
#[tokio::test]
async fn finish_not_sent_clears_fence_and_unblocks_next() {
    let mut h = harness_with(FakeDispatch::not_sent("ipc_disconnected"));
    // 第一条走 fallback，bridge 设 fence 后 helper 明确拒发 → NotSent finish
    let permit = use_fallback(
        h.router
            .send(h.ctx("fence-1", ActionKind::Send), "第一条".into(), false)
            .await,
    );
    h.store
        .set_compat_fence("fence-1", Some("compat-msg-1"), Some("compat-turn-1"))
        .expect("set fence");
    assert!(h.store.has_compat_fence(&h.key).expect("has fence"));

    // fence 在册时后续消息仍可排队，但不接力
    let qb = h
        .router
        .send(h.ctx("fence-2", ActionKind::Send), "第二条".into(), true)
        .await;
    assert!(matches!(qb, RouterOutcome::Queued { .. }));
    assert!(
        h.router.flush_thread(&h.key).await.is_none(),
        "fence 在册不接力"
    );

    // helper 明确拒发 → NotSent → fence 同事务清除
    permit
        .finish(not_sent_receipt(
            "fence-1",
            ControlRoute::HeadlessCompat,
            "cli_missing",
        ))
        .expect("finish");
    assert!(
        !h.store.has_compat_fence(&h.key).expect("fence cleared"),
        "NotSent 同事务清 fence"
    );

    // A 恢复（可用）后下一条正常发送
    h.restart_with(FakeDispatch::accepted());
    let out = h
        .router
        .flush_thread(&h.key)
        .await
        .expect("next dispatchable");
    assert_eq!(completed(out).delivery, Delivery::Accepted);
    let sends = h.dispatch.sends.lock().unwrap();
    assert_eq!(sends.len(), 1);
    assert_eq!(sends[0].action_id, "fence-2", "下一条正常发送");
}

/// crash 残留 fence（动作 reserved）→ 重启恢复 unknown；reserved 态 clear 被 Conflict
/// 拒绝；终态后允许 clear；清 fence 前队列保持闸住。
#[tokio::test]
async fn compat_fence_blocks_and_clear_requires_terminal_state() {
    let mut h = harness();
    h.router
        .send(h.ctx("cf-1", ActionKind::Send), "被闸消息".into(), true)
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
            .send(ctx.clone(), "未知结果消息".into(), false)
            .await,
    );
    assert_eq!(r1.delivery, Delivery::UnknownDelivery);
    assert_eq!(h.dispatch.send_count(), 1);

    // 重放：原状态、零写入
    let r2 = completed(h.router.send(ctx, "未知结果消息".into(), false).await);
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
    let _ = completed(h.router.send(a, "第一条".into(), false).await);
    assert_eq!(h.dispatch.send_count(), 1);

    // 绑定在册：flush 不接力；持久屏障在册（队列视图 blockedReason 同源）
    h.router
        .send(h.ctx("trn-b", ActionKind::Send), "第二条".into(), true)
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
            )
            .await,
    );
    h.router
        .send(
            h.ctx("trn-r-b", ActionKind::Send),
            "重启后排队".into(),
            true,
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
            )
            .await,
    );
    let v = h.view_of("trn-nt");
    assert_eq!(v.state, ActionState::Accepted);
    assert!(v.receipt.unwrap().turn_id.is_none(), "缺 turnId 如实回执");
    h.router
        .send(h.ctx("trn-nt-2", ActionKind::Send), "后续消息".into(), true)
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
    let _ = completed(h.router.send(a, "第一条".into(), false).await);
    assert_eq!(h.dispatch.send_count(), 1);
    // 完成快照先于 ACK 落镜像（证据已成立），ACK 采集的基线 == 同一 revision
    h.dispatch.set_terminal_turn("turn-9");
    h.router
        .send(h.ctx("b1-b", ActionKind::Send), "第二条".into(), true)
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
    let _ = completed(h.router.send(a, "换代前已发".into(), false).await);
    h.router
        .send(
            h.ctx("b1-gen-2", ActionKind::Send),
            "换代后排队".into(),
            true,
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
            .send(h.ctx("unk-a", ActionKind::Send), "未知投递".into(), false)
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
        .send(h.ctx("unk-b", ActionKind::Send), "后续消息".into(), false)
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
            .send(h.ctx("bar-a", ActionKind::Send), "缺ID已发".into(), false)
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
    let r2 = completed(h.router.send(ctx2, "未知投递".into(), false).await);
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
        .send(h.ctx("pf-a", ActionKind::Send), "落账失败消息".into(), true)
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
        .send(h.ctx("pf-b", ActionKind::Send), "后续消息".into(), true)
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
            .send(h.ctx("fc-a", ActionKind::Send), "未知投递".into(), false)
            .await,
    );
    assert_eq!(h.state_of("fc-a"), ActionState::Unknown);
    assert!(
        h.store.has_compat_fence(&h.key).expect("query"),
        "保守屏障在册"
    );
    h.router
        .send(h.ctx("fc-b", ActionKind::Send), "被困后续".into(), true)
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
        .send(h.ctx("fc-q", ActionKind::Send), "仍在排队".into(), true)
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
        .send(h.ctx("fc-r", ActionKind::Send), "派发在飞".into(), true)
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
            .send(h.ctx("fcx-a", ActionKind::Send), "未知投递".into(), false)
            .await,
    );
    h.router
        .send(h.ctx("fcx-b", ActionKind::Send), "被困后续".into(), true)
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
        tokio::spawn(async move { router.send(ctx1, "first".into(), false).await })
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
        router.send(ctx2, "second".into(), false),
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

/// 事项①（兜底形态）：兜底 permit 在外（guard 随 permit 移入 detached bridge，
/// 分钟级）时，同 thread 新 send 同样立即返回 Queued；兜底落账后 flush_thread 接力。
#[tokio::test]
async fn send_returns_queued_while_fallback_permit_outstanding() {
    let h = harness_with(FakeDispatch::not_sent("ipc_disconnected"));
    // f1 走兜底：permit 在外（guard 被其持有）
    let permit = use_fallback(
        h.router
            .send(h.ctx("f1", ActionKind::Send), "first".into(), false)
            .await,
    );
    // f2：新 send 立即排队（改前挂在 lock_owned().await 直到 permit 落账）
    let started = std::time::Instant::now();
    let outcome = h
        .router
        .send(h.ctx("f2", ActionKind::Send), "second".into(), false)
        .await;
    assert!(
        started.elapsed() < std::time::Duration::from_secs(2),
        "f2 send 不得被在飞兜底阻塞"
    );
    let (aid, pos) = queued(outcome);
    assert_eq!(aid, "f2");
    assert_eq!(pos, 1, "f1 已 handoff 出队，f2 是唯一 queued 行");
    assert_eq!(h.state_of("f2"), ActionState::Queued, "f2 行在账排队");

    // 兜底落账（guard 释放）→ flush_thread 接力 f2
    permit
        .finish(accepted_compat_receipt("f1"))
        .expect("兜底落账");
    assert_eq!(h.state_of("f1"), ActionState::Accepted);
    let out = h.router.flush_thread(&h.key).await.expect("f2 被接力");
    // f2 走到兜底交接（A 路仍 not_sent）= 已被取走派发，不再留队
    let _permit2 = use_fallback(out);
    assert_eq!(h.state_of("f2"), ActionState::Reserved);
    assert_eq!(h.route_of("f2"), "headless_compat");
    assert!(h.store.next_queued(&h.key).unwrap().is_none(), "队列已空");
}

/// 事项②（复审 Important）：超期终态行被启动墓碑化——重载荷置 NULL 但**行保留**；
/// actionId 重放（同载荷）经 payload_hash 幂等比对命中既有终态：如实返回 accepted
///（receipt_missing 合成口径），绝不回 queued、绝不重派发。
#[tokio::test]
async fn tombstoned_terminal_replay_stays_terminal() {
    let h = harness();
    // 真实生命周期造 accepted 行（finish_with_turn_barrier 在册屏障 → 终态证据释放清屏障）
    let outcome = h
        .router
        .send(h.ctx("t-old", ActionKind::Send), "老消息".into(), false)
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
        .send(h.ctx("t-old", ActionKind::Send), "老消息".into(), false)
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
