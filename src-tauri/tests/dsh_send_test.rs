//! T10 验收测试（三项独立验收，不把 T09 的 Reserved/Queued 用例当队列推进）：
//! A. 两条普通消息在忙态**按序自动推进**，无需客户端重试；
//! B. 原生发送前复核 Host/epoch、设备及会话归属；
//! C. accepted 与最终效果分开记录；断线后的未知投递不自动重发。

use std::sync::{Arc, Mutex};

use multi_agents_manager_lib::services::dsh_control::actions::{
    HostProbe, NativeDispatch, NativeSendOutcome,
};
use multi_agents_manager_lib::services::dsh_control::model::{
    ActionPayload, ActionRequest, ActionState, DeviceContext, EffectEvidence, EffectKind,
    LocalHost, Operation, SendPayload, Target,
};
use multi_agents_manager_lib::services::dsh_control::store::DshActionStore;
use multi_agents_manager_lib::services::dsh_control::{DshControlHub, DshDb};
use rusqlite::Connection;

// ============================================================
// 注入假体
// ============================================================

/// 可编程原生发送器：按序吐出预置结局，记录每次调用的 (action_id, session)。
struct FakeNative {
    outcomes: Mutex<Vec<NativeSendOutcome>>,
    calls: Mutex<Vec<(String, String)>>,
}

impl FakeNative {
    fn new(outcomes: Vec<NativeSendOutcome>) -> Arc<Self> {
        Arc::new(Self {
            outcomes: Mutex::new(outcomes),
            calls: Mutex::new(Vec::new()),
        })
    }

    fn calls(&self) -> Vec<(String, String)> {
        self.calls.lock().unwrap().clone()
    }
}

impl NativeDispatch for FakeNative {
    fn send_message<'a>(
        &'a self,
        target: &'a Target,
        _payload: &'a SendPayload,
        action_id: &'a str,
    ) -> futures::future::BoxFuture<
        'a,
        Result<NativeSendOutcome, multi_agents_manager_lib::services::dsh_control::error::DshError>,
    > {
        Box::pin(async move {
            self.calls
                .lock()
                .unwrap()
                .push((action_id.to_string(), target.session_id.clone()));
            // 预置列表按“先发先用”填：从尾部弹即头部先出。
            self.outcomes.lock().unwrap().pop().ok_or_else(|| {
                multi_agents_manager_lib::services::dsh_control::error::DshError::new(
                    "DSH_STATE_IO",
                    "fake exhausted",
                )
            })
        })
    }
}

/// 可变 Host 探针（测 freshness 时改 epoch）。
fn probe(epoch: u64) -> (Arc<Mutex<u64>>, HostProbe) {
    let cell = Arc::new(Mutex::new(epoch));
    let probe_cell = Arc::clone(&cell);
    let probe: HostProbe = Arc::new(move || {
        let ep = *probe_cell.lock().unwrap();
        Ok(LocalHost {
            host_id: "local".into(),
            platform: "macos".into(),
            profile_dir: Default::default(),
            executable: Default::default(),
            pid: 1,
            started_at_ms: 1,
            epoch: ep,
            authority: "127.0.0.1:1".into(),
            package_sha256: String::new(),
        })
    });
    (cell, probe)
}

fn hub_with(
    outcomes: Vec<NativeSendOutcome>,
    epoch: u64,
) -> (
    Arc<DshControlHub>,
    Arc<FakeNative>,
    Arc<Mutex<u64>>,
    tempfile::TempDir,
) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let db_path = tmp.path().join("tuvis.db");
    {
        let conn = Connection::open(&db_path).expect("建库");
        multi_agents_manager_lib::database::schema::init(&conn).unwrap();
    }
    let shared = Arc::new(Mutex::new(Connection::open(&db_path).expect("hub 连接")));
    let fake = FakeNative::new(outcomes);
    let (_cell, host_probe) = probe(epoch);
    let hub = DshControlHub::with_native(
        DshDb::Owned(Arc::clone(&shared)),
        fake.clone(),
        host_probe,
        None,
    );
    hub.open_write_gate();
    (hub, fake, _cell, tmp)
}

fn send_req(action_id: &str, session: &str, text: &str) -> ActionRequest {
    ActionRequest {
        target: Target {
            host_id: "local".into(),
            epoch: 1,
            session_id: session.into(),
        },
        action_id: action_id.into(),
        expected_revision: None,
        payload: ActionPayload::Send(SendPayload {
            text: text.into(),
            receipt_ids: Vec::new(),
            queue_only: true,
            bypass_paused_queue: false,
        }),
    }
}

fn dev() -> DeviceContext {
    DeviceContext {
        device_id: "dev-1".into(),
        device_name: "Dev".into(),
    }
}

fn evidence(native: &str) -> EffectEvidence {
    EffectEvidence {
        operation: Operation::Send,
        native_id: Some(native.into()),
        epoch: 1,
        effect_kind: EffectKind::Message,
    }
}

