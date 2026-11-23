//! 题卡展示冻结 overlay 聚合 RED（合同 `.git/tuvis-question-skip-work/
//! coordinator-wiring-contract.md` §8 + §9 首批聚合 ⑦「投影」+ 任务书行为 ①–⑥）。
//!
//! RED 纪律（与 coordinator_tests 同脉）：本文件必须**编译通过并在 stub 上运行
//! 失败**——编译失败不是 RED；stub（display.rs）只提供 API 形状并保守拒绝
//! （try_frozen 恒 None / 资格核验恒失配 / overlay 恒无戳 / mutation 包装恒
//! `Unimplemented` 且不执行闭包），正向断言在 stub 上必败，负向断言钉死 GREEN
//! 不得退化的保守面。
//!
//! 零真实副作用纪律：
//! * 存储一律 `DeviceStore::memory()` / 自建内存连接 + `schema::init` +
//!   `migration::migrate`（DAO27 真两表），绝不写 `~/.tuvis`；
//! * FrozenSource 一律经真实链产出：`QuestionSkipLive` refresh 票 + 真实
//!   `OwnerRegistry` live proof + 真实 `build_source_snapshot`（canonical raw 引用
//!   链）——**不从 filtered asyncQuestions / 客户端计算** source 或 generation；
//! * ThreadProjection 一律 serde 反序列化构造（新 optional 字段不破坏夹具）；
//! * 无 COM / 无桌面 / 无进程杀 / 无真实网络；Native 面完全不触（display-only）。
//!
//! 聚合行为 → 测试映射（任务书 ①–⑦）：
//! * ① T1（已知无行 + 完整单题 raw → active/0 current + known-empty history、
//        签同观察戳）；①负 T2（unloaded → 两账全省略、不假填 []、不签戳）。
//! * ② T3（投影尾窗 truncated=true 不撤 descriptor）；T4（raw 拒 → 只省 current
//        账、history 仍读；持连接锁可证 overlay 不触 DB）。
//! * ③ T5（同 source 新 generation active/0 与旧 generation skipped 共存；
//        current 每 source group 恰一组、题数来自 source builder）。
//! * ④ T6（prepare 快照 + restore 后迟到 finish 不重藏、latest skip native 不被
//        restore 遮）；T7（旧行 NULL 快照保持 null、绝不借新 generation 题）。
//! * ⑤⑥ T8（DB 写经 §8 包装 → 旧 stamp 失配且闭包绝不执行、epoch checked 前进、
//          新观察重新取得资格、旧冻结票跨 restore 不能安装/取得资格）；
//          T9（包装覆盖 prepare/claim/finish/settle/recover；失败取消 → Unknown，
//          恢复后原值结算可重装）。
//! * ⑦ 同观察资格语义在 T8/T9 内（stamp 与冻结 epoch 同源）。
//!
//! 诚实申报（MANIFEST/COVERAGE 同文）：HTTP/SSE 双订阅实际集成、adapter mirrors
//! 接线、publish_fresh_projection 的 CAS 预留拼装归批次5/6/8；「同短缓存锁内执行
//! reserve 闭包」为 GREEN 实现边界（无外部可观察缝），由独立 review 复核，本批
//! 只钉「stale stamp 闭包零执行」这一外部行为。

#![allow(dead_code)]

use std::sync::{Arc, Mutex};

use rusqlite::{params, Connection};
use serde_json::{json, Value};

use crate::database::dao::codex_question_display::{
    claim_cas, finish, prepare_skip, recover, restore, settle_without_invoke, ActionReceipt,
    ClaimOutcome, NativeReceipt, PrepareOutcome, QuestionSnapshot, RequestIdentity, StoreError,
};
use crate::database::{migration, schema};
use crate::remote::pairing::DeviceStore;
use crate::services::codex_desktop_ipc::owners::OwnerRegistry;
use crate::services::codex_desktop_ipc::types::{
    OwnerLease, QuestionDisplayDecision, QuestionDisplayGroupView, QuestionGroupIdentity4,
    QuestionNativeEffect, ThreadKey, ThreadProjection,
};
use crate::services::codex_question_skip::display::{
    overlay_projection, DisplayChanged, FrozenHistory, QuestionDisplayStore,
};
use crate::services::codex_question_skip::live::{
    owner_lease_encoding, FrozenSource, LiveRefusal, QuestionSkipLive, SourceObservation,
};
use crate::services::codex_question_skip::source::{build_source_snapshot, SourceStamp};
use crate::services::codex_question_skip::types::{
    DisplayDecision, GroupIdentity, NativeEffect, SkipReason,
};

// ===========================================================================
// 夹具常量：身份与原料
// ===========================================================================

const DEVICE: &str = "device";
const SESSION: &str = "session";
const THREAD: &str = "thread";
const HOST: &str = "local";
const OWNER_CLIENT: &str = "owner-a";
/// 天气题（合同 §6 已授权 Task4：Tuvis weather 可持久 skip/history/restore）。
const WEATHER_TITLE: &str = "weather?";
const WEATHER_OPTIONS: [&str; 2] = ["yes", "no"];
/// 内容换代后的标题（真实 builder 产出不同 content_generation）。
const WEATHER_TITLE_V2: &str = "weather? (edited)";
const TURN: &str = "turn";
const ITEM: &str = "item";
/// FrozenSource.build 字段的非空占位（display 路径不解析 build，只要求真实链在场）。
const BUILD: &str = "26.1013.31256";

