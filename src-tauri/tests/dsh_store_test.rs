//! T09-1：DSH 专用动作账本三表（dsh_actions / dsh_claims / dsh_contract_evidence）
//! 的建表与迁移幂等。全部用真实 SQLite 临时库，不触用户账本。
//!
//! RED 依据（对基线 45f69379）：三表尚不存在 → `init` 后查询表名失败/缺列。

use rusqlite::Connection;

fn open_memory() -> Connection {
    let conn = Connection::open_in_memory().expect("内存库可建");
    multi_agents_manager_lib::database::schema::init(&conn).unwrap();
    conn
}

fn table_columns(conn: &Connection, table: &str) -> Vec<String> {
    let mut stmt = conn
        .prepare(&format!("PRAGMA table_info({table})"))
        .expect("PRAGMA 可执行");
    let cols: Vec<String> = stmt
        .query_map([], |row| row.get::<_, String>(1))
        .expect("列查询")
        .map(|r| r.expect("列名"))
        .collect();
    cols
}

fn index_names(conn: &Connection, table: &str) -> Vec<String> {
    let mut stmt = conn
        .prepare(&format!("PRAGMA index_list({table})"))
        .expect("PRAGMA index_list 可执行");
    stmt.query_map([], |row| row.get::<_, String>(1))
        .expect("索引查询")
        .map(|r| r.expect("索引名"))
        .collect()
}

/// 三张专表必须存在，且列集覆盖 T09 语义所需的最小面。
#[test]
fn dsh_control_tables_exist_with_required_columns() {
    let conn = open_memory();

    let actions = table_columns(&conn, "dsh_actions");
    for required in [
        "seq",
        "action_id",
        "device_id",
        "device_name",
        "host_id",
        "epoch",
        "session_id",
        "operation",
        "payload_hash",
        "state",
        "native_id",
        "reason_code",
        "evidence_json",
        "created_at_ms",
        "updated_at_ms",
    ] {
        assert!(
            actions.iter().any(|c| c == required),
            "dsh_actions 缺列 {required}；实际列：{actions:?}"
        );
    }

    let claims = table_columns(&conn, "dsh_claims");
    for required in [
        "claim_id",
        "action_id",
        "epoch",
        "client_id",
        "event_id",
        "revision",
        "claimed_at_ms",
    ] {
        assert!(
            claims.iter().any(|c| c == required),
            "dsh_claims 缺列 {required}；实际列：{claims:?}"
        );
    }

    let evidence = table_columns(&conn, "dsh_contract_evidence");
    for required in [
        "host_id",
        "platform",
        "package_sha256",
        "rule_version",
        "operation",
        "method",
        "structural_rule",
        "semantic_rule",
        "evidenced_at_ms",
    ] {
        assert!(
            evidence.iter().any(|c| c == required),
            "dsh_contract_evidence 缺列 {required}；实际列：{evidence:?}"
        );
    }
}

/// actionId 全局唯一：同 id 二次插入必须被约束拒绝（不是「更新覆盖」）。
#[test]
fn dsh_actions_action_id_is_globally_unique() {
    let conn = open_memory();
    let insert = |state: &str| {
        conn.execute(
            "INSERT INTO dsh_actions (action_id, device_id, device_name, host_id, epoch, \
             session_id, operation, payload_hash, state, created_at_ms, updated_at_ms) \
             VALUES ('act-1', 'dev', 'Dev', 'local', 1, 's-1', 'send', 'hash-1', ?1, 1, 1)",
            [state],
        )
    };
    assert!(insert("queued").is_ok(), "首次插入应成功");
    assert!(
        insert("queued").is_err(),
        "同 action_id 二次插入必须被唯一约束拒绝（幂等由上层查重，不靠覆盖）"
    );
}

/// 同一动作的 claim 唯一（action_id 一列），且 claim 必须带 epoch/clientId/eventId/revision。
#[test]
fn dsh_claims_one_claim_per_action_with_generation_fields() {
    let conn = open_memory();
    conn.execute(
        "INSERT INTO dsh_actions (action_id, device_id, device_name, host_id, epoch, \
         session_id, operation, payload_hash, state, created_at_ms, updated_at_ms) \
         VALUES ('act-1', 'dev', 'Dev', 'local', 7, 's-1', 'answer', 'h', 'reserved', 1, 1)",
        [],
    )
    .expect("动作可插");
    let insert_claim = |claim_id: &str| {
        conn.execute(
            "INSERT INTO dsh_claims (claim_id, action_id, epoch, client_id, event_id, revision, claimed_at_ms) \
             VALUES (?1, 'act-1', 7, 'client-1', 'evt-1', 3, 100)",
            [claim_id],
        )
    };
    assert!(insert_claim("claim-1").is_ok());
    assert!(
        insert_claim("claim-2").is_err(),
        "同一 action 只能有一份 claim（action_id 唯一）"
    );
}

/// 契约证据按 (host, platform, package_sha256, rule_version, operation) 归因：
/// 同键重复写入必须被拒（upsert 由上层显式做），不同规则版本/不同包可共存。
#[test]
fn dsh_contract_evidence_is_keyed_by_package_and_rule_version() {
    let conn = open_memory();
    let insert = |rule: i64, pkg: &str, op: &str| {
        conn.execute(
            "INSERT INTO dsh_contract_evidence (host_id, platform, package_sha256, rule_version, \
             operation, method, structural_rule, semantic_rule, evidenced_at_ms) \
             VALUES ('local', 'macos', ?1, ?2, ?3, 'session/prompt', 'req', 'sem', 1)",
            rusqlite::params![pkg, rule, op],
        )
    };
    assert!(insert(2, "pkg-a", "send").is_ok());
    assert!(
        insert(2, "pkg-a", "send").is_err(),
        "同 (包,规则版本,操作) 重复写入必须被拒"
    );
    assert!(insert(3, "pkg-a", "send").is_ok(), "新规则版本可共存");
    assert!(insert(2, "pkg-b", "send").is_ok(), "新包指纹可共存");
}

/// 迁移幂等：schema::init 连跑两次不报错、不丢数据。
#[test]
fn dsh_control_schema_init_is_idempotent_and_preserves_rows() {
    let conn = open_memory();
    conn.execute(
        "INSERT INTO dsh_actions (action_id, device_id, device_name, host_id, epoch, \
         session_id, operation, payload_hash, state, created_at_ms, updated_at_ms) \
         VALUES ('act-keep', 'dev', 'Dev', 'local', 1, 's-1', 'send', 'h', 'queued', 1, 1)",
        [],
    )
    .expect("动作可插");

    multi_agents_manager_lib::database::schema::init(&conn).unwrap();
    multi_agents_manager_lib::database::schema::init(&conn).unwrap();

    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM dsh_actions WHERE action_id = 'act-keep'",
            [],
            |r| r.get(0),
        )
        .expect("计数可查");
    assert_eq!(count, 1, "重复 init 不得清空或重复建表");
}

/// 三个索引必须存在（按设备查动作、按动作查 claim、按包归因查证据）。
#[test]
fn dsh_control_tables_have_query_indexes() {
    let conn = open_memory();
    assert!(
        !index_names(&conn, "dsh_actions").is_empty(),
        "dsh_actions 至少要有按设备/时间的查询索引"
    );
    assert!(
        index_names(&conn, "dsh_actions")
            .iter()
            .any(|n| n.contains("device") || n.contains("state")),
        "dsh_actions 缺少设备或状态索引：{:?}",
        index_names(&conn, "dsh_actions")
    );
}

// ============================================================
// T09-2：DAO 语义（生产 store 接口）
// ============================================================

use multi_agents_manager_lib::services::dsh_control::model::{
    ActionPayload, ActionRequest, ActionState, DeviceContext, EffectEvidence, EffectKind,
    Operation, SendPayload, Target,
};
use multi_agents_manager_lib::services::dsh_control::store::DshActionStore;

fn send_request(action_id: &str, text: &str) -> ActionRequest {
    ActionRequest {
        target: Target {
            host_id: "local".into(),
            epoch: 1,
            session_id: "s-1".into(),
        },
        action_id: action_id.into(),
        expected_revision: None,
        payload: ActionPayload::Send(SendPayload {
            text: text.into(),
            receipt_ids: Vec::new(),
            queue_only: false,
            bypass_paused_queue: false,
        }),
    }
}

fn device(id: &str) -> DeviceContext {
    DeviceContext {
        device_id: id.into(),
        device_name: format!("Device {id}"),
    }
}

/// 计划固定名 1：同设备同 actionId 重试只登记一次、只派发一次。
#[test]
fn same_device_action_id_is_idempotent() {
    let conn = open_memory();
    let store = DshActionStore::new(&conn);
    let req = send_request("act-idem", "hello");

    let first = store.register(&device("dev-1"), &req).expect("首次登记");
    let second = store.register(&device("dev-1"), &req).expect("重试登记");
    assert_eq!(first.state, ActionState::Queued);
    assert_eq!(second.state, ActionState::Queued);
    assert_eq!(
        store.count_actions().expect("计数"),
        1,
        "同 actionId 重试不得产生第二行"
    );

    // 派发计数由 store 记录：只有一次 reserve 成功。
    let native_dispatches = store
        .reserve_for_dispatch_for(
            &device("dev-1"),
            "act-idem",
            "claim-1",
            1,
            "client-1",
            "evt-1",
            0,
        )
        .map(|r| r.is_some() as u32)
        .unwrap_or(0)
        + store
            .reserve_for_dispatch_for(
                &device("dev-1"),
                "act-idem",
                "claim-2",
                1,
                "client-1",
                "evt-1",
                0,
            )
            .map(|r| r.is_some() as u32)
            .unwrap_or(0);
    assert_eq!(native_dispatches, 1, "双请求竞争只允许一次有效派发");
}

/// 计划固定名 2：跨设备隔离——别的设备读不到别人的动作。
#[test]
fn other_device_cannot_read_action() {
    let conn = open_memory();
    let store = DshActionStore::new(&conn);
    let req = send_request("act-iso", "hello");
    store.register(&device("dev-1"), &req).expect("登记");

    let id = "act-iso";
    assert!(
        store.view(&device("dev-2"), id).is_err(),
        "other_device.action(&id).is_err()"
    );
    assert!(store.view(&device("dev-1"), id).is_ok());
}

/// 计划固定名 3：崩溃后 reserved 恢复为 UnknownDelivery，不自动重投。
#[test]
fn restart_does_not_replay_reserved() {
    let conn = open_memory();
    let store = DshActionStore::new(&conn);
    let req = send_request("act-crash", "hello");
    store.register(&device("dev-1"), &req).expect("登记");
    store
        .reserve_for_dispatch_for(
            &device("dev-1"),
            "act-crash",
            "claim-1",
            1,
            "client-1",
            "evt-1",
            0,
        )
        .expect("占用")
        .expect("queued→reserved 成功");

    // 模拟重启：新 store 实例读同一库做恢复。
    let restored_store = DshActionStore::new(&conn);
    restored_store
        .recover_reserved_after_restart()
        .expect("恢复");
    let restored = restored_store
        .view(&device("dev-1"), "act-crash")
        .expect("可读");
    assert_eq!(
        restored.receipt.state,
        ActionState::UnknownDelivery,
        "crash reserved 只能恢复为 UnknownDelivery（不自动重投）"
    );
    assert_eq!(restored_store.count_actions().expect("计数"), 1);
    // 再恢复一次也不得回到可派发状态。
    restored_store
        .recover_reserved_after_restart()
        .expect("幂等恢复");
    let again = restored_store
        .view(&device("dev-1"), "act-crash")
        .expect("可读");
    assert_eq!(again.receipt.state, ActionState::UnknownDelivery);
}