// ============================================================
// A：队列自动推进（忙态按序，零客户端重试）
// ============================================================

/// A1：m1 受理（在途）→ m2 忙态排队（无第二次原生调用）→ m1 效果落地 →
/// m2 **自动** reserve+发送+受理——客户端从未重发 m2。
#[tokio::test]
async fn second_message_auto_advances_after_first_effect() {
    // 预置按发送顺序：m1 先（列表头），实现从尾弹——倒序填。
    let (hub, fake, _p, _tmp) = hub_with(
        vec![
            NativeSendOutcome::Accepted {
                native_id: Some("n2".into()),
            }, // 第二次弹出
            NativeSendOutcome::Accepted {
                native_id: Some("n1".into()),
            }, // 先弹出
        ],
        1,
    );
    let s = "s-a1";
    let m1 = hub
        .dispatch(dev(), send_req("m1", s, "一"))
        .await
        .expect("m1");
    assert_eq!(m1.state, ActionState::Accepted, "m1 原生受理（≠成功）");

    // m2 在 m1 在途时到达：排队，原生侧零调用。
    let m2 = hub
        .dispatch(dev(), send_req("m2", s, "二"))
        .await
        .expect("m2");
    assert_eq!(m2.state, ActionState::Queued, "忙态排队是正确暂态");
    assert_eq!(fake.calls().len(), 1, "排队阶段不得原生发送");

    // m1 效果落地（受理→成功的唯一通路）。
    let view = hub
        .record_effect(&dev(), "m1", &evidence("n1"))
        .await
        .expect("效果");
    assert_eq!(view.receipt.state, ActionState::Succeeded);

    // 泵应已自动推进 m2：受理，原生调用序 = [m1, m2]。
    let m2_view = hub.action(&dev(), "m2").await.expect("读 m2");
    assert_eq!(
        m2_view.receipt.state,
        ActionState::Accepted,
        "m2 必须自动推进到受理——不需要客户端重试"
    );
    let calls = fake.calls();
    assert_eq!(calls.len(), 2, "m2 恰好一次原生发送");
    assert_eq!(calls[0], ("m1".into(), s.into()), "顺序：m1 先");
    assert_eq!(calls[1], ("m2".into(), s.into()), "顺序：m2 后（FIFO）");
}

/// A2：重启后排队消息自动推进；崩溃在途（accepted）**不**重发。
#[tokio::test]
async fn restart_advances_queued_but_never_resends_in_flight() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let db_path = tmp.path().join("tuvis.db");
    {
        let conn = Connection::open(&db_path).expect("建库");
        multi_agents_manager_lib::database::schema::init(&conn).unwrap();
    }
    let shared = Arc::new(Mutex::new(Connection::open(&db_path).expect("连接")));
    let fake = FakeNative::new(vec![
        NativeSendOutcome::Accepted {
            native_id: Some("r2".into()),
        }, // 重启后 m2
        NativeSendOutcome::Accepted {
            native_id: Some("r1".into()),
        }, // 崩溃前 m1
    ]);
    let (_cell, host_probe) = probe(1);
    let hub = DshControlHub::with_native(
        DshDb::Owned(Arc::clone(&shared)),
        fake.clone(),
        host_probe,
        None,
    );
    hub.open_write_gate();
    let s = "s-a2";
    let m1 = hub
        .dispatch(dev(), send_req("m1", s, "一"))
        .await
        .expect("m1");
    assert_eq!(m1.state, ActionState::Accepted);
    let m2 = hub
        .dispatch(dev(), send_req("m2", s, "二"))
        .await
        .expect("m2");
    assert_eq!(m2.state, ActionState::Queued);
    assert_eq!(fake.calls().len(), 1);

    // 「重启」：同一库重建 hub（仅账本视角——m1 accepted 在途、m2 queued），
    // 先启动恢复（把 m1 收口为 unknown，**不重发**），再全量推进队列（m2 从未
    // 发送过，应当自动送出）。
    let fake2 = FakeNative::new(vec![NativeSendOutcome::Accepted {
        native_id: Some("r2".into()),
    }]);
    let (_c2, probe2) = probe(1);
    let hub2 = DshControlHub::with_native(
        DshDb::Owned(Arc::clone(&shared)),
        fake2.clone(),
        probe2,
        None,
    );
    let recovered = hub2.startup_recovery().expect("恢复");
    assert_eq!(recovered, 1, "崩溃在途 m1 被收口");
    let m1_view = hub2.action(&dev(), "m1").await.expect("读 m1");
    assert_eq!(m1_view.receipt.state, ActionState::UnknownDelivery);

    hub2.advance_all_queues().await;
    let m2_view = hub2.action(&dev(), "m2").await.expect("读 m2");
    assert_eq!(
        m2_view.receipt.state,
        ActionState::Accepted,
        "从未发送过的排队消息在重启后自动推进"
    );
    // m1 不重发：新发送器只见过 m2。
    assert_eq!(fake2.calls(), vec![("m2".into(), s.into())]);
}