fn key() -> ThreadKey {
    ThreadKey {
        host_id: HOST.into(),
        thread_id: THREAD.into(),
    }
}

/// canonical raw（与 coordinator_tests / adapter_source_actor_tests 同形：
/// 引用链完整、健康戳可过）。
fn questions_raw(questions: &[(&str, &[&str])]) -> Value {
    let qs: Vec<Value> = questions
        .iter()
        .map(|(title, options)| {
            json!({
                "title": title,
                "options": options,
            })
        })
        .collect();
    json!({
        "contextToken": "token",
        "turnHistory": {"history": {
            "isComplete": true,
            "islands": [{"entries": [{"value": "ref"}]}],
            "entitiesByKey": {"ref": {"turnId": TURN, "items": [
                {"id": ITEM, "type": "agentMessage", "questions": qs}
            ]}}
        }}
    })
}

fn weather_raw() -> Value {
    questions_raw(&[(WEATHER_TITLE, &WEATHER_OPTIONS)])
}

/// 同 turn/item、题文内容换代：真实 builder 的 content_generation 必然变化。
fn weather_raw_v2() -> Value {
    questions_raw(&[(WEATHER_TITLE_V2, &WEATHER_OPTIONS)])
}

/// ThreadProjection serde 构造（任务书：不批量 literal——新 optional 字段不破坏
/// 本夹具；stamp/owner/local_seq 均 serde skip/default）。
fn empty_projection(thread_id: &str) -> ThreadProjection {
    serde_json::from_value(json!({
        "threadId": thread_id,
        "revision": null,
        "stale": false,
        "availability": "controllable",
        "capabilities": {
            "send": true,
            "state": true,
            "command": false,
            "permissions": false,
            "blockingInput": false,
            "interrupt": false
        },
        "runtimeStatus": "idle",
        "contentState": "ready",
        "messages": [],
        "truncated": false,
        "pending": [],
        "probeStatus": {"state": "idle", "stage": null, "reasonCode": null}
    }))
    .expect("projection fixture deserializes via serde")
}

/// DAO GroupIdentity → wire group4（与 FE GroupIdentity4 逐字段同形）。
fn to_wire(group: &GroupIdentity) -> QuestionGroupIdentity4 {
    QuestionGroupIdentity4 {
        thread_id: group.thread_id.clone(),
        source_turn_id: group.source_turn_id.clone(),
        source_item_id: group.source_item_id.clone(),
        content_generation: group.content_generation.clone(),
    }
}

/// 真实链发布权威 raw source（refresh 票 + OwnerRegistry live proof +
/// build_source_snapshot），与 coordinator_tests 的 rig.publish 同路。
fn publish(
    live: &QuestionSkipLive,
    registry: &OwnerRegistry,
    lease: &OwnerLease,
    thread: &ThreadKey,
    raw: &Value,
) -> FrozenSource {
    let refresh = live
        .begin_source_refresh(thread)
        .expect("refresh ticket begins");
    let proof = registry
        .live_proof(lease)
        .expect("current owner issues a live proof");
    let stamp = SourceStamp {
        owner_lease: Some(owner_lease_encoding(lease)),
        owner_epoch: lease.epoch,
        owner_generation: lease.generation,
        mirror_build_generation: 1,
        source_seq: 0,
        stale: false,
        budget_refused: Some(false),
        truncated: Some(false),
        raw_complete: Some(true),
        display_local_seq: 0,
    };
    let observed = SourceObservation {
        snapshot: build_source_snapshot(&thread.thread_id, raw, &stamp)
            .expect("canonical raw builds a complete source"),
        proof,
        build: BUILD.into(),
    };
    live.finish_source_refresh(refresh, Ok(observed))
        .expect("authoritative refresh publishes")
}

/// 真实 builder 产物 → 当前组 group4。
fn source_group_identity(source: &FrozenSource) -> GroupIdentity {
    source.snapshot.groups[0].selector.group.clone()
}

/// 真实 builder 产物 → DAO 原题快照（questionKey/index/全文题文/有序 options，
/// 与 selector 完整成员一致）。
fn source_snapshot_question(source: &FrozenSource) -> QuestionSnapshot {
    let group = &source.snapshot.groups[0];
    QuestionSnapshot {
        question_key: group.members[0].question_key.clone(),
        index: i64::from(group.members[0].index),
        title: group.selector.members[0].title.clone(),
        options: group.selector.members[0].options.clone(),
    }
}