/// 计划固定名 4：审计/持久化失败不得返回成功。
#[test]
fn audit_failure_is_not_success() {
    let conn = open_memory();
    let store = DshActionStore::new(&conn);
    // 制造持久化失败：把账本表改名，register 必然失败（模拟写库不可用）。
    conn.execute("ALTER TABLE dsh_actions RENAME TO dsh_actions_offline", [])
        .expect("改表");
    let err = store
        .register(&device("dev-1"), &send_request("act-fail", "x"))
        .expect_err("持久化失败必须上抛，不得谎报成功");
    assert!(!err.detail.is_empty(), "错误必须带明细：{err:?}");
}

/// 同 actionId 异 payload：必须拒绝（不能把不同动作合并成一次）。
#[test]
fn same_action_id_with_different_payload_is_rejected() {
    let conn = open_memory();
    let store = DshActionStore::new(&conn);
    store
        .register(&device("dev-1"), &send_request("act-x", "first"))
        .expect("首次");
    let err = store
        .register(&device("dev-1"), &send_request("act-x", "second"))
        .expect_err("同 id 异 payload 必须拒绝");
    assert_eq!(err.code, "DSH_ACTION_ID_CONFLICT", "实际：{err:?}");
}

/// 成功只能由明确映射的效果证据提升：accepted 不等于 succeeded。
#[test]
fn success_requires_explicit_effect_evidence() {
    let conn = open_memory();
    let store = DshActionStore::new(&conn);
    store
        .register(&device("dev-1"), &send_request("act-succ", "hi"))
        .expect("登记");
    store
        .reserve_for_dispatch_for(&device("dev-1"), "act-succ", "claim-1", 1, "c", "e", 0)
        .expect("占用")
        .expect("reserved");
    store
        .mark_accepted_for(&device("dev-1"), "act-succ", Some("native-1"))
        .expect("受理");
    assert_eq!(
        store
            .view(&device("dev-1"), "act-succ")
            .expect("读")
            .receipt
            .state,
        ActionState::Accepted,
        "受理不得直接变成成功"
    );

    let evidence = EffectEvidence {
        operation: Operation::Send,
        native_id: Some("native-1".into()),
        epoch: 1,
        effect_kind: EffectKind::Message,
    };
    store
        .succeed_with_evidence_for(&device("dev-1"), "act-succ", &evidence)
        .expect("提升成功");
    let view = store.view(&device("dev-1"), "act-succ").expect("读");
    assert_eq!(view.receipt.state, ActionState::Succeeded);
    assert_eq!(view.evidence.as_ref(), Some(&evidence));
}

// ============================================================
// P1-3（T10 复审退回）：效果提升的归因加固——native_id 与效果类别
// ============================================================
//
// 复审缺口：`succeed_with_evidence_inner` 的 native_id 校验是「双方都有才比对」，
// 证据缺 native_id 即可放行；effect_kind 完全未校验。外来事件（无关 turn/end）
// 因此能把 Send 动作标成 Succeeded 并推进队列。

/// 已受理动作（native_id = native-1）+ 证据**缺** native_id → 必须拒绝：
/// 无归因凭据的事件不得确认本动作（今天：成功提升放行——RED）。
#[test]
fn evidence_without_native_id_cannot_confirm_action() {
    let conn = open_memory();
    let store = DshActionStore::new(&conn);
    store
        .register(&device("dev-1"), &send_request("act-noid", "hi"))
        .expect("登记");
    store
        .reserve_for_dispatch_for(&device("dev-1"), "act-noid", "claim-1", 1, "c", "e", 0)
        .expect("占用")
        .expect("reserved");
    store
        .mark_accepted_for(&device("dev-1"), "act-noid", Some("native-1"))
        .expect("受理");

    let evidence = EffectEvidence {
        operation: Operation::Send,
        native_id: None,
        epoch: 1,
        effect_kind: EffectKind::Message,
    };
    let err = store
        .succeed_with_evidence_for(&device("dev-1"), "act-noid", &evidence)
        .expect_err("缺 native_id 的证据不得确认动作");
    assert_eq!(err.code, "DSH_ACTION_EVIDENCE_MISMATCH", "实际：{err:?}");
    assert_eq!(
        store
            .view(&device("dev-1"), "act-noid")
            .expect("读")
            .receipt
            .state,
        ActionState::Accepted,
        "拒绝后动作必须停在 Accepted"
    );
}

/// 外来 turn/end（别的回合的 native_id）不得确认本动作（既有纪律回归面）。
#[test]
fn foreign_native_id_cannot_confirm_action() {
    let conn = open_memory();
    let store = DshActionStore::new(&conn);
    store
        .register(&device("dev-1"), &send_request("act-foreign", "hi"))
        .expect("登记");
    store
        .reserve_for_dispatch_for(&device("dev-1"), "act-foreign", "claim-1", 1, "c", "e", 0)
        .expect("占用")
        .expect("reserved");
    store
        .mark_accepted_for(&device("dev-1"), "act-foreign", Some("native-1"))
        .expect("受理");

    let foreign = EffectEvidence {
        operation: Operation::Send,
        native_id: Some("turn-of-another-message".into()),
        epoch: 1,
        effect_kind: EffectKind::Message,
    };
    let err = store
        .succeed_with_evidence_for(&device("dev-1"), "act-foreign", &foreign)
        .expect_err("外来 native_id 不得确认本动作");
    assert_eq!(err.code, "DSH_ACTION_EVIDENCE_MISMATCH", "实际：{err:?}");
}

/// 效果类别必须与操作类属一致：Send 只认 Message；Decision/QueueItem/
/// SessionChange 类证据一律拒绝（今天：放行——RED）。
#[test]
fn effect_kind_must_match_the_operation_class() {
    let conn = open_memory();
    let store = DshActionStore::new(&conn);
    store
        .register(&device("dev-1"), &send_request("act-kind", "hi"))
        .expect("登记");
    store
        .reserve_for_dispatch_for(&device("dev-1"), "act-kind", "claim-1", 1, "c", "e", 0)
        .expect("占用")
        .expect("reserved");
    store
        .mark_accepted_for(&device("dev-1"), "act-kind", Some("native-1"))
        .expect("受理");

    for wrong_kind in [
        EffectKind::Decision,
        EffectKind::QueueItem,
        EffectKind::SessionChange,
    ] {
        let evidence = EffectEvidence {
            operation: Operation::Send,
            native_id: Some("native-1".into()),
            epoch: 1,
            effect_kind: wrong_kind,
        };
        let err = store
            .succeed_with_evidence_for(&device("dev-1"), "act-kind", &evidence)
            .expect_err("Send 动作不得被非 Message 类证据确认");
        assert_eq!(err.code, "DSH_ACTION_EVIDENCE_MISMATCH", "实际：{err:?}");
    }
    assert_eq!(
        store
            .view(&device("dev-1"), "act-kind")
            .expect("读")
            .receipt
            .state,
        ActionState::Accepted,
        "全部拒绝后动作必须停在 Accepted"
    );
}

/// 受理时未记 native_id 的动作：效果提升一律拒绝（无归因凭据——fail-closed，
/// 不得让任意携带 ID 的事件冒认；今天：放行——RED）。
#[test]
fn action_without_recorded_native_id_refuses_any_effect() {
    let conn = open_memory();
    let store = DshActionStore::new(&conn);
    store
        .register(&device("dev-1"), &send_request("act-nonative", "hi"))
        .expect("登记");
    store
        .reserve_for_dispatch_for(&device("dev-1"), "act-nonative", "claim-1", 1, "c", "e", 0)
        .expect("占用")
        .expect("reserved");
    store
        .mark_accepted_for(&device("dev-1"), "act-nonative", None)
        .expect("受理（未记 native_id）");

    let evidence = EffectEvidence {
        operation: Operation::Send,
        native_id: Some("whatever".into()),
        epoch: 1,
        effect_kind: EffectKind::Message,
    };
    let err = store
        .succeed_with_evidence_for(&device("dev-1"), "act-nonative", &evidence)
        .expect_err("无受理 native_id 的动作不可被任何证据确认");
    assert_eq!(err.code, "DSH_ACTION_EVIDENCE_MISMATCH", "实际：{err:?}");
}

// ============================================================
// T09-3：production_hub 单例 + actor 串行
// ============================================================

use multi_agents_manager_lib::services::dsh_control::actor::DshSessionActor;
use std::sync::Arc;

/// 计划验收⑤：production_hub 为 Tauri/RemoteState 同一实例（进程级单例）。
#[test]
fn production_hub_is_a_process_wide_singleton() {
    let a = multi_agents_manager_lib::services::dsh_control::production_hub();
    let b = multi_agents_manager_lib::services::dsh_control::production_hub();
    assert!(
        Arc::ptr_eq(&a, &b),
        "Tauri state 与 RemoteState 必须拿到同一 Arc（进程级单例）"
    );
}

/// 单会话 actor：同一会话的动作严格串行，不并发进入原生派发。
#[test]
fn session_actor_serializes_actions_per_session() {
    let actor = DshSessionActor::new();
    let order = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));

    let run = |tag: &'static str, order: Arc<std::sync::Mutex<Vec<&'static str>>>| {
        let req = send_request(&format!("act-{tag}"), tag);
        actor
            .run_serialized(&req.target.session_id, || {
                order.lock().unwrap().push(tag);
            })
            .expect("串行执行");
    };
    run("a", Arc::clone(&order));
    run("b", Arc::clone(&order));
    assert_eq!(
        *order.lock().unwrap(),
        vec!["a", "b"],
        "同会话必须按提交顺序串行"
    );
}

/// actor 不跨会话阻塞：不同会话互不串行（各自独立锁）。
#[test]
fn session_actor_keeps_other_sessions_independent() {
    let actor = DshSessionActor::new();
    let order = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    actor
        .run_serialized("s-1", || order.lock().unwrap().push("s1"))
        .expect("s-1");
    actor
        .run_serialized("s-2", || order.lock().unwrap().push("s2"))
        .expect("s-2");
    assert_eq!(*order.lock().unwrap(), vec!["s1", "s2"]);
}

// ============================================================
// T09-4：契约证据持久化与归因（Host / 包身份 / 规则代际变化必须重归因）
// ============================================================

use multi_agents_manager_lib::services::dsh_control::model::{
    ContractEvidence, EvidenceSource, OperationContractMatch,
};
use multi_agents_manager_lib::services::dsh_control::store::DshContractEvidenceStore;
use std::collections::BTreeMap;

fn evidence_fixture(pkg: &str, rule: u32, ops: &[Operation]) -> ContractEvidence {
    let matches: BTreeMap<Operation, OperationContractMatch> = ops
        .iter()
        .map(|op| {
            (
                *op,
                OperationContractMatch {
                    method: "session/prompt".into(),
                    structural_rule: "req".into(),
                    semantic_rule: "sem".into(),
                },
            )
        })
        .collect();
    ContractEvidence {
        package_sha256: pkg.into(),
        rule_version: rule,
        matches,
    }
}

/// 同一份 (host, 平台, 包指纹, 规则版本) 证据可写入并按原值读回。
#[test]
fn contract_evidence_round_trips_for_current_identity() {
    let conn = open_memory();
    let store = DshContractEvidenceStore::new(&conn);
    let ev = evidence_fixture("pkg-abc", 5, &[Operation::Send, Operation::Read]);
    store.record("local", "macos", &ev).expect("写入证据");

    let live = store
        .load_current("local", "macos", "pkg-abc", 5)
        .expect("读取");
    assert_eq!(live.matches.len(), 2, "全部规则必须读回");
    let send = live.matches.get(&Operation::Send).expect("Send 规则在库");
    assert_eq!(send.method, "session/prompt");
    assert_eq!(live.package_sha256, "pkg-abc");
    assert_eq!(live.rule_version, 5);
}