// ============================================================
// B：原生发送前复核
// ============================================================

/// B1：目标代际陈旧（探针 epoch=2 vs 目标 epoch=1）→ 失败落账、零原生调用、
/// 队列继续推进（面对同一陈旧探针同样失败——不卡死队列）。
#[tokio::test]
async fn stale_target_fails_before_native_without_sending() {
    let (hub, fake, cell, _tmp) = hub_with(
        vec![NativeSendOutcome::Accepted {
            native_id: Some("n".into()),
        }],
        1,
    );
    *cell.lock().unwrap() = 2; // 宿主已换代

    let s = "s-b1";
    let r = hub
        .dispatch(dev(), send_req("stale-1", s, "x"))
        .await
        .expect("派发");
    assert_eq!(r.state, ActionState::Failed, "陈旧目标必须失败：{r:?}");
    assert_eq!(fake.calls().len(), 0, "复核不过不得触碰原生通道");
    let view = hub.action(&dev(), "stale-1").await.expect("读");
    assert_eq!(
        view.receipt.reason_code.as_deref(),
        Some("dsh_target_stale")
    );
}

/// B2：设备归属——别的设备碰同一动作 → 不存在；会话归属——原生发送拿到的
/// target 会话就是动作自己的会话（载荷与身份摘要绑定，无跨会话取载荷通道）。
#[tokio::test]
async fn device_and_session_ownership_hold_on_native_path() {
    let (hub, fake, _cell, _tmp) = hub_with(
        vec![
            NativeSendOutcome::Accepted {
                native_id: Some("n2".into()),
            },
            NativeSendOutcome::Accepted {
                native_id: Some("n1".into()),
            },
        ],
        1,
    );
    let r = hub
        .dispatch(dev(), send_req("own-1", "s-own-a", "a"))
        .await
        .expect("派发");
    assert_eq!(r.state, ActionState::Accepted);

    // 跨设备触碰：不存在（不泄露存在性）。
    let other = DeviceContext {
        device_id: "dev-2".into(),
        device_name: "Other".into(),
    };
    let err = hub
        .dispatch(other.clone(), send_req("own-1", "s-own-a", "a"))
        .await
        .expect_err("跨设备必须拒绝");
    assert_eq!(err.code, "DSH_ACTION_NOT_FOUND");

    // 会话归属：每个动作的原生发送 target 会话与其登记会话一致。
    let r2 = hub
        .dispatch(dev(), send_req("own-2", "s-own-b", "b"))
        .await
        .expect("派发（不同会话并行）");
    assert_eq!(r2.state, ActionState::Accepted);
    let calls = fake.calls();
    assert_eq!(calls[0], ("own-1".into(), "s-own-a".into()));
    assert_eq!(calls[1], ("own-2".into(), "s-own-b".into()));
}

// ============================================================
// C：受理与效果分离；未知投递不自动重发
// ============================================================

/// C1：受理 ≠ 成功——Accepted 回执无 evidence；效果证据错配拒绝；正确证据才成功。
#[tokio::test]
async fn accepted_is_recorded_separately_from_final_effect() {
    let (hub, _fake, _cell, _tmp) = hub_with(
        vec![NativeSendOutcome::Accepted {
            native_id: Some("n1".into()),
        }],
        1,
    );
    let r = hub
        .dispatch(dev(), send_req("eff-1", "s-c1", "x"))
        .await
        .expect("派发");
    assert_eq!(r.state, ActionState::Accepted);
    let view = hub.action(&dev(), "eff-1").await.expect("读");
    assert!(
        view.evidence.is_none(),
        "受理阶段不得携带效果证据（受理≠成功）"
    );

    // 错配证据（错 native id）拒绝。
    let wrong = EffectEvidence {
        operation: Operation::Send,
        native_id: Some("other".into()),
        epoch: 1,
        effect_kind: EffectKind::Message,
    };
    let err = hub
        .record_effect(&dev(), "eff-1", &wrong)
        .await
        .expect_err("错配证据必须拒绝");
    assert_eq!(err.code, "DSH_ACTION_EVIDENCE_MISMATCH");

    // 正确证据 → Succeeded + 证据随行。
    let done = hub
        .record_effect(&dev(), "eff-1", &evidence("n1"))
        .await
        .expect("成功");
    assert_eq!(done.receipt.state, ActionState::Succeeded);
    assert_eq!(done.evidence.as_ref(), Some(&evidence("n1")));
}