/// finish 可阻断内存库：`qs_finish_block.on_flag=1` 时 claimed→finished 的 UPDATE
/// 一律 ABORT（模拟 finish SQL 失败/提交未知）；返回连接 Arc 供测试翻旗。
/// （与 coordinator_tests::triggered_store 同脉，旗由本文件测试自管。）
fn blocked_store() -> (DeviceStore, Arc<Mutex<Connection>>) {
    let conn = Connection::open_in_memory().expect("打开内存库失败");
    let _ = schema::init(&conn);
    migration::migrate(&conn).expect("内存库建表失败");
    conn.execute(
        "CREATE TABLE qs_finish_block (on_flag INTEGER NOT NULL)",
        [],
    )
    .expect("block table");
    conn.execute(
        "CREATE TRIGGER qs_block_finish BEFORE UPDATE ON codex_question_skip_actions
         WHEN NEW.state = 'finished' AND OLD.state = 'claimed'
           AND (SELECT on_flag FROM qs_finish_block) = 1
         BEGIN SELECT RAISE(ABORT, 'test: finish blocked'); END;",
        [],
    )
    .expect("block trigger");
    conn.execute("INSERT INTO qs_finish_block (on_flag) VALUES (0)", [])
        .expect("disarmed finish block");
    let arc = Arc::new(Mutex::new(conn));
    (DeviceStore::Owned(Arc::clone(&arc)), arc)
}

fn set_finish_block(conn: &Mutex<Connection>, on: bool) {
    let guard = conn.lock().expect("conn lock");
    guard
        .execute(
            "UPDATE qs_finish_block SET on_flag = ?1",
            [if on { 1 } else { 0 }],
        )
        .expect("flip finish block flag");
}

/// 无绑定 skip 请求（完整身份经唯一构造器）。
fn unbound_skip(action_id: &str, group: &GroupIdentity) -> RequestIdentity {
    RequestIdentity::for_skip_unbound(
        action_id.into(),
        DEVICE.into(),
        SESSION.into(),
        key(),
        group.clone(),
    )
    .expect("unbound skip identity builds")
}

/// restore 请求（typed expected 绑定构造器）。
fn restore_request(action_id: &str, group: &GroupIdentity, expected: i64) -> RequestIdentity {
    RequestIdentity::for_restore(
        action_id.into(),
        DEVICE.into(),
        SESSION.into(),
        key(),
        group.clone(),
        expected,
    )
    .expect("restore identity builds")
}

fn closed_verified() -> NativeReceipt {
    NativeReceipt {
        effect: NativeEffect::ClosedVerified,
        reason: SkipReason::None,
    }
}

/// 装配 proof/live 材料（保持 registry 活性由调用方持有）。
fn proof_rig() -> (QuestionSkipLive, OwnerRegistry, OwnerLease) {
    let live = QuestionSkipLive::default();
    let mut registry = OwnerRegistry::default();
    let lease = registry.bind(key(), OWNER_CLIENT.into(), 1);
    (live, registry, lease)
}

// ===========================================================================
// ① 已知 DB 无行 + 完整单题 raw → active/0 current + known-empty history + 戳
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t1_known_empty_db_and_complete_single_source_project_active_zero_current_with_known_empty_history(
) {
    let store = DeviceStore::memory();
    let display = QuestionDisplayStore::new(store.clone());
    let (live, registry, lease) = proof_rig();
    let source = publish(&live, &registry, &lease, &key(), &weather_raw());
    let group = source_group_identity(&source);

    // 未装载：try_frozen 恒 None（不签假空事实）。
    assert!(
        display.try_frozen(&key()).is_none(),
        "unloaded cache must not fabricate history"
    );

    // 已知无行装载：known-empty（不是 unknown）。
    display.ensure_loaded(&key()).await;
    let frozen: FrozenHistory = display
        .try_frozen(&key())
        .expect("ensure_loaded must install the known-empty frozen history");
    assert!(
        frozen.entries().is_empty(),
        "no DB rows = known-empty history, not unknown"
    );

    // 同观察投影：current descriptor active/0 + known-empty history 账 + 戳。
    let mut p = empty_projection(THREAD);
    let stamp = overlay_projection(&mut p, Ok(&source), Some(&frozen))
        .expect("loaded history + healthy current raw must issue the observation stamp");
    assert_eq!(stamp.thread(), &key(), "stamp binds the observed thread");
    assert_eq!(
        stamp.epoch(),
        frozen.epoch(),
        "stamp carries the frozen epoch"
    );
    assert_eq!(
        p.display_stamp.as_ref(),
        Some(&stamp),
        "projection carries the same-observation stamp"
    );

    let expected_current = vec![QuestionDisplayGroupView {
        group: to_wire(&group),
        revision: 0,
        decision: QuestionDisplayDecision::Active,
        latest_native_effect: None,
        reason_code: None,
        source_complete: true,
        source_question_count: 1,
    }];
    assert_eq!(
        p.question_display_groups,
        Some(expected_current),
        "known no-row group = active/0 descriptor from the source builder"
    );
    assert_eq!(
        p.question_skip_history,
        Some(Vec::new()),
        "known-empty history must be an explicit empty account, not omitted"
    );
}