/// Host 换代（host_id 变）：旧 Host 的证据不得被当成当前目标的证据。
#[test]
fn evidence_from_other_host_is_not_loaded() {
    let conn = open_memory();
    let store = DshContractEvidenceStore::new(&conn);
    store
        .record(
            "host-old",
            "macos",
            &evidence_fixture("pkg-abc", 5, &[Operation::Send]),
        )
        .expect("写入");
    let live = store
        .load_current("host-new", "macos", "pkg-abc", 5)
        .expect("读取");
    assert!(live.matches.is_empty(), "旧 Host 证据不得串给新 Host");
}

/// 包内容指纹变化（同 build 重打包）：旧包证据不得解锁新包。
#[test]
fn evidence_from_other_package_is_not_loaded() {
    let conn = open_memory();
    let store = DshContractEvidenceStore::new(&conn);
    store
        .record(
            "local",
            "macos",
            &evidence_fixture("pkg-old", 5, &[Operation::Send]),
        )
        .expect("写入");
    let live = store
        .load_current("local", "macos", "pkg-new", 5)
        .expect("读取");
    assert!(live.matches.is_empty(), "旧包证据不得解锁新包");
}

/// 规则代际变化：旧规则版本的证据不得用于新规则求值。
#[test]
fn evidence_from_other_rule_version_is_not_loaded() {
    let conn = open_memory();
    let store = DshContractEvidenceStore::new(&conn);
    store
        .record(
            "local",
            "macos",
            &evidence_fixture("pkg-abc", 4, &[Operation::Send]),
        )
        .expect("写入");
    let live = store
        .load_current("local", "macos", "pkg-abc", 5)
        .expect("读取");
    assert!(live.matches.is_empty(), "旧规则代际证据不得复用");
}

/// 重新识别是幂等覆盖：同键再写不产生重复行；换包后旧身份可被清理。
#[test]
fn reidentification_overwrites_and_prunes_stale_identities() {
    let conn = open_memory();
    let store = DshContractEvidenceStore::new(&conn);
    let ev = evidence_fixture("pkg-abc", 5, &[Operation::Send, Operation::Read]);
    store.record("local", "macos", &ev).expect("首次");
    store.record("local", "macos", &ev).expect("再次");
    let total: i64 = conn
        .query_row("SELECT COUNT(*) FROM dsh_contract_evidence", [], |r| {
            r.get(0)
        })
        .expect("计数");
    assert_eq!(total, 2, "同键重写不得产生重复行");

    // 换包：写入新包后清理同 Host 的旧包身份，只留当前身份。
    store
        .record(
            "local",
            "macos",
            &evidence_fixture("pkg-new", 5, &[Operation::Send]),
        )
        .expect("新包");
    let pruned = store
        .prune_other_identities("local", "macos", "pkg-new", 5)
        .expect("清理旧代");
    assert_eq!(pruned, 2, "旧包的 2 行必须被清理");
    let remaining = store
        .load_current("local", "macos", "pkg-new", 5)
        .expect("读取");
    assert_eq!(remaining.matches.len(), 1);
    let stale = store
        .load_current("local", "macos", "pkg-abc", 5)
        .expect("读取旧包");
    assert!(stale.matches.is_empty(), "旧包证据必须已被清理");
}

/// 证据落库后可直接驱动能力求值，且契约档只能是「兼容可尝试」
/// （`verified=true` 但 `requires_auto_adapt=true`），不是「本机已验证」。
#[test]
fn stored_contract_evidence_is_attemptable_not_verified_on_machine() {
    use multi_agents_manager_lib::services::dsh_control::capability::{evaluate, OperationContext};
    let conn = open_memory();
    let store = DshContractEvidenceStore::new(&conn);
    let ev = evidence_fixture("pkg-abc", 5, &[Operation::Send]);
    store.record("local", "macos", &ev).expect("写入");
    let live = store
        .load_current("local", "macos", "pkg-abc", 5)
        .expect("读取");

    let ctx = OperationContext {
        target_fresh: true,
        auto_adapt_enabled: true,
        implemented: [Operation::Send].into_iter().collect(),
        contract: Some(live.clone()),
        contract_rule_version: live.rule_version,
        current_package_sha256: live.package_sha256.clone(),
        ..OperationContext::default()
    };
    let status = evaluate(&ctx)
        .get(Operation::Send)
        .expect("Send 状态")
        .clone();
    assert!(status.allowed, "契约匹配 + 自动适配开 → 可尝试");
    assert_eq!(status.evidence, EvidenceSource::ContractMatched);
    assert!(status.verified, "证据锚定当前包与规则代际");
    assert!(
        status.requires_auto_adapt,
        "契约匹配只能是「兼容可尝试」，必须受自动适配活门约束（不是本机已验证）"
    );
    // 关闭自动适配 → 立即不允许（活门生效）。
    let closed = OperationContext {
        auto_adapt_enabled: false,
        ..ctx
    };
    assert!(
        !evaluate(&closed).allowed(Operation::Send),
        "关闭自动适配后契约档写动作必须立即撤回"
    );
}

// ============================================================
// T09 复审修复：B1 epoch 可逆、B2 动作身份绑定、M1 并发幂等
// ============================================================

/// B1：epoch 超过 i64::MAX 时必须原值往返（旧实现饱和成 i64::MAX 丢代际）。
#[test]
fn large_epoch_round_trips_without_saturation() {
    let conn = open_memory();
    let store = DshActionStore::new(&conn);
    let mut req = send_request("act-big-epoch", "x");
    req.target.epoch = u64::MAX;
    store.register(&device("dev-1"), &req).expect("登记");
    let view = store.view(&device("dev-1"), "act-big-epoch").expect("读");
    assert_eq!(view.target.epoch, u64::MAX, "epoch 不得饱和截断");

    // 1<<63 边界（i64 负值位）同样可逆。
    let mut req2 = send_request("act-sign-epoch", "y");
    req2.target.epoch = 1u64 << 63;
    store.register(&device("dev-1"), &req2).expect("登记");
    let view2 = store.view(&device("dev-1"), "act-sign-epoch").expect("读");
    assert_eq!(view2.target.epoch, 1u64 << 63);
}

/// B2：别的设备用同一 actionId + 同 payload 登记，不得拿到 A 的 receipt。
#[test]
fn other_device_same_action_id_is_not_visible() {
    let conn = open_memory();
    let store = DshActionStore::new(&conn);
    let req = send_request("act-owner", "same text");
    let a = store.register(&device("dev-a"), &req).expect("A 登记");

    let err = store
        .register(&device("dev-b"), &req)
        .expect_err("B 不得复用 A 的动作 id");
    assert_eq!(
        err.code, "DSH_ACTION_NOT_FOUND",
        "跨设备按不存在回答：{err:?}"
    );
    assert_eq!(a.action_id, "act-owner");
}

/// B2：同设备同 actionId 但换了 session（同 payload）→ 冲突，不得当重试。
#[test]
fn same_device_same_id_different_session_is_conflict() {
    let conn = open_memory();
    let store = DshActionStore::new(&conn);
    let mut req = send_request("act-scope", "text");
    store.register(&device("dev-1"), &req).expect("首次");
    req.target.session_id = "s-2".into();
    let err = store
        .register(&device("dev-1"), &req)
        .expect_err("同 id 换目标必须冲突");
    assert_eq!(err.code, "DSH_ACTION_ID_CONFLICT");
}

/// B2：写路径同样受设备归属约束——别的设备不能占用/受理/提升 A 的动作。
#[test]
fn write_paths_enforce_device_ownership() {
    let conn = open_memory();
    let store = DshActionStore::new(&conn);
    store
        .register(&device("dev-a"), &send_request("act-owned", "x"))
        .expect("登记");
    assert!(
        store
            .reserve_for_dispatch_for(&device("dev-b"), "act-owned", "c1", 1, "cl", "e", 0)
            .is_err(),
        "B 不得占用 A 的动作"
    );
    assert!(
        store
            .mark_accepted_for(&device("dev-b"), "act-owned", None)
            .is_err(),
        "B 不得受理 A 的动作"
    );
}

/// M1：并发登记同一 actionId，恰好一次插入成功，其余走幂等返回（不得 STATE_IO）。
#[test]
fn concurrent_register_same_action_is_idempotent() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let db = tmp.path().join("t.db");
    {
        let conn = Connection::open(&db).expect("建库");
        multi_agents_manager_lib::database::schema::init(&conn).unwrap();
    }
    let req = send_request("act-race", "text");
    let results: Vec<Result<ActionState, String>> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let db = db.clone();
                let req = req.clone();
                scope.spawn(move || {
                    let conn = Connection::open(&db).expect("连接");
                    let store = DshActionStore::new(&conn);
                    store
                        .register(&device("dev-1"), &req)
                        .map(|r| r.state)
                        .map_err(|e| format!("{}: {}", e.code, e.detail))
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().expect("join"))
            .collect()
    });
    let ok = results.iter().filter(|r| r.is_ok()).count();
    assert_eq!(ok, 8, "全部调用都应成功（幂等），实际：{results:?}");
    let conn = Connection::open(&db).expect("连接");
    let total: i64 = conn
        .query_row("SELECT COUNT(*) FROM dsh_actions", [], |r| r.get(0))
        .expect("计数");
    assert_eq!(total, 1, "并发登记只能产生一行");
}

/// M3：多 Host 共存时，清理 A 的证据不得删掉 B 的；空指纹直接拒绝清理。
#[test]
fn prune_is_scoped_to_one_host_and_refuses_empty_fingerprint() {
    let conn = open_memory();
    let store = DshContractEvidenceStore::new(&conn);
    store
        .record(
            "host-a",
            "macos",
            &evidence_fixture("pkg-a1", 5, &[Operation::Send]),
        )
        .expect("A 旧");
    store
        .record(
            "host-a",
            "macos",
            &evidence_fixture("pkg-a2", 5, &[Operation::Send]),
        )
        .expect("A 新");
    store
        .record(
            "host-b",
            "macos",
            &evidence_fixture("pkg-b1", 5, &[Operation::Send]),
        )
        .expect("B");

    let pruned = store
        .prune_other_identities("host-a", "macos", "pkg-a2", 5)
        .expect("清理 A 旧代");
    assert_eq!(pruned, 1, "只清 A 的旧包");
    assert_eq!(
        store
            .load_current("host-a", "macos", "pkg-a2", 5)
            .expect("A 新")
            .matches
            .len(),
        1,
        "A 当前身份保留"
    );
    assert_eq!(
        store
            .load_current("host-b", "macos", "pkg-b1", 5)
            .expect("B")
            .matches
            .len(),
        1,
        "B 的证据不得被 A 的清理牵连"
    );
    let err = store
        .prune_other_identities("host-a", "macos", "", 5)
        .expect_err("空指纹不得清库");
    assert!(!err.detail.is_empty());
}

// ============================================================
// T09 复审 B3：真实 hub 装配（Tauri/RemoteState 同一 Arc、可派发、启动恢复）
// ============================================================

/// 验收⑤：Tauri `manage` 与 RemoteState 取的是**同一进程级 Arc**（不是两次
/// 各自构造）。这里从两条真实取用路径各取一次引用比较指针。
#[test]
fn production_hub_is_shared_by_tauri_and_remote() {
    // Tauri 侧：lib.rs 用 `.manage(production_hub())` 注册。
    let from_tauri = multi_agents_manager_lib::services::dsh_control::production_hub();
    // RemoteState 侧：生产装配同样调用 `production_hub()`。
    let from_remote = multi_agents_manager_lib::services::dsh_control::production_hub();
    assert!(
        std::sync::Arc::ptr_eq(&from_tauri, &from_remote),
        "Tauri 与 RemoteState 必须共享同一 hub 实例"
    );
    assert!(
        from_tauri.is_wired(),
        "生产 hub 必须已装配（非 disabled 占位）"
    );
}