/// C2：断线（未知投递）→ UnknownDelivery；排队中的下一条自动推进；
/// 未知投递的那条**绝不自动重发**。
#[tokio::test]
async fn unknown_delivery_never_auto_resends_and_queue_continues() {
    let (hub, fake, _cell, _tmp) = hub_with(
        vec![
            NativeSendOutcome::Accepted {
                native_id: Some("n2".into()),
            }, // m2（后弹）
            NativeSendOutcome::UnknownDelivery, // m1（先弹）
        ],
        1,
    );
    let s = "s-c2";
    let m1 = hub
        .dispatch(dev(), send_req("m1", s, "一"))
        .await
        .expect("m1");
    assert_eq!(m1.state, ActionState::UnknownDelivery, "断线 → 未知投递");

    let m2 = hub
        .dispatch(dev(), send_req("m2", s, "二"))
        .await
        .expect("m2");
    // m1 已是终态（不占会话）→ m2 立即占用并自动发送。
    assert_eq!(m2.state, ActionState::Accepted, "队列在未知投递后继续推进");

    let calls = fake.calls();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0].0, "m1");
    assert_eq!(calls[1].0, "m2");

    // 显式全量推进也不得把 m1 重发（它不是 queued）。
    hub.advance_all_queues().await;
    assert_eq!(fake.calls().len(), 2, "未知投递绝不自动重发");
    let m1_view = hub.action(&dev(), "m1").await.expect("读");
    assert_eq!(m1_view.receipt.state, ActionState::UnknownDelivery);
}

// ============================================================
// 独立审查收口：B1 深队列 / M1 拒绝推进 / B4 探针错 / M5 门关 /
// M6 不可推进队首 / M3 Rejected 端到端
// ============================================================

/// B1 回归：200 条排队消息全链排水不栈溢出（迭代泵）。
#[tokio::test]
async fn two_hundred_message_chain_drains_without_stack_overflow() {
    let n = 200;
    // 全部拒绝：每条终态 → 泵推进下一条（诚实 fastest path；若递归会栈溢出 abort）。
    let outcomes: Vec<_> = std::iter::repeat_n(
        NativeSendOutcome::Rejected {
            code: "gateway/arguments-invalid".into(),
        },
        n,
    )
    .collect();
    let (hub, fake, _c, tmp) = hub_with(outcomes, 1);
    let db_path = tmp.path().join("tuvis.db");
    let s = "s-deep";
    // 走正常登记记录受理证据，待 200 条排好后再一次性启动泵。
    {
        let conn = Connection::open(&db_path).expect("连接");
        let store = DshActionStore::new(&conn);
        for i in 0..n {
            let request = send_req(&format!("deep-{i}"), s, &format!("m{i}"));
            assert_eq!(
                store.register(&dev(), &request).unwrap().state,
                ActionState::Queued
            );
        }
    }
    hub.advance_all_queues().await;
    assert_eq!(fake.calls().len(), n, "每条恰好发送一次");
    let conn = Connection::open(&db_path).expect("连接");
    let failed: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM dsh_actions WHERE state='failed'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(failed, n as i64, "全部落终态");
}

/// M1+M3：在途消息被**拒绝**（终态）→ 排队中的下一条自动推进（Rejected 端到端）。
#[tokio::test]
async fn rejected_in_flight_auto_advances_queued_next() {
    let (hub, fake, _c, _tmp) = hub_with(
        vec![
            NativeSendOutcome::Accepted {
                native_id: Some("n2".into()),
            },
            NativeSendOutcome::Rejected {
                code: "gateway/arguments-invalid".into(),
            },
        ],
        1,
    );
    let s = "s-rej";
    let m1 = hub
        .dispatch(dev(), send_req("r1", s, "一"))
        .await
        .expect("m1");
    assert_eq!(
        m1.state,
        ActionState::Failed,
        "原生拒绝 → Failed（不是 Accepted/Unknown）"
    );
    assert_eq!(m1.reason_code.as_deref(), Some("gateway/arguments-invalid"));
    // m1 终态后 dispatch m2：无在途 → 立即占用+发送。
    let m2 = hub
        .dispatch(dev(), send_req("r2", s, "二"))
        .await
        .expect("m2");
    assert_eq!(m2.state, ActionState::Accepted);
    assert_eq!(fake.calls().len(), 2);
}