// ===========================================================================
// ①负 history 缺失（未装载）→ 两账全省略 + 无戳，绝不假填 []
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t2_unloaded_or_missing_history_omits_both_accounts_and_issues_no_stamp() {
    let store = DeviceStore::memory();
    let display = QuestionDisplayStore::new(store.clone());
    let (live, registry, lease) = proof_rig();
    let source = publish(&live, &registry, &lease, &key(), &weather_raw());

    // 从未装载：try_frozen 恒 None（合同 §8：未加载在镜像临界区前 ensure_loaded）。
    assert!(
        display.try_frozen(&key()).is_none(),
        "a store that never ran ensure_loaded must not fabricate history"
    );

    // (a) raw 健康在场但 history 未装载 → 两账全省略（current 也不补）。
    let mut p = empty_projection(THREAD);
    let stamp = overlay_projection(&mut p, Ok(&source), None);
    assert!(stamp.is_none(), "no history account → no stamp");
    assert!(
        p.question_display_groups.is_none(),
        "history missing → current account omitted too, never fabricated from raw alone"
    );
    assert!(
        p.question_skip_history.is_none(),
        "unloaded DB = no fake [] history"
    );
    assert!(p.display_stamp.is_none(), "absent accounts = absent stamp");

    // (b) source 拒绝（Unknown）且 history 未装载 → 同样全省略。
    let mut p2 = empty_projection(THREAD);
    let stamp2 = overlay_projection(&mut p2, Err(LiveRefusal::Unknown), None);
    assert!(stamp2.is_none());
    assert!(p2.question_display_groups.is_none());
    assert!(p2.question_skip_history.is_none());
    assert!(p2.display_stamp.is_none());
}

// ===========================================================================
// ② 投影尾窗 truncated=true 不撤 current descriptor（来源完整性来自源证明）
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t3_tail_window_truncation_keeps_current_descriptor_from_complete_source_proof() {
    let store = DeviceStore::memory();
    let display = QuestionDisplayStore::new(store.clone());
    let (live, registry, lease) = proof_rig();
    let source = publish(&live, &registry, &lease, &key(), &weather_raw());
    let group = source_group_identity(&source);

    display.ensure_loaded(&key()).await;
    let frozen = display
        .try_frozen(&key())
        .expect("known-empty frozen history installs");

    // 尾窗截断是消息预算语义，不属于来源健康：descriptor 照常装配。
    let mut p = empty_projection(THREAD);
    p.truncated = true;
    let stamp = overlay_projection(&mut p, Ok(&source), Some(&frozen))
        .expect("complete raw keeps the current descriptor despite tail-window truncation");

    let current = p
        .question_display_groups
        .expect("descriptor present while projection truncated");
    assert_eq!(current.len(), 1);
    assert!(
        current[0].source_complete,
        "sourceComplete comes from the source proof, not the tail window"
    );
    assert_eq!(current[0].source_question_count, 1);
    assert_eq!(current[0].group, to_wire(&group));
    assert_eq!(current[0].decision, QuestionDisplayDecision::Active);
    assert_eq!(current[0].revision, 0);
    assert_eq!(stamp.epoch(), frozen.epoch());
}

// ===========================================================================
// ② raw 拒绝 → 只省 current 账；history 仍可读；overlay 不触 DB（持连接锁可运行）
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t4_refused_source_drops_current_only_and_history_stays_readable_without_db_access() {
    // 自建内存连接（Owned），测试可持锁证明 overlay 绝不 DeviceStore.with。
    let conn = Connection::open_in_memory().expect("打开内存库失败");
    let _ = schema::init(&conn);
    migration::migrate(&conn).expect("内存库建表失败");
    let conn_arc = Arc::new(Mutex::new(conn));
    let store = DeviceStore::Owned(Arc::clone(&conn_arc));

    let (live, registry, lease) = proof_rig();
    let source = publish(&live, &registry, &lease, &key(), &weather_raw());
    let group = source_group_identity(&source);
    let snapshot = source_snapshot_question(&source);
    let identity = unbound_skip("act-t4", &group);

    // 直seed：skip 落账 → 合法 claim（DAO 合同：finish 仅在 Claimed 上落终态）→
    // finished（closed_verified）。
    store.with(|c| {
        assert!(matches!(
            prepare_skip(c, &identity, &snapshot, 10),
            Ok(PrepareOutcome::Prepared { revision: 1 })
        ));
        assert_eq!(
            claim_cas(c, &identity, "cmd-t4", 11),
            Ok(ClaimOutcome::Claimed)
        );
        assert_eq!(
            finish(c, &identity, &closed_verified(), 12),
            Ok(ActionReceipt {
                action_id: identity.action_id().to_string(),
                display: DisplayDecision::Skipped,
                revision: 1,
                native: closed_verified(),
            })
        );
    });

    let display = QuestionDisplayStore::new(store.clone());
    display.ensure_loaded(&key()).await;
    let frozen = display
        .try_frozen(&key())
        .expect("seeded history freezes for load");

    // 持连接锁期间 overlay 只读冻结产物仍可运行（合同 §8：overlay 绝不触 DB）。
    let _conn_guard = conn_arc.lock().expect("hold conn lock for the seam");
    let mut p = empty_projection(THREAD);
    let stamp = overlay_projection(&mut p, Err(LiveRefusal::Unknown), Some(&frozen))
        .expect("loaded history stays readable without a healthy current raw");
    assert_eq!(stamp.epoch(), frozen.epoch());
    drop(_conn_guard);

    assert!(
        p.question_display_groups.is_none(),
        "no healthy current raw → current account omitted, never filled from history"
    );
    let history = p
        .question_skip_history
        .expect("history account present while current raw refused");
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].group, to_wire(&group));
    assert_eq!(history[0].decision, QuestionDisplayDecision::Skipped);
    assert_eq!(history[0].revision, 1);
    assert_eq!(
        history[0].latest_native_effect,
        Some(QuestionNativeEffect::ClosedVerified),
        "latest skip native comes from the frozen receipt"
    );
    assert_eq!(
        history[0].reason_code, None,
        "SkipReason::None maps to null, not a fabricated reason code"
    );
    assert_eq!(
        history[0].question.as_ref().map(|q| q.question_key.clone()),
        Some(snapshot.question_key.clone()),
        "history question snapshot maps key/index/title/options from the frozen entry"
    );
}