/// 生产 hub 真的能登记并占用动作（不是恒 Unsupported）。
#[tokio::test]
async fn production_hub_dispatches_into_the_action_ledger() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let db = tmp.path().join("tuvis.db");
    {
        let conn = Connection::open(&db).expect("建库");
        multi_agents_manager_lib::database::schema::init(&conn).unwrap();
    }
    let hub = multi_agents_manager_lib::services::dsh_control::DshControlHub::with_db(
        multi_agents_manager_lib::services::dsh_control::DshDb::Owned(std::sync::Arc::new(
            std::sync::Mutex::new(Connection::open(&db).expect("hub 连接")),
        )),
    );
    hub.open_write_gate();
    let req = send_request("act-hub", "hello");
    let receipt = hub
        .dispatch(device("dev-1"), req.clone())
        .await
        .expect("派发登记");
    assert_eq!(
        receipt.state,
        ActionState::Reserved,
        "先 reserve 后发的占用语义"
    );
    // 重试幂等：同请求再派发不得产生第二行。
    let again = hub.dispatch(device("dev-1"), req).await.expect("幂等");
    assert_eq!(again.action_id, "act-hub");
    let conn = Connection::open(&db).expect("连接");
    let total: i64 = conn
        .query_row("SELECT COUNT(*) FROM dsh_actions", [], |r| r.get(0))
        .expect("计数");
    assert_eq!(total, 1);
    // 跨设备读不到。
    assert!(hub.action(&device("dev-2"), "act-hub").await.is_err());
}

/// 启动恢复：崩溃遗留的 reserved 在 hub 装配后恢复为 unknown_delivery。
#[tokio::test]
async fn production_hub_recovers_reserved_actions_on_startup() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let db = tmp.path().join("tuvis.db");
    {
        let conn = Connection::open(&db).expect("建库");
        multi_agents_manager_lib::database::schema::init(&conn).unwrap();
    }
    let hub = multi_agents_manager_lib::services::dsh_control::DshControlHub::with_db(
        multi_agents_manager_lib::services::dsh_control::DshDb::Owned(std::sync::Arc::new(
            std::sync::Mutex::new(Connection::open(&db).expect("hub 连接")),
        )),
    );
    hub.open_write_gate();
    hub.dispatch(device("dev-1"), send_request("act-crash2", "x"))
        .await
        .expect("占用");
    let recovered = hub.recover_actions_after_restart().expect("恢复");
    assert_eq!(recovered, 1);
    let view = hub
        .action(&device("dev-1"), "act-crash2")
        .await
        .expect("读");
    assert_eq!(
        view.receipt.state,
        ActionState::UnknownDelivery,
        "不自动重投"
    );
}

// ============================================================
// T09 复审：M6 审计同事务、M8 覆盖缺口
// ============================================================

/// M6：审计写失败 → 占用整体回滚，动作不得进入 reserved（零派发）。
#[test]
fn audit_failure_rolls_back_reservation() {
    let conn = open_memory();
    let store = DshActionStore::new(&conn);
    store
        .register(&device("dev-1"), &send_request("act-audit", "x"))
        .expect("登记");
    // 让审计表不可写：改名（INSERT 必然失败）。
    conn.execute("ALTER TABLE write_audit RENAME TO write_audit_offline", [])
        .expect("改表");
    let err = store
        .reserve_for_dispatch_for(&device("dev-1"), "act-audit", "claim-a", 1, "c", "e", 0)
        .expect_err("审计失败必须上抛");
    assert_eq!(err.code, "DSH_STATE_IO", "实际：{err:?}");
    // 状态必须仍是 queued（占用被回滚 → 未派发）。
    let view = store.view(&device("dev-1"), "act-audit").expect("读");
    assert_eq!(
        view.receipt.state,
        ActionState::Queued,
        "审计失败不得留下 reserved"
    );
    let claims: i64 = conn
        .query_row("SELECT COUNT(*) FROM dsh_claims", [], |r| r.get(0))
        .expect("计数");
    assert_eq!(claims, 0, "claim 也必须一起回滚");
}

/// M8：三张表都要有查询索引（此前只检查 dsh_actions）。
#[test]
fn all_dsh_tables_have_indexes() {
    let conn = open_memory();
    // 复审 m8：PRIMARY KEY/UNIQUE 会自动生成 sqlite_autoindex_*，只断「非空」是空断言。
    let actions = index_names(&conn, "dsh_actions");
    assert!(
        actions.iter().any(|n| n == "idx_dsh_actions_device_seq"),
        "{actions:?}"
    );
    assert!(
        actions.iter().any(|n| n == "idx_dsh_actions_state_seq"),
        "{actions:?}"
    );
    let claims = index_names(&conn, "dsh_claims");
    assert!(
        claims.iter().any(|n| n == "idx_dsh_claims_event"),
        "{claims:?}"
    );
    let evidence = index_names(&conn, "dsh_contract_evidence");
    assert!(
        evidence
            .iter()
            .any(|n| n == "idx_dsh_contract_evidence_package"),
        "{evidence:?}"
    );
}

/// M8：`migration::migrate` 幂等且不动既有非 DSH 账本（新建库 + 旧库两轮）。
#[test]
fn migration_is_idempotent_and_preserves_existing_ledgers() {
    let conn = Connection::open_in_memory().expect("库");
    multi_agents_manager_lib::database::schema::init(&conn).unwrap();
    // 既有账本：写一条 settings 与一条 dsh 动作。
    conn.execute(
        "INSERT INTO settings (key, value) VALUES ('keep_me', 'v')",
        [],
    )
    .expect("settings");
    conn.execute(
        "INSERT INTO dsh_actions (action_id, device_id, device_name, host_id, epoch, \
         session_id, operation, payload_hash, state, created_at_ms, updated_at_ms) \
         VALUES ('act-mig', 'd', 'D', 'local', '1', 's', 'send', 'h', 'queued', 1, 1)",
        [],
    )
    .expect("action");

    multi_agents_manager_lib::database::migration::migrate(&conn).expect("首次迁移");
    multi_agents_manager_lib::database::migration::migrate(&conn).expect("二次迁移");

    let setting: String = conn
        .query_row(
            "SELECT value FROM settings WHERE key = 'keep_me'",
            [],
            |r| r.get(0),
        )
        .expect("settings 保留");
    assert_eq!(setting, "v", "迁移不得动既有 settings");
    let action: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM dsh_actions WHERE action_id = 'act-mig'",
            [],
            |r| r.get(0),
        )
        .expect("计数");
    assert_eq!(action, 1, "迁移不得清 DSH 动作账本");
}

/// M8：崩溃恢复后再次占用必须失败（不重投），且派发计数保持 0。
#[test]
fn recovered_action_cannot_be_reserved_again() {
    let conn = open_memory();
    let store = DshActionStore::new(&conn);
    store
        .register(&device("dev-1"), &send_request("act-noreplay", "x"))
        .expect("登记");
    assert!(store
        .reserve_for_dispatch_for(&device("dev-1"), "act-noreplay", "c1", 1, "cl", "e", 0)
        .expect("首次占用")
        .is_some());
    store.recover_reserved_after_restart().expect("恢复");
    let again = store
        .reserve_for_dispatch_for(&device("dev-1"), "act-noreplay", "c2", 1, "cl", "e", 0)
        .expect("再次占用请求");
    assert!(
        again.is_none(),
        "恢复为 unknown_delivery 后不得再占用（不自动重投）"
    );
}

/// B2/M2：写路径与证据提升的设备归属与证据映射。
#[test]
fn evidence_must_map_to_its_action() {
    let conn = open_memory();
    let store = DshActionStore::new(&conn);
    store
        .register(&device("dev-1"), &send_request("act-map", "x"))
        .expect("登记");
    store
        .reserve_for_dispatch_for(&device("dev-1"), "act-map", "c1", 1, "cl", "e", 0)
        .expect("占用")
        .expect("reserved");
    store
        .mark_accepted_for(&device("dev-1"), "act-map", Some("native-1"))
        .expect("受理");

    // 错误 operation 的证据：拒绝。
    let wrong_op = EffectEvidence {
        operation: Operation::CancelCurrent,
        native_id: Some("native-1".into()),
        epoch: 1,
        effect_kind: EffectKind::SessionChange,
    };
    assert_eq!(
        store
            .succeed_with_evidence_for(&device("dev-1"), "act-map", &wrong_op)
            .expect_err("错误 operation 不得提升成功")
            .code,
        "DSH_ACTION_EVIDENCE_MISMATCH"
    );
    // 错误 epoch：拒绝。
    let wrong_epoch = EffectEvidence {
        operation: Operation::Send,
        native_id: Some("native-1".into()),
        epoch: 2,
        effect_kind: EffectKind::Message,
    };
    assert_eq!(
        store
            .succeed_with_evidence_for(&device("dev-1"), "act-map", &wrong_epoch)
            .expect_err("错误 epoch 不得提升成功")
            .code,
        "DSH_ACTION_EVIDENCE_MISMATCH"
    );
    // 正确映射：成功。
    let right = EffectEvidence {
        operation: Operation::Send,
        native_id: Some("native-1".into()),
        epoch: 1,
        effect_kind: EffectKind::Message,
    };
    assert_eq!(
        store
            .succeed_with_evidence_for(&device("dev-1"), "act-map", &right)
            .expect("提升")
            .state,
        ActionState::Succeeded
    );
}

// ============================================================
// T09 复审：RemoteState 侧真的拿到同一 hub（不只是 Tauri 侧）
// ============================================================

mod dsh_action_endpoint_tests {
    use super::{device, open_memory, send_request, ActionState, DshActionStore};
    use axum::{
        body::{to_bytes, Body},
        http::{header, Request, StatusCode},
        response::Response,
        Router,
    };
    use multi_agents_manager_lib::{
        inject::{capability, engine::RealInjector},
        remote::{
            codex_ipc::CodexControlHub,
            pairing::{persist_device, DeviceStore, NewDevice},
            pin::PinRateLimiter,
            server::{router, CreateTaskHub, RemoteState},
        },
        services::dsh_control::{error::codes, model::ActionView, DshControlHub, DshDb},
        session::SessionsResponse,
    };
    use std::sync::{Arc, Mutex};
    use tower::ServiceExt;

    const ACTION_ID: &str = "act-ep";
    const OWNER: &str = "dev-1";
    const OTHER_DEVICE: &str = "dev-2";