/// B4：探针报错（如宿主未启动）→ Failed 终态 + 原因码，不是 Reserved 楔死。
#[tokio::test]
async fn probe_error_becomes_terminal_failure() {
    use multi_agents_manager_lib::services::dsh_control::error::DshError;
    let tmp = tempfile::tempdir().expect("tempdir");
    let db_path = tmp.path().join("tuvis.db");
    {
        let conn = Connection::open(&db_path).expect("建库");
        multi_agents_manager_lib::database::schema::init(&conn).unwrap();
    }
    let shared = std::sync::Arc::new(Mutex::new(Connection::open(&db_path).expect("连接")));
    let fake = FakeNative::new(vec![NativeSendOutcome::Accepted { native_id: None }]);
    let probe: HostProbe =
        Arc::new(|| Err(DshError::new("DSH_HOST_NOT_RUNNING", "desktop not running")));
    let hub =
        DshControlHub::with_native(DshDb::Owned(Arc::clone(&shared)), fake.clone(), probe, None);
    hub.open_write_gate();
    let r = hub
        .dispatch(dev(), send_req("p1", "s-p", "x"))
        .await
        .expect("派发");
    assert_eq!(r.state, ActionState::Failed, "探针错必须终态化：{r:?}");
    assert!(r
        .reason_code
        .as_deref()
        .unwrap()
        .starts_with("dsh_probe_failed"));
    assert_eq!(fake.calls().len(), 0, "探针错零原生调用");
    // 会话未楔死：下一条照常派发（同样失败但不会卡 Queued）。
    let r2 = hub
        .dispatch(dev(), send_req("p2", "s-p", "y"))
        .await
        .expect("派发2");
    assert_eq!(r2.state, ActionState::Failed);
}

/// 缺受理证据的旧队首应拒绝，健康队尾仍能发送。
#[tokio::test]
async fn unusable_queue_head_is_failed_and_queue_continues() {
    let (hub, fake, _c, tmp) = hub_with(
        vec![NativeSendOutcome::Accepted {
            native_id: Some("ok".into()),
        }],
        1,
    );
    let db_path = tmp.path().join("tuvis.db");
    let s = "s-null";
    // 直接写入缺载荷和受理证据的旧队首；健康队尾走正常登记。
    {
        let conn = Connection::open(&db_path).expect("连接");
        conn.execute(
            "INSERT INTO dsh_actions (action_id, device_id, device_name, host_id, epoch, \
             session_id, operation, payload_hash, state, created_at_ms, updated_at_ms) \
             VALUES ('null-head', 'dev-1', 'Dev', 'local', '1', ?1, 'send', 'h', 'queued', 1, 1)",
            [s],
        )
        .expect("写入旧队首");
        DshActionStore::new(&conn)
            .register(&dev(), &send_req("good-next", s, "ok"))
            .unwrap();
    }
    hub.advance_all_queues().await;
    // 队首被收口为 failed，队尾正常发送——不再无限空转。
    let head = hub.action(&dev(), "null-head").await.expect("读队首");
    assert_eq!(head.receipt.state, ActionState::Failed);
    assert_eq!(
        head.receipt.reason_code.as_deref(),
        Some("dsh_acceptance_unproven")
    );
    let next = hub.action(&dev(), "good-next").await.expect("读队尾");
    assert_eq!(
        next.receipt.state,
        ActionState::Accepted,
        "队列不得被坏队首楔死"
    );
    assert_eq!(fake.calls().len(), 1, "只有好消息被发送");
}

/// M5：写门关闭时 record_effect / advance_all_queues 零原生发送。
#[tokio::test]
async fn closed_gate_blocks_effect_and_advance() {
    let (hub, fake, _c, _tmp) = hub_with(
        vec![
            NativeSendOutcome::Accepted {
                native_id: Some("g2".into()),
            }, // 后弹（给 g2）
            NativeSendOutcome::Accepted {
                native_id: Some("g1".into()),
            }, // 先弹（给 g1）
        ],
        1,
    );
    let s = "s-gate";
    let g1 = hub
        .dispatch(dev(), send_req("g1", s, "一"))
        .await
        .expect("g1");
    assert_eq!(g1.state, ActionState::Accepted);
    let g2 = hub
        .dispatch(dev(), send_req("g2", s, "二"))
        .await
        .expect("g2");
    assert_eq!(g2.state, ActionState::Queued);
    assert_eq!(fake.calls().len(), 1);

    hub.close_write_gate();
    let err = hub
        .record_effect(&dev(), "g1", &evidence("g1"))
        .await
        .expect_err("门关时效果提升必须拒绝");
    assert_eq!(err.code, "DSH_ACTION_WRITE_GATED");
    hub.advance_all_queues().await;
    assert_eq!(fake.calls().len(), 1, "门关时零原生发送");
    let g2v = hub.action(&dev(), "g2").await.expect("读 g2");
    assert_eq!(g2v.receipt.state, ActionState::Queued, "排队不被推进");

    // 重开门：效果落地 → 自动推进恢复。
    hub.open_write_gate();
    let done = hub
        .record_effect(&dev(), "g1", &evidence("g1"))
        .await
        .expect("效果");
    assert_eq!(done.receipt.state, ActionState::Succeeded);
    let g2b = hub.action(&dev(), "g2").await.expect("读 g2");
    assert_eq!(
        g2b.receipt.state,
        ActionState::Accepted,
        "重开后队列自动推进"
    );
}