// ===========================================================================
// ③ 同 source 新 generation active/0 与旧 generation skipped 共存；
//    current 每 source group 恰一组、题数来自 source builder
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t5_new_generation_active_zero_coexists_with_old_generation_skipped_history() {
    let store = DeviceStore::memory();
    let (live, registry, lease) = proof_rig();

    // gen1：真实 builder 产组 + 真实快照落账（skipped/1，closed_verified）。
    // 合法三步：prepare → claim（DAO 合同：finish 仅在 Claimed 上）→ finish。
    let source1 = publish(&live, &registry, &lease, &key(), &weather_raw());
    let group1 = source_group_identity(&source1);
    let snapshot1 = source_snapshot_question(&source1);
    let identity1 = unbound_skip("act-gen1", &group1);
    store.with(|c| {
        assert!(matches!(
            prepare_skip(c, &identity1, &snapshot1, 10),
            Ok(PrepareOutcome::Prepared { revision: 1 })
        ));
        assert_eq!(
            claim_cas(c, &identity1, "cmd-gen1", 11),
            Ok(ClaimOutcome::Claimed)
        );
        assert!(finish(c, &identity1, &closed_verified(), 12).is_ok());
    });

    // gen2：同 turn/item、题文换代 → 真实 builder 产出新 content_generation。
    let source2 = publish(&live, &registry, &lease, &key(), &weather_raw_v2());
    let group2 = source_group_identity(&source2);
    assert_ne!(
        group2, group1,
        "edited content must be a new generation via the real builder"
    );

    let display = QuestionDisplayStore::new(store.clone());
    display.ensure_loaded(&key()).await;
    let frozen = display
        .try_frozen(&key())
        .expect("thread history freezes with the old generation row");

    let mut p = empty_projection(THREAD);
    let stamp = overlay_projection(&mut p, Ok(&source2), Some(&frozen))
        .expect("new generation source + loaded history issue the stamp");
    assert_eq!(stamp.epoch(), frozen.epoch());

    let current = p
        .question_display_groups
        .expect("current descriptor for the new generation");
    assert_eq!(
        current.len(),
        1,
        "exactly one current group per source group, not per DB row"
    );
    assert_eq!(current[0].group, to_wire(&group2));
    assert_eq!(
        current[0].decision,
        QuestionDisplayDecision::Active,
        "new generation has no DB row yet → known active"
    );
    assert_eq!(current[0].revision, 0);
    assert_eq!(current[0].latest_native_effect, None);
    assert_eq!(current[0].source_question_count, 1,
        "sourceQuestionCount comes from the source builder (declared_count), not filtered asyncQuestions");
    assert!(current[0].source_complete);

    let history = p
        .question_skip_history
        .expect("old generation history coexists");
    assert_eq!(history.len(), 1, "old generation history preserved");
    assert_eq!(history[0].group, to_wire(&group1));
    assert_eq!(history[0].decision, QuestionDisplayDecision::Skipped);
    assert_eq!(history[0].revision, 1);
    assert_eq!(
        history[0].latest_native_effect,
        Some(QuestionNativeEffect::ClosedVerified)
    );
    assert_eq!(
        history[0].question.as_ref().map(|q| q.title.clone()),
        Some(WEATHER_TITLE.to_string()),
        "old generation snapshot keeps its own title, never the new generation's"
    );
}