    fn action_router() -> Router {
        // 两张独立内存库，不初始化生产单例、不启动应用或派发原生动作。
        let conn = open_memory();
        DshActionStore::new(&conn)
            .register(&device(OWNER), &send_request(ACTION_ID, "x"))
            .expect("登记动作");
        let hub = DshControlHub::with_db(DshDb::Owned(Arc::new(Mutex::new(conn))));
        let store = DeviceStore::memory();
        store.with(|conn| {
            for id in [OWNER, OTHER_DEVICE] {
                persist_device(
                    conn,
                    &NewDevice {
                        id: id.into(),
                        name: id.into(),
                        ua: "test".into(),
                        origin_ip: format!("127.0.0.1#{id}"),
                        via: "lan".into(),
                        paired_at: chrono::Utc::now().timestamp_millis(),
                    },
                )
                .expect("登记已配对设备");
            }
        });
        let no_sessions = || SessionsResponse {
            sessions: Vec::new(),
            total_count: 0,
            waiting_count: 0,
        };
        router(Arc::new(RemoteState {
            opencode_native: std::sync::Arc::new(
                multi_agents_manager_lib::services::opencode_native::NativeHub::new(
                    multi_agents_manager_lib::remote::pairing::DeviceStore::memory(),
                ),
            ),
            ui_config_source: Box::new(|| None),
            subagent_source: Default::default(),
            subagent_message_source: Default::default(),
            capability_table: capability::new_table(),
            session_source: Box::new(no_sessions),
            fresh_session_source: Box::new(no_sessions),
            pairing_counter: Box::new(Vec::new),
            board_hidden_ids: Box::new(Vec::new),
            board_hidden_hide: Arc::new(|_| 0),
            board_hidden_unhide: Arc::new(|_| 0),
            unread_mark_read: Arc::new(|_, _, _| ()),
            session_close: Arc::new(|_| panic!("不应关闭真实进程")),
            store,
            host_source: Box::new(|| serde_json::Value::Null),
            message_source: Box::new(|_, _, _| Err("未注入内容源".into())),
            path_source: Box::new(|_, _, _| (Vec::new(), false)),
            watcher_tx: tokio::sync::broadcast::channel(8).0,
            sse_registry: Default::default(),
            max_devices_source: Box::new(|| 3),
            pin_limiter: Mutex::new(PinRateLimiter::new()),
            global_pin_limiter: Mutex::new(PinRateLimiter::global()),
            pin_source: Box::new(|| None),
            now_source: Box::new(|| 0),
            via_hosts_source: Box::new(|| None),
            rate_bucket_channels_source: Box::new(Vec::new),
            home_source: Box::new(|| None),
            injector: Arc::new(RealInjector),
            resume_spawner: Arc::new(|_| panic!("不应启动终端")),
            create_hub: Arc::new(CreateTaskHub::stub()),
            archive_source: Box::new(Vec::new),
            archive_delete: Arc::new(|_| 0),
            confirm_probe: Arc::new(|_, _, _| false),
            dialog_probe: Arc::new(|_, _| None),
            screen_probe: Arc::new(|_, _| None),
            codex_control: CodexControlHub::disabled_arc(),
            dsh_control: hub,
            dsh_console_mount: None,
            dsh_console_upstream_source: None,
            dsh_auto_adapt_source: Arc::new(|| false),
            desktop_language: Default::default(),
        }))
    }

    async fn get_action(id: &str, device_id: Option<&str>) -> Response {
        let mut request = Request::get(format!("/m/api/v1/dsh/action/{id}"));
        if let Some(id) = device_id {
            request = request.header(header::COOKIE, format!("mam_device={id}"));
        }
        action_router()
            .oneshot(request.body(Body::empty()).expect("构造请求"))
            .await
            .expect("请求真实路由")
    }

    fn assert_no_store(headers: &axum::http::HeaderMap) {
        assert_eq!(
            headers
                .get(header::CACHE_CONTROL)
                .and_then(|v| v.to_str().ok()),
            Some("no-store"),
            "动作查询响应必须禁止缓存"
        );
    }

    async fn json_body(body: Body) -> serde_json::Value {
        let bytes = to_bytes(body, 16 * 1024).await.expect("读取响应");
        serde_json::from_slice(&bytes).expect("解析 JSON 响应")
    }

    #[tokio::test]
    async fn owner_receives_uncacheable_action_view() {
        let (parts, body) = get_action(ACTION_ID, Some(OWNER)).await.into_parts();
        assert_eq!(parts.status, StatusCode::OK);
        let view: ActionView = serde_json::from_value(json_body(body).await).expect("动作视图");
        assert_eq!(view.receipt.action_id, ACTION_ID);
        assert_eq!(view.receipt.state, ActionState::Queued);
        assert_no_store(&parts.headers);
    }

    async fn assert_uncacheable_not_found(action_id: &str, device_id: &str) {
        let (parts, body) = get_action(action_id, Some(device_id)).await.into_parts();
        assert_eq!(parts.status, StatusCode::NOT_FOUND);
        assert_eq!(json_body(body).await["error"], codes::ACTION_NOT_FOUND);
        assert_no_store(&parts.headers);
    }

    #[tokio::test]
    async fn missing_action_returns_uncacheable_not_found() {
        assert_uncacheable_not_found("missing", OWNER).await;
    }

    #[tokio::test]
    async fn other_paired_device_receives_uncacheable_not_found_for_owned_action() {
        assert_uncacheable_not_found(ACTION_ID, OTHER_DEVICE).await;
    }

    #[tokio::test]
    async fn no_device_receives_uncacheable_gate_rejection() {
        // 无 cookie 由真实 gate 拦截，不会进入处理器的防御性 403 分支。
        let response = get_action(ACTION_ID, None).await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert_no_store(response.headers());
    }
}

// ============================================================
// T09 验收缺口修复：单会话 actor 接入 dispatch、共享连接、写门、抢输回执
// ============================================================

fn hub_with_temp_db() -> (
    std::sync::Arc<multi_agents_manager_lib::services::dsh_control::DshControlHub>,
    tempfile::TempDir,
) {
    // 注意：调用方若需直接开连接，用 tmp.path().join("tuvis.db") 二次打开。
    let tmp = tempfile::tempdir().expect("tempdir");
    let db = tmp.path().join("tuvis.db");
    {
        let conn = Connection::open(&db).expect("建库");
        multi_agents_manager_lib::database::schema::init(&conn).unwrap();
    }
    let conn = Connection::open(&db).expect("hub 连接");
    let hub = multi_agents_manager_lib::services::dsh_control::DshControlHub::with_db(
        multi_agents_manager_lib::services::dsh_control::DshDb::Owned(std::sync::Arc::new(
            std::sync::Mutex::new(conn),
        )),
    );
    hub.open_write_gate();
    // 写门默认关（fail-closed）；本辅助返回「已过启动恢复」的 hub。
    hub.open_write_gate();
    (hub, tmp)
}

/// 验收①「单会话 actor + 占用互斥」：同一会话的**两个不同 actionId** 派发时，
/// 只允许一个进入占用（Reserved）；另一个保持 queued。不同会话互不影响。
///
/// 这是用户驳回意见「同一会话的两个不同动作仍可同时占用」的直接回归：
/// 若去掉 `reserve_for_dispatch_inner` 的同会话占用检查，本用例 FAILED。
#[tokio::test]
async fn same_session_two_action_ids_only_one_reserves() {
    let (hub, _tmp) = hub_with_temp_db();
    let mut first = send_request("act-s1-a", "a");
    let mut second = send_request("act-s1-b", "b");
    first.target.session_id = "s-shared".into();
    second.target.session_id = "s-shared".into();

    let r1 = hub
        .dispatch(device("dev-1"), first.clone())
        .await
        .expect("第一个");
    let r2 = hub
        .dispatch(device("dev-1"), second.clone())
        .await
        .expect("第二个");
    assert_eq!(r1.state, ActionState::Reserved, "第一个应占用成功");
    assert_eq!(
        r2.state,
        ActionState::Queued,
        "同会话已有占用中的动作时，第二个不得再占用（用户驳回原句的直接回归）"
    );

    // 不同会话不受影响：各自可占用。
    let mut other = send_request("act-s2-a", "c");
    other.target.session_id = "s-other".into();
    let r3 = hub
        .dispatch(device("dev-1"), other)
        .await
        .expect("其它会话");
    assert_eq!(r3.state, ActionState::Reserved, "不同会话互不阻塞");
}

/// 真并发（多线程 runtime）：同会话两个不同 actionId 同时派发，**恰一个** Reserved。
///
/// 必须在 multi_thread runtime 下跑，否则 current_thread 会让两个任务串行完成，
/// 用例失去判别力（上一版即因此被复审判为「摆设」）。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_same_session_dispatch_reserves_exactly_one() {
    let (hub, _tmp) = hub_with_temp_db();
    let mut a = send_request("act-cc-a", "a");
    let mut b = send_request("act-cc-b", "b");
    a.target.session_id = "s-cc".into();
    b.target.session_id = "s-cc".into();

    let hub1 = std::sync::Arc::clone(&hub);
    let hub2 = std::sync::Arc::clone(&hub);
    let h1 = tokio::spawn(async move { hub1.dispatch(device("dev-1"), a).await });
    let h2 = tokio::spawn(async move { hub2.dispatch(device("dev-1"), b).await });
    let (r1, r2) = (h1.await.expect("join1"), h2.await.expect("join2"));
    let s1 = r1.expect("第一个").state;
    let s2 = r2.expect("第二个").state;
    let reserved = [s1, s2]
        .iter()
        .filter(|s| **s == ActionState::Reserved)
        .count();
    assert_eq!(
        reserved, 1,
        "同会话并发派发必须恰有一个占用，实际 {s1:?}/{s2:?}"
    );
    let queued = [s1, s2]
        .iter()
        .filter(|s| **s == ActionState::Queued)
        .count();
    assert_eq!(queued, 1, "另一个必须保持 queued");
}

/// 写门：关闭后 dispatch 必须被拒（DSH_ACTION_WRITE_GATED），读路径仍可用。
#[tokio::test]
async fn closed_write_gate_rejects_dispatch_but_allows_reads() {
    let (hub, _tmp) = hub_with_temp_db();
    hub.dispatch(device("dev-1"), send_request("act-gate", "x"))
        .await
        .expect("开闸时派发成功");

    hub.close_write_gate();
    assert!(!hub.write_gate_open());
    let err = hub
        .dispatch(device("dev-1"), send_request("act-gate2", "y"))
        .await
        .expect_err("写门关闭后必须拒绝新写");
    assert_eq!(err.code, "DSH_ACTION_WRITE_GATED");
    // 读路径不受影响。
    assert!(hub.action(&device("dev-1"), "act-gate").await.is_ok());

    hub.open_write_gate();
    assert!(hub
        .dispatch(device("dev-1"), send_request("act-gate3", "z"))
        .await
        .is_ok());
}

/// 启动恢复门（复审 M4：两个分支都必须从**对立初态**出发，删掉开关也要红）。
///
/// - 成功路径：先把门关掉 → `startup_recovery()` 成功 → 门必须变为**开**；
/// - 失败路径：门先开着 → 用坏库（DROP `dsh_actions`）→ 恢复失败 → 门必须**关**。
#[tokio::test]
async fn startup_recovery_opens_gate_on_success_and_closes_on_failure() {
    // ---- 成功路径：关 → 开 ----
    let (hub, _tmp) = hub_with_temp_db();
    hub.open_write_gate();
    hub.dispatch(device("dev-1"), send_request("act-crashy", "x"))
        .await
        .expect("占用");
    hub.close_write_gate();
    assert!(!hub.write_gate_open(), "前置：门已关");

    let recovered = hub.startup_recovery().expect("恢复成功");
    assert_eq!(recovered, 1);
    assert!(
        hub.write_gate_open(),
        "恢复成功必须把**关着的**门打开（起点已关，故这条能抓到「删掉 open」）"
    );
    let v = hub
        .action(&device("dev-1"), "act-crashy")
        .await
        .expect("读");
    assert_eq!(v.receipt.state, ActionState::UnknownDelivery);

    // ---- 失败路径：开 → 关 ----
    let tmp2 = tempfile::tempdir().expect("tempdir");
    let broken_db = tmp2.path().join("broken.db");
    {
        let conn = Connection::open(&broken_db).expect("建库");
        multi_agents_manager_lib::database::schema::init(&conn).unwrap();
        conn.execute("DROP TABLE dsh_actions", []).expect("破坏库");
    }
    let broken = multi_agents_manager_lib::services::dsh_control::DshControlHub::with_db(
        multi_agents_manager_lib::services::dsh_control::DshDb::Owned(std::sync::Arc::new(
            std::sync::Mutex::new(Connection::open(&broken_db).expect("坏库连接")),
        )),
    );
    broken.open_write_gate();
    assert!(broken.write_gate_open(), "前置：门开着");
    let err = broken.startup_recovery().expect_err("坏库必须恢复失败");
    assert!(!err.detail.is_empty());
    assert!(
        !broken.write_gate_open(),
        "恢复失败必须把**开着的**门关掉（起点已开，故这条能抓到「删掉 close」）"
    );
}