/// M4：恢复失败后可重试（一次性标记只在成功后置位）。
#[tokio::test]
async fn failed_recovery_can_retry_and_then_open() {
    let (hub, _fake, _c, tmp) = hub_with(vec![NativeSendOutcome::Accepted { native_id: None }], 1);
    let db_path = tmp.path().join("tuvis.db");
    hub.dispatch(dev(), send_req("rc-1", "s-rc", "x"))
        .await
        .expect("占用");
    {
        let conn = Connection::open(&db_path).expect("连接");
        conn.execute_batch(
            "CREATE TRIGGER rc_boom BEFORE DELETE ON dsh_claims \
             BEGIN SELECT RAISE(ABORT, 'rc boom'); END;",
        )
        .expect("触发器");
    }
    hub.close_write_gate();
    assert!(hub.startup_recovery().is_err(), "首次失败");
    assert!(!hub.write_gate_open());
    // 修复触发器后重试——必须能成功开门（不是被一次性标记吞成 Ok(0)）。
    {
        let conn = Connection::open(&db_path).expect("连接");
        conn.execute_batch("DROP TRIGGER rc_boom;")
            .expect("删触发器");
    }
    let recovered = hub.startup_recovery().expect("重试成功");
    assert_eq!(recovered, 1);
    assert!(hub.write_gate_open(), "重试成功必须开门");
}

// ============================================================
// P1-3 / P1-4（T10 复审退回）：效果归因反例 + 最终派发边界的写门复核
// ============================================================

/// P1-4：Host 探针等待期间写门关闭 → **原生零调用**（最终派发边界复核）。
/// 今天：探针返回后直接进原生发送（1 次调用、Accepted）——RED。
/// （multi_thread：主任务要阻塞等探针进入信号，current_thread 会饿死派发任务。）
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gate_closed_during_probe_means_zero_native_calls() {
    use std::sync::mpsc;
    let (entered_tx, entered_rx) = mpsc::channel::<()>();
    let (release_tx, release_rx) = mpsc::channel::<()>();
    // Receiver 非 Sync：Mutex 包一层才能进 Send+Sync 的探针闭包。
    let release_rx = std::sync::Mutex::new(release_rx);
    let host_probe: HostProbe = Arc::new(move || {
        entered_tx.send(()).expect("探针进入信号");
        release_rx
            .lock()
            .unwrap()
            .recv_timeout(std::time::Duration::from_secs(5))
            .ok();
        Ok(LocalHost {
            host_id: "local".into(),
            platform: "macos".into(),
            profile_dir: Default::default(),
            executable: Default::default(),
            pid: 1,
            started_at_ms: 1,
            epoch: 1,
            authority: "127.0.0.1:1".into(),
            package_sha256: String::new(),
        })
    });

    let tmp = tempfile::tempdir().expect("tempdir");
    let db_path = tmp.path().join("tuvis.db");
    {
        let conn = Connection::open(&db_path).expect("建库");
        multi_agents_manager_lib::database::schema::init(&conn).unwrap();
    }
    let shared = Arc::new(Mutex::new(Connection::open(&db_path).expect("hub 连接")));
    let fake = FakeNative::new(vec![NativeSendOutcome::Accepted {
        native_id: Some("n1".into()),
    }]);
    let hub = DshControlHub::with_native(
        DshDb::Owned(Arc::clone(&shared)),
        fake.clone(),
        host_probe,
        None,
    );
    hub.open_write_gate();

    let hub2 = Arc::clone(&hub);
    let handle =
        tokio::spawn(async move { hub2.dispatch(dev(), send_req("m1", "s-gate", "hi")).await });
    entered_rx
        .recv_timeout(std::time::Duration::from_secs(5))
        .expect("探针已进入（此时动作已 Reserved）");
    hub.close_write_gate();
    release_tx.send(()).expect("放行探针");

    let receipt = handle.await.expect("join").expect("dispatch 完成");
    assert_eq!(
        fake.calls().len(),
        0,
        "探针期间关门 → 原生必须零调用（最终派发边界复核）"
    );
    assert_eq!(receipt.state, ActionState::Failed, "未发送 = 明确失败");
    assert_eq!(
        receipt.reason_code.as_deref(),
        Some("dsh_write_gated"),
        "失败原因必须指向写门：{:?}",
        receipt.reason_code
    );
    let view = hub.action(&dev(), "m1").await.expect("读回");
    assert_eq!(view.receipt.state, ActionState::Failed);
}