// ===========================================================================
// ④ prepare 快照 + restore 后迟到 finish 不重藏；latest skip native 不被遮
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t6_restore_then_late_finish_never_rehides_and_latest_skip_native_stays_visible() {
    let store = DeviceStore::memory();
    let (live, registry, lease) = proof_rig();
    let source = publish(&live, &registry, &lease, &key(), &weather_raw());
    let group = source_group_identity(&source);
    let snapshot = source_snapshot_question(&source);
    let identity = unbound_skip("act-t6", &group);

    // A：prepare + claim（finish 未落账）→ skipped/1，在途 native 保守 Unresolved。
    store.with(|c| {
        assert!(matches!(
            prepare_skip(c, &identity, &snapshot, 10),
            Ok(PrepareOutcome::Prepared { revision: 1 })
        ));
        assert_eq!(
            claim_cas(c, &identity, "cmd-t6", 11),
            Ok(ClaimOutcome::Claimed)
        );
    });
    let display = QuestionDisplayStore::new(store.clone());
    display.ensure_loaded(&key()).await;
    let frozen_a = display.try_frozen(&key()).expect("skip freezes");
    let mut pa = empty_projection(THREAD);
    let stamp_a = overlay_projection(&mut pa, Ok(&source), Some(&frozen_a))
        .expect("skipped current + history issue the stamp");
    assert_eq!(stamp_a.epoch(), frozen_a.epoch());
    let current_a = pa.question_display_groups.expect("current present");
    assert_eq!(current_a[0].decision, QuestionDisplayDecision::Skipped);
    assert_eq!(current_a[0].revision, 1);
    assert_eq!(
        current_a[0].latest_native_effect,
        Some(QuestionNativeEffect::Unresolved),
        "in-flight claim maps to the conservative Unresolved, not a fabricated close"
    );
    assert_eq!(pa.question_skip_history.as_ref().map(|h| h.len()), Some(1));

    // B：restore（持久 revision CAS）→ active/2；latest skip native 不被遮。
    let restore_id = restore_request("restore-t6", &group, 1);
    store.with(|c| {
        let receipt = restore(c, &restore_id, 1, 12).expect("restore cas succeeds");
        assert_eq!(receipt.display, DisplayDecision::Active);
        assert_eq!(receipt.revision, 2);
    });
    let display_b = QuestionDisplayStore::new(store.clone());
    display_b.ensure_loaded(&key()).await;
    let frozen_b = display_b
        .try_frozen(&key())
        .expect("restored history freezes");
    let mut pb = empty_projection(THREAD);
    overlay_projection(&mut pb, Ok(&source), Some(&frozen_b))
        .expect("restored state still projects");
    let current_b = pb
        .question_display_groups
        .expect("current present after restore");
    assert_eq!(current_b[0].decision, QuestionDisplayDecision::Active);
    assert_eq!(current_b[0].revision, 2);
    let history_b = pb
        .question_skip_history
        .expect("history readable after restore");
    assert_eq!(history_b[0].decision, QuestionDisplayDecision::Active);
    assert_eq!(history_b[0].revision, 2);
    assert_eq!(
        history_b[0].latest_native_effect,
        Some(QuestionNativeEffect::Unresolved),
        "restore does not mask the skip's native account"
    );

    // C：迟到 finish 首次落账 → 展示仍 active/2（finish 绝不改展示行，不重藏）。
    store.with(|c| {
        let late = finish(c, &identity, &closed_verified(), 13).expect("late finish settles");
        assert_eq!(late.display, DisplayDecision::Skipped);
        assert_eq!(
            late.revision, 1,
            "late finish returns the frozen skip result"
        );
    });
    let display_c = QuestionDisplayStore::new(store.clone());
    display_c.ensure_loaded(&key()).await;
    let frozen_c = display_c
        .try_frozen(&key())
        .expect("late-finished history freezes");
    let mut pc = empty_projection(THREAD);
    overlay_projection(&mut pc, Ok(&source), Some(&frozen_c))
        .expect("projection after late finish");
    let current_c = pc
        .question_display_groups
        .expect("current stays visible after late finish");
    assert_eq!(
        current_c[0].decision,
        QuestionDisplayDecision::Active,
        "late finish must not re-hide the restored card"
    );
    assert_eq!(current_c[0].revision, 2);
    assert_eq!(
        current_c[0].latest_native_effect,
        Some(QuestionNativeEffect::ClosedVerified)
    );
    let history_c = pc.question_skip_history.expect("history after late finish");
    assert_eq!(
        history_c[0].latest_native_effect,
        Some(QuestionNativeEffect::ClosedVerified)
    );
    assert_eq!(
        history_c[0]
            .question
            .as_ref()
            .map(|q| (q.question_key.clone(), q.question_index)),
        Some((snapshot.question_key.clone(), snapshot.index)),
        "question snapshot maps questionKey/questionIndex/title/options for the restore entry"
    );
    assert_eq!(
        history_c[0].question.as_ref().map(|q| q.options.clone()),
        Some(
            WEATHER_OPTIONS
                .iter()
                .map(|s| s.to_string())
                .collect::<Vec<_>>()
        )
    );
}

// ===========================================================================
// ④ 旧行 NULL 快照保持 null；绝不借新 generation 题文
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t7_old_row_without_snapshot_keeps_null_question_and_never_borrows_new_generation() {
    let store = DeviceStore::memory();
    // 旧行直插（老库升级形态）：skipped/7、快照 NULL、无动作行。
    store.with(|c| {
        let inserted = c
            .execute(
                "INSERT INTO codex_question_display
                    (thread_id, source_turn_id, source_item_id, content_generation,
                     decision, revision, device_id, updated_at, source_snapshot_json)
                 VALUES (?1, 't-old', 'i-old', 'gen-old', 'skipped', 7, ?2, 9, NULL)",
                params![THREAD, DEVICE],
            )
            .expect("seed legacy display row");
        assert_eq!(inserted, 1);
    });

    let (live, registry, lease) = proof_rig();
    let source = publish(&live, &registry, &lease, &key(), &weather_raw());
    let group = source_group_identity(&source);

    let display = QuestionDisplayStore::new(store.clone());
    display.ensure_loaded(&key()).await;
    let frozen = display.try_frozen(&key()).expect("legacy row freezes");

    let mut p = empty_projection(THREAD);
    let stamp = overlay_projection(&mut p, Ok(&source), Some(&frozen))
        .expect("legacy row + current raw project");
    assert_eq!(stamp.epoch(), frozen.epoch());

    let current = p
        .question_display_groups
        .expect("current descriptor present");
    assert_eq!(
        current.len(),
        1,
        "only the source group has a current descriptor"
    );
    assert_eq!(current[0].group, to_wire(&group));
    assert_eq!(current[0].decision, QuestionDisplayDecision::Active);
    assert_eq!(
        current[0].revision, 0,
        "source group has no DB row → active/0"
    );

    let history = p.question_skip_history.expect("legacy history readable");
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].group.content_generation, "gen-old");
    assert_eq!(history[0].decision, QuestionDisplayDecision::Skipped);
    assert_eq!(history[0].revision, 7);
    assert!(
        history[0].question.is_none(),
        "NULL snapshot stays null — never borrowed from the new generation's title"
    );
    assert_eq!(
        history[0].latest_native_effect, None,
        "no action row = no fabricated native effect"
    );
}