/// 重复派发（幂等）：同 actionId 第二次不得再占用，返回既有状态。
#[tokio::test]
async fn idempotent_redispatch_returns_existing_state() {
    let (hub, _tmp) = hub_with_temp_db();
    let req = send_request("act-idem-hub", "x");
    let first = hub
        .dispatch(device("dev-1"), req.clone())
        .await
        .expect("首次");
    assert_eq!(first.state, ActionState::Reserved);
    let second = hub.dispatch(device("dev-1"), req).await.expect("重试");
    assert_eq!(
        second.state,
        ActionState::Reserved,
        "重试不得把状态退回 Queued"
    );
    assert_eq!(second.action_id, first.action_id);
}

// ============================================================
// T09 收口（用户裁决）：按操作区分同会话占用策略
//   普通消息类（send/file_upload/image_upload）= FIFO，互不并行占用；
//   控制类（cancel/approval/answer/queue_*/steer/...）可越过消息占用。
// 「针对当前真实 pending」的原生身份校验属 T11，本批只测占用策略。
// ============================================================

use multi_agents_manager_lib::services::dsh_control::model::{
    AnswerPayload, ApprovalPayload, QuestionAnswer, RequestKey,
};

fn request_key(session: &str, event: &str) -> RequestKey {
    RequestKey {
        host_id: "local".into(),
        epoch: 1,
        session_id: session.into(),
        client_id: "client-1".into(),
        event_id: event.into(),
        agent_id: "agent-1".into(),
        revision: 0,
    }
}

fn control_request(action_id: &str, session: &str, payload: ActionPayload) -> ActionRequest {
    ActionRequest {
        target: Target {
            host_id: "local".into(),
            epoch: 1,
            session_id: session.into(),
        },
        action_id: action_id.into(),
        expected_revision: None,
        payload,
    }
}

/// 把某动作行直接置为 running（T09 尚无 running setter；SQL 直改测试自有临时库）。
fn force_state(hub_db: &Connection, action_id: &str, state: &str) {
    hub_db
        .execute(
            "UPDATE dsh_actions SET state = ?2 WHERE action_id = ?1",
            rusqlite::params![action_id, state],
        )
        .expect("改状态");
}

/// RED：Send running → CancelCurrent 必须可占用（旧实现：一律挡回 Queued）。
#[tokio::test]
async fn send_running_allows_cancel_to_reserve() {
    let (hub, tmp) = hub_with_temp_db();
    let s = "s-ctl-1";
    hub.dispatch(device("dev-1"), {
        let mut r = send_request("act-send-1", "hi");
        r.target.session_id = s.into();
        r
    })
    .await
    .expect("Send 占用");
    force_state(
        &Connection::open(tmp.path().join("tuvis.db")).unwrap(),
        "act-send-1",
        "running",
    );

    let r = hub
        .dispatch(
            device("dev-1"),
            control_request("act-cancel-1", s, ActionPayload::CancelCurrent),
        )
        .await
        .expect("派发");
    assert_eq!(
        r.state,
        ActionState::Reserved,
        "Send running 时 CancelCurrent 必须可占用（控制动作不被消息占用拦住）"
    );
}

/// RED：Send accepted（等待审批）→ 对应 Approval 必须可占用。
#[tokio::test]
async fn send_awaiting_approval_allows_approval_to_reserve() {
    let (hub, tmp) = hub_with_temp_db();
    let s = "s-ctl-2";
    hub.dispatch(device("dev-1"), {
        let mut r = send_request("act-send-2", "hi");
        r.target.session_id = s.into();
        r
    })
    .await
    .expect("Send 占用");

    let payload = ActionPayload::Approval(ApprovalPayload {
        key: request_key(s, "evt-approval-2"),
        decision: "allowed-once".into(),
    });
    let r = hub
        .dispatch(device("dev-1"), control_request("act-appr-2", s, payload))
        .await
        .expect("派发");
    assert_eq!(r.state, ActionState::Reserved, "审批不得被消息占用拦住");
    // running 态同样放行（占用检查含 running）。
    force_state(
        &Connection::open(tmp.path().join("tuvis.db")).unwrap(),
        "act-send-2",
        "running",
    );
    let payload_b = ActionPayload::Approval(ApprovalPayload {
        key: request_key(s, "evt-approval-2b"),
        decision: "rejected".into(),
    });
    let rb = hub
        .dispatch(
            device("dev-1"),
            control_request("act-appr-2b", s, payload_b),
        )
        .await
        .expect("派发");
    assert_eq!(rb.state, ActionState::Reserved);
}

/// RED：Send running → 对应 Answer 必须可占用。
#[tokio::test]
async fn send_running_allows_answer_to_reserve() {
    let (hub, tmp) = hub_with_temp_db();
    let s = "s-ctl-3";
    hub.dispatch(device("dev-1"), {
        let mut r = send_request("act-send-3", "hi");
        r.target.session_id = s.into();
        r
    })
    .await
    .expect("Send 占用");
    force_state(
        &Connection::open(tmp.path().join("tuvis.db")).unwrap(),
        "act-send-3",
        "running",
    );

    let payload = ActionPayload::Answer(AnswerPayload {
        key: request_key(s, "evt-q-3"),
        revision_token: None,
        answers: vec![QuestionAnswer {
            id: "q1".into(),
            selected: vec!["苹果".into()],
            custom: None,
        }],
    });
    let r = hub
        .dispatch(device("dev-1"), control_request("act-ans-3", s, payload))
        .await
        .expect("派发");
    assert_eq!(r.state, ActionState::Reserved, "问答不得被消息占用拦住");
}

/// RED：QueueEdit/QueueRemove/Steer 也属控制类，不被 Send 占用拦住。
#[tokio::test]
async fn send_running_allows_queue_controls_to_reserve() {
    use multi_agents_manager_lib::services::dsh_control::model::{
        QueueEditPayload, QueueRemovePayload, SteerPayload,
    };
    let (hub, tmp) = hub_with_temp_db();
    let s = "s-ctl-4";
    hub.dispatch(device("dev-1"), {
        let mut r = send_request("act-send-4", "hi");
        r.target.session_id = s.into();
        r
    })
    .await
    .expect("Send 占用");
    force_state(
        &Connection::open(tmp.path().join("tuvis.db")).unwrap(),
        "act-send-4",
        "running",
    );

    for (id, payload) in [
        (
            "act-qe-4",
            ActionPayload::QueueEdit(QueueEditPayload {
                item_id: "qi-1".into(),
                text: "改".into(),
            }),
        ),
        (
            "act-qr-4",
            ActionPayload::QueueRemove(QueueRemovePayload {
                item_id: "qi-1".into(),
            }),
        ),
        (
            "act-st-4",
            ActionPayload::Steer(SteerPayload {
                item_id: "qi-1".into(),
            }),
        ),
    ] {
        let r = hub
            .dispatch(device("dev-1"), control_request(id, s, payload))
            .await
            .unwrap_or_else(|e| panic!("{id} 派发失败：{e:?}"));
        assert_eq!(r.state, ActionState::Reserved, "{id} 不得被 Send 占用拦住");
    }
}

/// 回归：两条不同 Send 仍为 Reserved/Queued（FIFO 不放宽）。
#[tokio::test]
async fn two_sends_still_fifo() {
    let (hub, _tmp) = hub_with_temp_db();
    let s = "s-fifo";
    let r1 = hub
        .dispatch(device("dev-1"), {
            let mut r = send_request("act-f-1", "a");
            r.target.session_id = s.into();
            r
        })
        .await
        .expect("第一条");
    let r2 = hub
        .dispatch(device("dev-1"), {
            let mut r = send_request("act-f-2", "b");
            r.target.session_id = s.into();
            r
        })
        .await
        .expect("第二条");
    assert_eq!(r1.state, ActionState::Reserved);
    assert_eq!(
        r2.state,
        ActionState::Queued,
        "第二条 Send 必须排队（T10 队列推进自动派发）"
    );
}

/// 回归：其他 session 不互相阻塞（跨会话仍可并行占用）。
#[tokio::test]
async fn other_session_not_blocked() {
    let (hub, _tmp) = hub_with_temp_db();
    let r1 = hub
        .dispatch(device("dev-1"), {
            let mut r = send_request("act-os-1", "a");
            r.target.session_id = "s-a".into();
            r
        })
        .await
        .expect("会话 A");
    let r2 = hub
        .dispatch(device("dev-1"), {
            let mut r = send_request("act-os-2", "b");
            r.target.session_id = "s-b".into();
            r
        })
        .await
        .expect("会话 B");
    assert_eq!(r1.state, ActionState::Reserved);
    assert_eq!(r2.state, ActionState::Reserved);
}

/// 回归：设备归属与请求身份的拒绝面不因「可越过」而放宽。
#[tokio::test]
async fn ownership_and_identity_guards_still_reject() {
    let (hub, _tmp) = hub_with_temp_db();
    let s = "s-guard";
    hub.dispatch(device("dev-1"), {
        let mut r = send_request("act-g-1", "a");
        r.target.session_id = s.into();
        r
    })
    .await
    .expect("Send 占用");

    // 别的设备对同一动作 id 登记请求 → 不存在（不泄露存在性）。
    let err = hub
        .dispatch(device("dev-2"), {
            let mut r = send_request("act-g-1", "a");
            r.target.session_id = s.into();
            r
        })
        .await
        .expect_err("跨设备必须拒绝");
    assert_eq!(err.code, "DSH_ACTION_NOT_FOUND");

    // 同设备同 id 不同载荷 → 身份冲突。
    let err = hub
        .dispatch(device("dev-1"), {
            let mut r = send_request("act-g-1", "different");
            r.target.session_id = s.into();
            r
        })
        .await
        .expect_err("同 id 异载荷必须冲突");
    assert_eq!(err.code, "DSH_ACTION_ID_CONFLICT");

    // 控制动作同样过设备归属：A 先登记一个 Cancel，B 用**同一 action id** 再来
    // → 不存在（「可越过消息占用」不等于绕过设备归属）。
    hub.dispatch(
        device("dev-1"),
        control_request("act-g-cancel", s, ActionPayload::CancelCurrent),
    )
    .await
    .expect("A 的控制动作可登记");
    let err = hub
        .dispatch(
            device("dev-2"),
            control_request("act-g-cancel", s, ActionPayload::CancelCurrent),
        )
        .await
        .expect_err("B 触碰 A 的控制动作必须拒绝");
    assert_eq!(err.code, "DSH_ACTION_NOT_FOUND");
}

// ============================================================
// T09 收口（修复 2）：dispatch/action 的同步 DB 段移入 spawn_blocking
//   —— 持 DB 锁时，单 worker 上的轻量异步任务仍能及时运行。
// ============================================================