/// P1-3（复审反例）：外来 turn/end 不确认本动作——m1 受理后，别处回合的
/// 效果证据（同 op/epoch 但 native_id 是别的回合）不得把 m1 提成 Succeeded，
/// 也不得借 record_effect 触发队列推进。
#[tokio::test]
async fn foreign_turn_end_does_not_confirm_action() {
    let (hub, fake, _c, _tmp) = hub_with(
        vec![
            NativeSendOutcome::Accepted {
                native_id: Some("m2-native".into()),
            }, // 泵推进 m2 时弹出
            NativeSendOutcome::Accepted {
                native_id: Some("m1-native".into()),
            },
        ],
        1,
    );
    let s = "s-foreign";
    hub.dispatch(dev(), send_req("m1", s, "一"))
        .await
        .expect("m1 受理");
    hub.dispatch(dev(), send_req("m2", s, "二"))
        .await
        .expect("m2 忙态排队");
    assert_eq!(fake.calls().len(), 1);

    // 外来回合的效果证据：操作/代际都对，但 native_id 属于别的回合。
    let foreign = EffectEvidence {
        operation: Operation::Send,
        native_id: Some("turn-of-someone-else".into()),
        epoch: 1,
        effect_kind: EffectKind::Message,
    };
    let err = hub
        .record_effect(&dev(), "m1", &foreign)
        .await
        .expect_err("外来 turn/end 不得确认本动作");
    assert_eq!(err.code, "DSH_ACTION_EVIDENCE_MISMATCH");

    let m1 = hub.action(&dev(), "m1").await.expect("读 m1");
    assert_eq!(m1.receipt.state, ActionState::Accepted, "m1 必须停在受理");
    // 队列不得被外来事件推进：m2 仍排队，原生调用数不变。
    let m2 = hub.action(&dev(), "m2").await.expect("读 m2");
    assert_eq!(m2.receipt.state, ActionState::Queued, "m2 不得被推进");
    assert_eq!(fake.calls().len(), 1, "无第二次原生发送");
}

// ============================================================
// P1-D（复审）：启动泵就绪预检——凭据未就绪时泵保留 Queued（不误标 Failed）。
// ============================================================

/// 启动恢复拒绝缺少受理证据或受理于上一进程的动作，同 ID 重试应重放拒绝。
#[tokio::test]
async fn startup_recovery_replays_rejection_for_unproven_queued_action_ids() {
    let (hub, fake, _c, tmp) = hub_with(vec![NativeSendOutcome::Accepted { native_id: None }], 1);
    let device = dev();
    let legacy = send_req("legacy-queued", "s-recovery", "旧库动作");
    let prior_boot = send_req("prior-boot-queued", "s-recovery", "上次启动的动作");
    {
        let conn = Connection::open(tmp.path().join("tuvis.db")).unwrap();
        let store = DshActionStore::new(&conn);
        store.register(&device, &legacy).unwrap();
        store.register(&device, &prior_boot).unwrap();
        // 保留旧库已有的身份摘要，模拟迁移后尚无 send_acceptance 的排队行。
        conn.execute(
            "UPDATE dsh_actions SET send_acceptance = NULL WHERE action_id = ?1",
            [&legacy.action_id],
        )
        .unwrap();
        // 只替换进程标识。SQLite 对 u128 JSON 数字转换会丢精度，直接改原数字文本。
        let stored: String = conn
            .query_row(
                "SELECT send_acceptance FROM dsh_actions WHERE action_id = ?1",
                [&prior_boot.action_id],
                |row| row.get(0),
            )
            .unwrap();
        let old_boot = stored
            .split_once("\"boot\":")
            .unwrap()
            .1
            .split(',')
            .next()
            .unwrap();
        let replacement = if old_boot == "0" { "1" } else { "0" };
        let previous_acceptance = stored.replacen(
            &format!("\"boot\":{old_boot}"),
            &format!("\"boot\":{replacement}"),
            1,
        );
        conn.execute(
            "UPDATE dsh_actions SET send_acceptance = ?1 WHERE action_id = ?2",
            [&previous_acceptance, &prior_boot.action_id],
        )
        .unwrap();
    }
    hub.close_write_gate();
    hub.startup_recovery().expect("启动恢复");

    for request in [legacy, prior_boot] {
        let restored = hub
            .action(&device, &request.action_id)
            .await
            .unwrap()
            .receipt;
        assert_eq!(restored.state, ActionState::Failed);
        assert_eq!(
            restored.reason_code.as_deref(),
            Some("dsh_acceptance_unproven")
        );
        let replay = hub
            .dispatch(device.clone(), request)
            .await
            .expect("同 ID 重放原拒绝回执");
        assert_eq!(replay.action_id, restored.action_id);
        assert_eq!(replay.state, restored.state);
        assert_eq!(replay.reason_code, restored.reason_code);
    }
    assert!(fake.calls().is_empty(), "恢复和同 ID 重放均须零原生调用");
    let fresh = hub
        .dispatch(
            device,
            send_req("fresh-after-recovery", "s-recovery", "新动作"),
        )
        .await
        .unwrap();
    assert_eq!(fresh.state, ActionState::Accepted);
    assert_eq!(fake.calls().len(), 1);
}