// ===========================================================================
// ⑤⑥ DB 写经 §8 包装失效旧 stamp；epoch checked 前进；新观察重新取得资格；
//     旧冻结票跨 restore 不能安装/取得新发布资格；stale stamp 闭包零执行
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t8_db_write_invalidates_old_stamp_and_newer_observation_regains_eligibility() {
    let store = DeviceStore::memory();
    let display = QuestionDisplayStore::new(store.clone());
    let (live, registry, lease) = proof_rig();
    let source = publish(&live, &registry, &lease, &key(), &weather_raw());
    let group = source_group_identity(&source);
    let snapshot = source_snapshot_question(&source);
    let identity = unbound_skip("act-t8", &group);

    // 装载前 seed：skipped/1。
    store.with(|c| {
        assert!(matches!(
            prepare_skip(c, &identity, &snapshot, 10),
            Ok(PrepareOutcome::Prepared { revision: 1 })
        ));
    });
    display.ensure_loaded(&key()).await;
    let frozen_a = display.try_frozen(&key()).expect("frozen A installs");
    let epoch_a = frozen_a.epoch();

    let mut p = empty_projection(THREAD);
    let stamp_a = overlay_projection(&mut p, Ok(&source), Some(&frozen_a))
        .expect("observation issues stamp A");
    assert_eq!(stamp_a.epoch(), epoch_a);

    // 同 epoch 新观察取得发布资格，reserve 闭包恰好执行一次。
    let mut reserves = 0usize;
    let granted = display.with_publish_eligibility(&stamp_a, || {
        reserves += 1;
        41u64
    });
    assert_eq!(granted, Ok(41), "fresh stamp gains publish eligibility");
    assert_eq!(reserves, 1, "reserve closure runs exactly once per grant");

    // 本功能写必须经 §8 包装：restore → 旧 stamp 立即失配，闭包绝不执行。
    let restore_id = restore_request("restore-t8", &group, 1);
    let receipt = display
        .run_mutation(Some(&key()), |c| restore(c, &restore_id, 1, 13))
        .expect("mutation wrapper executes the restore SQL");
    assert_eq!(receipt.display, DisplayDecision::Active);
    assert_eq!(receipt.revision, 2);

    let mut reserves_stale = 0usize;
    assert_eq!(
        display.with_publish_eligibility(&stamp_a, || {
            reserves_stale += 1;
            42u64
        }),
        Err(DisplayChanged),
        "stamp across restore must fail eligibility"
    );
    assert_eq!(
        reserves_stale, 0,
        "stale stamp must never reach reserve_publish_seq"
    );

    // 包装成功后缓存已重装（无需再 ensure_loaded）：epoch checked 前进、内容更新。
    let frozen_b = display
        .try_frozen(&key())
        .expect("successful mutation refreezes and installs");
    assert!(
        frozen_b.epoch() > epoch_a,
        "cache epoch strictly advances (checked_add, never reused)"
    );
    let entry_b = &frozen_b.entries()[0];
    assert_eq!(
        (entry_b.decision.clone(), entry_b.revision),
        (DisplayDecision::Active, 2),
        "refrozen content reflects the restore"
    );

    // 新观察取得新资格。
    let mut p2 = empty_projection(THREAD);
    let stamp_b = overlay_projection(&mut p2, Ok(&source), Some(&frozen_b))
        .expect("new observation issues stamp B");
    let mut reserves_b = 0usize;
    assert!(
        display
            .with_publish_eligibility(&stamp_b, || {
                reserves_b += 1;
                43u64
            })
            .is_ok(),
        "newer observation regains eligibility"
    );
    assert_eq!(reserves_b, 1);

    // 旧冻结票（restore 前读取）即使再次投影也只产出失配戳：不能安装/取得新资格。
    let mut p3 = empty_projection(THREAD);
    let stamp_old = overlay_projection(&mut p3, Ok(&source), Some(&frozen_a.clone()))
        .expect("stale frozen still projects, stamp carries its own epoch");
    assert_eq!(stamp_old.epoch(), epoch_a);
    assert_eq!(
        display.with_publish_eligibility(&stamp_old, || 44u64),
        Err(DisplayChanged),
        "old DB read across restore cannot install or gain new publish eligibility"
    );
}