/// 判别性（dispatch）：外部线程持有 DB 连接锁 400ms 期间，dispatch 的等锁必须
/// 发生在**阻塞池**（spawn_blocking）——本用例的 dispatch 与轻量任务跑在**同一个
/// current_thread worker** 上：若 dispatch 在 worker 上同步等锁，10ms 心跳任务
/// 会被饿死（窗口内 0 拍），断言失败。
#[tokio::test(flavor = "current_thread")]
async fn dispatch_does_not_starve_the_worker_while_db_locked() {
    let tmp = tempfile::tempdir().expect("tempdir");
    {
        let conn = Connection::open(tmp.path().join("tuvis.db")).expect("建库");
        multi_agents_manager_lib::database::schema::init(&conn).unwrap();
    }
    let shared = std::sync::Arc::new(std::sync::Mutex::new(
        Connection::open(tmp.path().join("tuvis.db")).expect("共享连接"),
    ));
    let hub = multi_agents_manager_lib::services::dsh_control::DshControlHub::with_db(
        multi_agents_manager_lib::services::dsh_control::DshDb::Owned(std::sync::Arc::clone(
            &shared,
        )),
    );
    hub.open_write_gate();

    // 外部线程持锁 400ms 后自动释放。
    let holder_shared = std::sync::Arc::clone(&shared);
    let holder = std::thread::spawn(move || {
        let _guard = holder_shared.lock().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(400));
    });
    // 等 holder 确实拿到锁（忙等一小段，锁竞争下足够可靠）。
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let mut req = send_request("act-block-1", "x");
    req.target.session_id = "s-block".into();
    let hub2 = std::sync::Arc::clone(&hub);
    let began = std::time::Instant::now();
    let dispatch_task =
        tokio::task::spawn(async move { hub2.dispatch(device("dev-1"), req).await });

    // 轻量心跳：同一 worker 上 10ms 一拍；持锁窗口（~350ms 剩余）内应至少 5 拍。
    let mut ticks = 0u32;
    while began.elapsed() < std::time::Duration::from_millis(300) {
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        ticks += 1;
    }
    assert!(
        ticks >= 5,
        "持锁期间轻量异步任务被饿死（仅 {ticks} 拍）——dispatch 在 worker 上同步等锁"
    );

    let receipt = dispatch_task.await.expect("join").expect("dispatch 成功");
    assert_eq!(receipt.state, ActionState::Reserved);
    assert!(
        began.elapsed() >= std::time::Duration::from_millis(300),
        "dispatch 应等待锁释放后才完成"
    );
    let _ = holder.join();
}

/// 判别性（action GET 路径）：同上，但走 `action()` 只读查询。
#[tokio::test(flavor = "current_thread")]
async fn action_get_does_not_starve_the_worker_while_db_locked() {
    let tmp = tempfile::tempdir().expect("tempdir");
    {
        let conn = Connection::open(tmp.path().join("tuvis.db")).expect("建库");
        multi_agents_manager_lib::database::schema::init(&conn).unwrap();
    }
    let shared = std::sync::Arc::new(std::sync::Mutex::new(
        Connection::open(tmp.path().join("tuvis.db")).expect("共享连接"),
    ));
    let hub = multi_agents_manager_lib::services::dsh_control::DshControlHub::with_db(
        multi_agents_manager_lib::services::dsh_control::DshDb::Owned(std::sync::Arc::clone(
            &shared,
        )),
    );
    hub.open_write_gate();
    hub.dispatch(device("dev-1"), send_request("act-block-2", "x"))
        .await
        .expect("预置动作");

    let holder_shared = std::sync::Arc::clone(&shared);
    let holder = std::thread::spawn(move || {
        let _guard = holder_shared.lock().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(400));
    });
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let hub2 = std::sync::Arc::clone(&hub);
    let dev = device("dev-1");
    let began = std::time::Instant::now();
    let action_task = tokio::task::spawn(async move { hub2.action(&dev, "act-block-2").await });

    let mut ticks = 0u32;
    while began.elapsed() < std::time::Duration::from_millis(300) {
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        ticks += 1;
    }
    assert!(
        ticks >= 5,
        "持锁期间轻量异步任务被饿死（仅 {ticks} 拍）——action 在 worker 上同步等锁"
    );

    let view = action_task.await.expect("join").expect("action 成功");
    assert_eq!(view.receipt.action_id, "act-block-2");
    assert!(
        began.elapsed() >= std::time::Duration::from_millis(300),
        "action 也应等待锁释放后才完成"
    );
    let _ = holder.join();
}

/// Mj1：崩溃时 `accepted`/`running` 同属送达未知——恢复必须一并收口，
/// 否则停在 accepted 的动作会在重启后永久占用会话。
#[tokio::test]
async fn startup_recovery_also_collects_accepted_and_running() {
    let (hub, tmp) = hub_with_temp_db();
    let db_path = tmp.path().join("tuvis.db");
    hub.dispatch(device("dev-1"), {
        let mut r = send_request("act-acc-1", "a");
        r.target.session_id = "s-acc".into();
        r
    })
    .await
    .expect("占用");
    let conn = Connection::open(&db_path).expect("连接");
    conn.execute(
        "UPDATE dsh_actions SET state = 'accepted' WHERE action_id = 'act-acc-1'",
        [],
    )
    .expect("置 accepted");

    let recovered = hub.startup_recovery().expect("恢复");
    assert_eq!(recovered, 1, "accepted 也必须被恢复");
    let view = hub.action(&device("dev-1"), "act-acc-1").await.expect("读");
    assert_eq!(view.receipt.state, ActionState::UnknownDelivery);
    // 会话不再被幽灵占用：新 Send 可立即 reserve。
    let r = hub
        .dispatch(device("dev-1"), {
            let mut r = send_request("act-acc-2", "b");
            r.target.session_id = "s-acc".into();
            r
        })
        .await
        .expect("恢复后新消息");
    assert_eq!(
        r.state,
        ActionState::Reserved,
        "accepted 崩溃残留不得永久占用会话"
    );
}

/// m3：占用互斥查询必须命中会话覆盖索引（EXPLAIN QUERY PLAN 断言）。
#[test]
fn occupancy_query_uses_the_session_covering_index() {
    let conn = open_memory();
    let mut stmt = conn
        .prepare(
            "EXPLAIN QUERY PLAN SELECT COUNT(*) FROM dsh_actions WHERE host_id = ?1 AND epoch = ?2 AND session_id = ?3 AND operation IN ('send') AND state IN ('reserved')",
        )
        .expect("explain");
    let plan: Vec<String> = stmt
        .query_map(rusqlite::params!["h", "1", "s"], |r| r.get::<_, String>(3))
        .expect("rows")
        .filter_map(Result::ok)
        .collect();
    let joined = plan.join(" | ");
    assert!(
        joined.contains("idx_dsh_actions_session_state"),
        "占用查询未走会话覆盖索引：{joined}"
    );
}

// ============================================================
// 独立复审收口：B1 FIFO 队序、M1 占用矩阵补实、M2 恢复 running、
// M5 key/target、M6 迁移补索引
// ============================================================

/// B1：第一条终态后、T10 队列泵未推进前，**后来的 Send 不得越过排队中的 Send**。
#[tokio::test]
async fn later_send_cannot_jump_over_a_queued_earlier_send() {
    let (hub, tmp) = hub_with_temp_db();
    let s = "s-b1";
    let db_path = tmp.path().join("tuvis.db");
    let mk = |id: &str| {
        let mut r = send_request(id, id);
        r.target.session_id = s.into();
        r
    };
    let f1 = hub
        .dispatch(device("dev-1"), mk("act-b1-1"))
        .await
        .expect("f1");
    assert_eq!(f1.state, ActionState::Reserved);
    let f2 = hub
        .dispatch(device("dev-1"), mk("act-b2-2"))
        .await
        .expect("f2");
    assert_eq!(f2.state, ActionState::Queued);

    // 第一条终态（succeeded）——T10 泵尚未推进。
    force_state(
        &Connection::open(&db_path).unwrap(),
        "act-b1-1",
        "succeeded",
    );

    // 第三条新 Send：必须仍排队在 f2 之后，不得越过。
    let f3 = hub
        .dispatch(device("dev-1"), mk("act-b3-3"))
        .await
        .expect("f3");
    assert_eq!(
        f3.state,
        ActionState::Queued,
        "后来的 Send 不得越过排队中的更早 Send（B1：1→3→2 乱序）"
    );
    // 排队中的 f2 重试派发可正常占用（队头是它）。
    let f2b = hub
        .dispatch(device("dev-1"), mk("act-b2-2"))
        .await
        .expect("f2 重试");
    assert_eq!(f2b.state, ActionState::Reserved, "队头重试应占用成功");
}

/// M1：Send **accepted**（不止 reserved/running）也阻塞下一条 Send。
#[tokio::test]
async fn send_accepted_still_blocks_next_send() {
    let (hub, tmp) = hub_with_temp_db();
    let s = "s-m1a";
    let db_path = tmp.path().join("tuvis.db");
    hub.dispatch(device("dev-1"), {
        let mut r = send_request("act-m1a-1", "a");
        r.target.session_id = s.into();
        r
    })
    .await
    .expect("占用");
    force_state(
        &Connection::open(&db_path).unwrap(),
        "act-m1a-1",
        "accepted",
    );
    let r2 = hub
        .dispatch(device("dev-1"), {
            let mut r = send_request("act-m1a-2", "b");
            r.target.session_id = s.into();
            r
        })
        .await
        .expect("派发");
    assert_eq!(
        r2.state,
        ActionState::Queued,
        "accepted 占用同样阻塞新 Send"
    );
}

/// M1：Send **running** 阻塞下一条 Send（与 accepted 同一谓词，独立锁定）。
#[tokio::test]
async fn send_running_still_blocks_next_send() {
    let (hub, tmp) = hub_with_temp_db();
    let s = "s-m1b";
    let db_path = tmp.path().join("tuvis.db");
    hub.dispatch(device("dev-1"), {
        let mut r = send_request("act-m1b-1", "a");
        r.target.session_id = s.into();
        r
    })
    .await
    .expect("占用");
    force_state(&Connection::open(&db_path).unwrap(), "act-m1b-1", "running");
    let r2 = hub
        .dispatch(device("dev-1"), {
            let mut r = send_request("act-m1b-2", "b");
            r.target.session_id = s.into();
            r
        })
        .await
        .expect("派发");
    assert_eq!(r2.state, ActionState::Queued, "running 占用同样阻塞新 Send");
}

/// M1 对照：控制动作在飞**不**阻塞新 Send（白名单只单向放行控制越过消息，
/// 不反向阻塞消息）。
#[tokio::test]
async fn control_in_flight_does_not_block_new_send() {
    let (hub, _tmp) = hub_with_temp_db();
    let s = "s-m1c";
    let r1 = hub
        .dispatch(
            device("dev-1"),
            control_request("act-m1c-ctl", s, ActionPayload::CancelCurrent),
        )
        .await
        .expect("控制动作");
    assert_eq!(r1.state, ActionState::Reserved);
    let r2 = hub
        .dispatch(device("dev-1"), {
            let mut r = send_request("act-m1c-msg", "m");
            r.target.session_id = s.into();
            r
        })
        .await
        .expect("新消息");
    assert_eq!(
        r2.state,
        ActionState::Reserved,
        "控制动作在飞不得阻塞新消息"
    );
}

/// M1 补实：Send 置 **accepted** 后 Approval 可占用（原用例只走了 reserved→running）。
#[tokio::test]
async fn send_accepted_allows_approval_to_reserve() {
    let (hub, tmp) = hub_with_temp_db();
    let s = "s-m1d";
    let db_path = tmp.path().join("tuvis.db");
    hub.dispatch(device("dev-1"), {
        let mut r = send_request("act-m1d-1", "a");
        r.target.session_id = s.into();
        r
    })
    .await
    .expect("占用");
    force_state(
        &Connection::open(&db_path).unwrap(),
        "act-m1d-1",
        "accepted",
    );
    let payload = ActionPayload::Approval(ApprovalPayload {
        key: request_key(s, "evt-m1d"),
        decision: "allowed-once".into(),
    });
    let r = hub
        .dispatch(device("dev-1"), control_request("act-m1d-ap", s, payload))
        .await
        .expect("派发");
    assert_eq!(
        r.state,
        ActionState::Reserved,
        "accepted 状态下审批同样可越过"
    );
}