/// 重启场景：m1 受理 → 崩溃恢复（→unknown）→ **凭据未就绪** → 启动泵不得把
/// m2 推进成 PEER_UNAUTHORIZED 永久 Failed；凭据就绪后再推进真发送。
#[tokio::test]
async fn startup_pump_waits_for_native_readiness_before_advancing() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let db_path = tmp.path().join("tuvis.db");
    {
        let conn = Connection::open(&db_path).expect("建库");
        multi_agents_manager_lib::database::schema::init(&conn).unwrap();
    }
    let shared = Arc::new(Mutex::new(Connection::open(&db_path).expect("hub 连接")));
    let fake = FakeNative::new(vec![
        NativeSendOutcome::Accepted {
            native_id: Some("n2".into()),
        }, // 凭据就绪后 m2 弹出
        NativeSendOutcome::Accepted {
            native_id: Some("n1".into()),
        },
    ]);
    let (_cell, host_probe) = probe(1);
    use std::sync::atomic::{AtomicBool, Ordering};
    let ready = Arc::new(AtomicBool::new(true));
    let hub = DshControlHub::with_readiness(
        DshDb::Owned(Arc::clone(&shared)),
        fake.clone(),
        host_probe,
        Arc::clone(&ready),
    );
    hub.open_write_gate();

    // 重启前：m1 受理，m2 忙态排队。
    hub.dispatch(dev(), send_req("m1", "s-ready", "一"))
        .await
        .expect("m1");
    hub.dispatch(dev(), send_req("m2", "s-ready", "二"))
        .await
        .expect("m2");
    assert_eq!(fake.calls().len(), 1);

    // 重启：恢复把 m1 收口 unknown（不重投），开门。
    let recovered = hub.startup_recovery().expect("恢复");
    assert_eq!(recovered, 1, "accepted 也在恢复收口范围（IN_FLIGHT_SQL）");
    let m1 = hub.action(&dev(), "m1").await.expect("读 m1");
    assert_eq!(
        m1.receipt.state,
        ActionState::UnknownDelivery,
        "重启恢复把 accepted 收口为 unknown（不自动重投）"
    );

    // 凭据未就绪：启动泵必须零原生调用、m2 保持 Queued。
    ready.store(false, Ordering::SeqCst);
    hub.advance_all_queues().await;
    assert_eq!(
        fake.calls().len(),
        1,
        "凭据未就绪 → 泵零原生调用（不得误标 Failed）"
    );
    let m2 = hub.action(&dev(), "m2").await.expect("读 m2");
    assert_eq!(m2.receipt.state, ActionState::Queued, "m2 保留排队");

    // 凭据就绪：泵推进 m2 真发送。
    ready.store(true, Ordering::SeqCst);
    hub.advance_all_queues().await;
    assert_eq!(fake.calls().len(), 2, "就绪后 m2 真发送");
    let m2b = hub.action(&dev(), "m2").await.expect("读 m2");
    assert_eq!(m2b.receipt.state, ActionState::Accepted);
}

// ============================================================
// P1-5（第三轮复审）：启动泵放弃后（>120s 窗口），凭据就绪事件必须重新
// 唤醒队列（on_credentials_delivered → advance_all_queues）。
// ============================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn late_credentials_wake_the_queue() {
    let (hub, fake, _c, _tmp) = hub_with(
        vec![
            NativeSendOutcome::Accepted {
                native_id: Some("n2".into()),
            },
            NativeSendOutcome::Accepted {
                native_id: Some("n1".into()),
            },
        ],
        1,
    );

    // m1 受理,m2 排队（模拟启动泵因未就绪而放弃后的残局）。
    hub.dispatch(dev(), send_req("m1", "s-late", "一"))
        .await
        .expect("m1");
    hub.dispatch(dev(), send_req("m2", "s-late", "二"))
        .await
        .expect("m2");
    assert_eq!(fake.calls().len(), 1);

    // m1 效果落地 → 泵放行 m2……这里是正常路径;真正的「迟到凭据」形态:
    // m1 在重启中已被收口 unknown,队列滞留 m2,凭据事件直接触发 advance。
    // 用 recover 把 m1 收口 unknown(重启残留),此时 record_effect 不可达。
    hub.startup_recovery().expect("恢复");
    let m1 = hub.action(&dev(), "m1").await.expect("读 m1");
    assert_eq!(m1.receipt.state, ActionState::UnknownDelivery);

    // 凭据就绪事件 → 必须排水(把滞留的 queued 推进)。
    hub.on_credentials_delivered().await;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while fake.calls().len() < 2 && std::time::Instant::now() < deadline {
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    assert_eq!(fake.calls().len(), 2, "凭据就绪必须唤醒队列——m2 被真发送");
    let m2 = hub.action(&dev(), "m2").await.expect("读 m2");
    assert_eq!(m2.receipt.state, ActionState::Accepted);
}