// ===========================================================================
// ⑥ §8 包装覆盖 prepare/claim/finish/settle/recover；失败取消 → Unknown，可恢复
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t9_mutation_wrapper_covers_skip_lifecycle_and_failures_go_unknown_then_recover() {
    let (store, conn_arc) = blocked_store();
    let display = QuestionDisplayStore::new(store.clone());
    let (live, registry, lease) = proof_rig();
    let source = publish(&live, &registry, &lease, &key(), &weather_raw());
    let group = source_group_identity(&source);
    let snapshot = source_snapshot_question(&source);
    let identity1 = unbound_skip("act-t9-a", &group);

    // prepare 经包装：SQL 执行 + 缓存就地重装（skipped/1）。
    let prepared = display
        .run_mutation(Some(&key()), |c| prepare_skip(c, &identity1, &snapshot, 10))
        .expect("wrapper prepares the skip");
    assert!(matches!(prepared, PrepareOutcome::Prepared { revision: 1 }));
    let frozen1 = display
        .try_frozen(&key())
        .expect("successful mutation installs the frozen cache");
    let epoch1 = frozen1.epoch();
    assert_eq!(
        (
            frozen1.entries()[0].decision.clone(),
            frozen1.entries()[0].revision
        ),
        (DisplayDecision::Skipped, 1)
    );

    // claim 经包装。
    let claimed = display
        .run_mutation(Some(&key()), |c| claim_cas(c, &identity1, "cmd-t9", 11))
        .expect("wrapper claims the action");
    assert_eq!(claimed, ClaimOutcome::Claimed);

    // finish 失败（提交未知模拟）：包装返回 Database，缓存取消为 Unknown
    //（try_frozen None——失败不安装旧读、不伪造新写）。
    set_finish_block(&conn_arc, true);
    let failure = display
        .run_mutation(Some(&key()), |c| {
            finish(c, &identity1, &closed_verified(), 12)
        })
        .expect_err("blocked finish must surface the store error");
    assert_eq!(failure, StoreError::Database);
    assert!(
        display.try_frozen(&key()).is_none(),
        "failed mutation leaves the cache Unknown, never a stale install"
    );

    // 解除阻断 → 同一封存值 finish 成功 → 缓存重装（epoch 前进），latest_skip
    // 为落账 native。
    set_finish_block(&conn_arc, false);
    let receipt1 = display
        .run_mutation(Some(&key()), |c| {
            finish(c, &identity1, &closed_verified(), 13)
        })
        .expect("retry after failure settles the same sealed value");
    assert_eq!(receipt1.display, DisplayDecision::Skipped);
    assert_eq!(receipt1.revision, 1);
    let frozen2 = display
        .try_frozen(&key())
        .expect("successful finish refreezes");
    assert!(frozen2.epoch() > epoch1, "epoch advances across mutations");
    let latest1 = frozen2.entries()[0]
        .latest_skip
        .as_ref()
        .expect("finished skip carries its frozen receipt");
    assert_eq!(latest1.native.effect, NativeEffect::ClosedVerified);

    // settle_without_invoke 经包装（第二个无绑定动作）：展示 revision 递增、
    // latest skip 按最新动作（seq 最大）表达 Unresolved/BindingRequired。
    let identity2 = unbound_skip("act-t9-b", &group);
    let prepared2 = display
        .run_mutation(Some(&key()), |c| prepare_skip(c, &identity2, &snapshot, 14))
        .expect("wrapper prepares the second skip");
    assert!(matches!(
        prepared2,
        PrepareOutcome::Prepared { revision: 2 }
    ));
    let settled = display
        .run_mutation(Some(&key()), |c| {
            settle_without_invoke(
                c,
                &identity2,
                &NativeReceipt {
                    effect: NativeEffect::Unresolved,
                    reason: SkipReason::BindingRequired,
                },
                15,
            )
        })
        .expect("wrapper settles the never-invoked action");
    assert_eq!(settled.display, DisplayDecision::Skipped);
    assert_eq!(settled.revision, 2);
    assert_eq!(settled.native.effect, NativeEffect::Unresolved);
    assert_eq!(settled.native.reason, SkipReason::BindingRequired);
    let frozen3 = display.try_frozen(&key()).expect("settle refreezes");
    assert!(frozen3.epoch() > frozen2.epoch());
    assert_eq!(frozen3.entries()[0].decision, DisplayDecision::Skipped);
    assert_eq!(frozen3.entries()[0].revision, 2);
    let latest2 = frozen3.entries()[0]
        .latest_skip
        .as_ref()
        .expect("latest skip follows the newest action");
    assert_eq!(latest2.action_id, identity2.action_id());
    assert_eq!(latest2.native.effect, NativeEffect::Unresolved);
    assert_eq!(latest2.native.reason, SkipReason::BindingRequired);

    // recover（全局，key=None）经包装：无遗留 → Ok(0)。
    assert_eq!(
        display.run_mutation(None, |c| recover(c, 16)),
        Ok(0),
        "global recover is a None-key mutation on the same wrapper"
    );
}