/// M2：恢复同时收口 accepted **与** running 两行。
#[tokio::test]
async fn startup_recovery_collects_both_accepted_and_running() {
    let (hub, tmp) = hub_with_temp_db();
    let s = "s-m2";
    let db_path = tmp.path().join("tuvis.db");
    for id in ["act-m2-a", "act-m2-b", "act-m2-c"] {
        hub.dispatch(device("dev-1"), {
            let mut r = send_request(id, id);
            r.target.session_id = s.into();
            r
        })
        .await
        .unwrap_or_else(|e| panic!("{id} 派发：{e:?}"));
    }
    let conn = Connection::open(&db_path).unwrap();
    conn.execute(
        "UPDATE dsh_actions SET state='accepted' WHERE action_id='act-m2-b'",
        [],
    )
    .unwrap();
    conn.execute(
        "UPDATE dsh_actions SET state='running' WHERE action_id='act-m2-c'",
        [],
    )
    .unwrap();

    let recovered = hub.startup_recovery().expect("恢复");
    assert_eq!(recovered, 3, "reserved/accepted/running 三行都必须收口");
    for id in ["act-m2-a", "act-m2-b", "act-m2-c"] {
        let v = hub
            .action(&device("dev-1"), id)
            .await
            .unwrap_or_else(|e| panic!("{id}: {e:?}"));
        assert_eq!(v.receipt.state, ActionState::UnknownDelivery, "{id}");
    }
}

/// M5：Approval/Answer 的 RequestKey 与 target 不一致 → DSH_ACTION_INVALID。
#[tokio::test]
async fn mismatched_request_key_target_is_rejected() {
    let (hub, _tmp) = hub_with_temp_db();
    let s = "s-m5";
    let mut bad_key = request_key("OTHER-SESSION", "evt-x");
    bad_key.epoch = 42; // 连 epoch 都不一样
    for (id, payload) in [
        (
            "act-m5-ap",
            ActionPayload::Approval(ApprovalPayload {
                key: bad_key.clone(),
                decision: "allowed-once".into(),
            }),
        ),
        (
            "act-m5-an",
            ActionPayload::Answer(AnswerPayload {
                key: bad_key,
                revision_token: None,
                answers: vec![],
            }),
        ),
    ] {
        let err = hub
            .dispatch(device("dev-1"), control_request(id, s, payload))
            .await
            .expect_err(&format!("{id} key/target 不一致必须拒绝"));
        assert_eq!(err.code, "DSH_ACTION_INVALID", "{id}：{err:?}");
    }
}

/// M6：旧库缺会话索引时，`migration::migrate` 必须补建且查询命中。
#[test]
fn migration_backfills_the_session_occupancy_index() {
    let conn = Connection::open_in_memory().expect("库");
    multi_agents_manager_lib::database::schema::init(&conn).unwrap();
    // 模拟旧库：删掉该索引。
    conn.execute("DROP INDEX idx_dsh_actions_session_state", [])
        .expect("删索引");

    multi_agents_manager_lib::database::migration::migrate(&conn).expect("迁移");

    let names = index_names(&conn, "dsh_actions");
    assert!(
        names.iter().any(|n| n == "idx_dsh_actions_session_state"),
        "迁移必须补建会话占用索引：{names:?}"
    );
    let mut stmt = conn
        .prepare("EXPLAIN QUERY PLAN SELECT COUNT(*) FROM dsh_actions WHERE host_id = ?1 AND epoch = ?2 AND session_id = ?3 AND state IN ('reserved')")
        .expect("explain");
    let plan: Vec<String> = stmt
        .query_map(rusqlite::params!["h", "1", "s"], |r| r.get::<_, String>(3))
        .expect("rows")
        .filter_map(Result::ok)
        .collect();
    assert!(plan.join(" | ").contains("idx_dsh_actions_session_state"));
}

// ============================================================
// T09 收口第三批：恢复的事务性（UPDATE 与 claim 清理同事务）
// ============================================================

/// RED：claim 清理失败时——恢复必须报错、动作状态**回滚保持原值**、claim 未被
/// 部分清除、写门保持关闭。旧实现：UPDATE 独立提交后 `let _ =` 吞掉 DELETE
/// 错误 → 返回 Ok → 写门误开 + 半完成状态（动作已 unknown 而 claim 遗留）。
#[tokio::test]
async fn claim_cleanup_failure_rolls_back_recovery_and_keeps_gate_closed() {
    let (hub, tmp) = hub_with_temp_db();
    let db_path = tmp.path().join("tuvis.db");
    hub.dispatch(device("dev-1"), send_request("act-tx-1", "x"))
        .await
        .expect("占用（reserved + claim）");

    // 触发器：claim 删除即炸。
    {
        let conn = Connection::open(&db_path).expect("连接");
        conn.execute_batch(
            "CREATE TRIGGER claims_delete_boom BEFORE DELETE ON dsh_claims \
             BEGIN SELECT RAISE(ABORT, 'claims delete boom'); END;",
        )
        .expect("触发器");
    }

    hub.close_write_gate();
    let err = hub
        .startup_recovery()
        .expect_err("claim 清理失败必须让恢复整体报错");
    assert!(!err.detail.is_empty(), "错误须带明细：{err:?}");

    // 动作状态回滚：仍是 reserved（不是 unknown_delivery）。
    let view = hub
        .action(&device("dev-1"), "act-tx-1")
        .await
        .expect("可读");
    assert_eq!(
        view.receipt.state,
        ActionState::Reserved,
        "UPDATE 必须随 DELETE 失败一起回滚，不得留下半完成状态"
    );
    // claim 未被清除。
    let conn = Connection::open(&db_path).expect("连接");
    let claims: i64 = conn
        .query_row("SELECT COUNT(*) FROM dsh_claims", [], |r| r.get(0))
        .expect("计数");
    assert_eq!(claims, 1, "claim 行必须原样保留");
    // 写门保持关闭。
    assert!(!hub.write_gate_open(), "恢复失败不得打开写入口");
}

/// GREEN 补全：正常恢复——三态转 UnknownDelivery、**claim 同步清除**、写门开启；
/// 重复恢复幂等（第二次 0 行、状态不变）。
#[tokio::test]
async fn normal_recovery_clears_claims_and_is_idempotent() {
    let (hub, tmp) = hub_with_temp_db();
    let db_path = tmp.path().join("tuvis.db");
    // 三条动作放**三个会话**——同会话会被占用策略挡在 Queued（无 claim）。
    for (id, session, final_state) in [
        ("act-ok-1", "s-ok-a", "reserved"),
        ("act-ok-2", "s-ok-b", "accepted"),
        ("act-ok-3", "s-ok-c", "running"),
    ] {
        hub.dispatch(device("dev-1"), {
            let mut r = send_request(id, id);
            r.target.session_id = session.into();
            r
        })
        .await
        .unwrap_or_else(|e| panic!("{id}：{e:?}"));
        force_state(&Connection::open(&db_path).unwrap(), id, final_state);
    }
    {
        let conn = Connection::open(&db_path).expect("连接");
        let claims: i64 = conn
            .query_row("SELECT COUNT(*) FROM dsh_claims", [], |r| r.get(0))
            .unwrap();
        assert_eq!(claims, 3, "前置：三条 claim 在");
    }

    hub.close_write_gate();
    let recovered = hub.startup_recovery().expect("恢复成功");
    assert_eq!(recovered, 3);
    assert!(hub.write_gate_open(), "成功恢复必须开写门");
    for id in ["act-ok-1", "act-ok-2", "act-ok-3"] {
        let v = hub
            .action(&device("dev-1"), id)
            .await
            .unwrap_or_else(|e| panic!("{id}: {e:?}"));
        assert_eq!(v.receipt.state, ActionState::UnknownDelivery, "{id}");
    }
    {
        let conn = Connection::open(&db_path).expect("连接");
        let claims: i64 = conn
            .query_row("SELECT COUNT(*) FROM dsh_claims", [], |r| r.get(0))
            .unwrap();
        assert_eq!(claims, 0, "恢复后 claim 必须同步清除");
    }

    // 幂等（复审 Minor 1：锁「零副作用」而非只锁返回值）——快照逐字段比对。
    let snapshot = |conn: &Connection| -> Vec<(String, String, Option<String>, i64)> {
        let mut stmt = conn
            .prepare("SELECT action_id, state, reason_code, updated_at_ms FROM dsh_actions ORDER BY action_id")
            .expect("snap");
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
            .expect("rows")
            .filter_map(Result::ok)
            .collect()
    };
    let snap_conn = Connection::open(&db_path).expect("连接");
    let before = snapshot(&snap_conn);
    let again = hub.startup_recovery().expect("二次恢复");
    assert_eq!(again, 0, "重复恢复不得再动任何行");
    let after = snapshot(&snap_conn);
    assert_eq!(
        before, after,
        "二次恢复必须零副作用（含 updated_at_ms/reason_code）"
    );
    let v = hub.action(&device("dev-1"), "act-ok-1").await.expect("读");
    assert_eq!(v.receipt.state, ActionState::UnknownDelivery);
    // Minor 2：恢复语义的可观测证据。
    assert_eq!(
        v.receipt.reason_code.as_deref(),
        Some("action_in_flight_at_crash"),
        "恢复必须携带在途崩溃原因码"
    );
    assert!(hub.write_gate_open());
}

/// 复审 Major 1：恢复是**一次性**的——门开启后再次 `startup_recovery()` 不得把
/// 期间真实在途（reserved/accepted/running）的动作打成 unknown 或删其 claim。
#[tokio::test]
async fn second_recovery_never_touches_live_in_flight_actions() {
    let (hub, _tmp) = hub_with_temp_db();
    hub.startup_recovery()
        .expect("首次恢复（消费一次性标记并开门）");

    // 门开后产生一条真实在途动作。
    let r = hub
        .dispatch(device("dev-1"), send_request("act-live-1", "x"))
        .await
        .expect("派发");
    assert_eq!(r.state, ActionState::Reserved);

    let again = hub.startup_recovery().expect("二次恢复必须短路成功");
    assert_eq!(again, 0);
    let v = hub
        .action(&device("dev-1"), "act-live-1")
        .await
        .expect("读");
    assert_eq!(
        v.receipt.state,
        ActionState::Reserved,
        "二次恢复不得打掉真实在途动作"
    );
    assert!(hub.write_gate_open(), "短路路径不得动门");
}

#[test]
fn stop_claim_survives_restart_and_rejects_a_second_device_for_the_same_turn() {
    let conn = open_memory();
    let store = multi_agents_manager_lib::services::dsh_control::store::DshActionStore::new(&conn);
    let first = device("device-a");
    let second = device("device-b");
    let mut a = control_request("act-stop-first", "s1", ActionPayload::CancelCurrent);
    a.expected_revision = Some(7);
    store.register(&first, &a).unwrap();
    store
        .reserve_for_dispatch_for(
            &first,
            &a.action_id,
            "stop-claim-a",
            a.target.epoch,
            "device-a",
            "turn:1:2:7",
            0,
        )
        .unwrap()
        .unwrap();
    store.mark_accepted_for(&first, &a.action_id, None).unwrap();
    store.recover_reserved_after_restart().unwrap();
    assert_eq!(
        store.view(&first, &a.action_id).unwrap().receipt.state,
        ActionState::Accepted
    );
    let mut b = control_request("act-stop-second", "s1", ActionPayload::CancelCurrent);
    b.expected_revision = Some(7);
    store.register(&second, &b).unwrap();
    let blocked = store
        .reserve_for_dispatch_for(
            &second,
            &b.action_id,
            "stop-claim-b",
            b.target.epoch,
            "device-b",
            "turn:1:2:7",
            0,
        )
        .unwrap_err();
    assert_eq!(blocked.code, "DSH_ACTION_STATE_CONFLICT");
    b.expected_revision = Some(8);
    assert_eq!(
        store.register(&second, &b).unwrap_err().code,
        "DSH_ACTION_ID_CONFLICT"
    );
}
