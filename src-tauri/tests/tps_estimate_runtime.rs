//! 独立临时库：估算事实不能修改原生账本或游标。
use multi_agents_manager_lib::{
    database::{migration, schema},
    services::tps::{
        estimate_store::{commit_estimate, CaptureGate, EstimateFact},
        store,
    },
};
use rusqlite::Connection;

fn fact(turn: &str) -> EstimateFact {
    EstimateFact {
        host_key: multi_agents_manager_lib::services::tps::estimate_store::stable_identity(
            "host", "host",
        ),
        thread_key: multi_agents_manager_lib::services::tps::estimate_store::stable_identity(
            "thread", "thread",
        ),
        turn_key: multi_agents_manager_lib::services::tps::estimate_store::stable_identity(
            "turn", turn,
        ),
        item_key: multi_agents_manager_lib::services::tps::estimate_store::stable_identity(
            "item", "item",
        ),
        model: Some("model".into()),
        channel: None,
        model_source: "turn-config".into(),
        reference_profile_id:
            multi_agents_manager_lib::services::tps::reference::reference_profile()
                .id
                .clone(),
        reference_tokens: 100,
        duration_ns: 4_000_000_000,
        ended_at_ms: 10000,
        run_id: "run".into(),
        source_build: "test".into(),
        short_window: false,
        small_sample: false,
    }
}
#[test]
fn estimate_commit_is_idempotent_and_does_not_touch_usage() {
    let mut conn = Connection::open_in_memory().unwrap();
    schema::init(&conn);
    migration::migrate(&conn).unwrap();
    migration::migrate(&conn).unwrap();
    store::save_state(&conn, "cursor:test", &serde_json::json!({"offset":42})).unwrap();
    let gate = CaptureGate::new(true);
    let permit = gate.permit().unwrap();
    assert!(commit_estimate(&mut conn, &fact("turn-a"), &permit).unwrap());
    assert!(!commit_estimate(&mut conn, &fact("turn-a"), &permit).unwrap());
    assert!(commit_estimate(&mut conn, &fact("turn-b"), &permit).unwrap());
    assert_eq!(
        conn.query_row("SELECT count(*) FROM tps_body_estimate", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        2
    );
    assert_eq!(
        conn.query_row("SELECT count(*) FROM tps_observation", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        0
    );
    assert_eq!(
        conn.query_row("SELECT count(*) FROM usage_detail", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        0
    );
    assert_eq!(
        store::load_state(&conn, "cursor:test").unwrap(),
        Some(serde_json::json!({"offset":42}))
    );
}

#[test]
fn saved_master_disable_reenable_rejects_old_capture_permit() {
    use multi_agents_manager_lib::services::usage::{
        model::UsageSettings, settings::save_to_conn_with_gate,
    };
    let mut conn = Connection::open_in_memory().unwrap();
    schema::init(&conn);
    migration::migrate(&conn).unwrap();
    let gate = CaptureGate::new(true);
    let old = gate.permit().unwrap();
    let mut settings = UsageSettings::default();
    settings.enabled = false;
    save_to_conn_with_gate(&conn, &settings, &gate).unwrap();
    settings.enabled = true;
    save_to_conn_with_gate(&conn, &settings, &gate).unwrap();
    assert!(
        !commit_estimate(&mut conn, &fact("old-turn"), &old).unwrap(),
        "old work must be rejected after stop and restart"
    );
    let current = gate.permit().unwrap();
    assert!(commit_estimate(&mut conn, &fact("new-turn"), &current).unwrap());
    assert_eq!(
        conn.query_row("SELECT count(*) FROM tps_body_estimate", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        1
    );
}

#[test]
fn saved_source_disable_reenable_rejects_old_capture_permit() {
    use multi_agents_manager_lib::services::tps::estimate_store::{
        set_estimate_enabled_with_gate, ENABLED_KEY,
    };
    let mut conn = Connection::open_in_memory().unwrap();
    schema::init(&conn);
    migration::migrate(&conn).unwrap();
    let gate = CaptureGate::new(true);
    let old = gate.permit().unwrap();
    set_estimate_enabled_with_gate(&conn, false, &gate).unwrap();
    set_estimate_enabled_with_gate(&conn, true, &gate).unwrap();
    assert_eq!(
        store::load_state(&conn, ENABLED_KEY).unwrap(),
        Some(serde_json::json!(true))
    );
    assert!(!commit_estimate(&mut conn, &fact("old-source"), &old).unwrap());
    assert!(commit_estimate(&mut conn, &fact("new-source"), &gate.permit().unwrap()).unwrap());
}

#[test]
fn saved_switch_transitions_notify_observer_even_before_tick() {
    use multi_agents_manager_lib::services::tps::estimate_store::set_estimate_enabled_with_gate;
    let conn = Connection::open_in_memory().unwrap();
    schema::init(&conn);
    migration::migrate(&conn).unwrap();
    let gate = CaptureGate::new(true);
    let mut notifications = gate.subscribe().unwrap();
    let before = *notifications.borrow_and_update();
    set_estimate_enabled_with_gate(&conn, false, &gate).unwrap();
    set_estimate_enabled_with_gate(&conn, true, &gate).unwrap();
    assert!(notifications.has_changed().unwrap());
    assert!(*notifications.borrow_and_update() >= before + 2);
}

#[test]
fn estimate_rejects_raw_session_identity() {
    let mut conn = Connection::open_in_memory().unwrap();
    schema::init(&conn);
    migration::migrate(&conn).unwrap();
    let gate = CaptureGate::new(true);
    let mut value = fact("turn-a");
    value.thread_key = "/Users/example/private/project".into();
    assert!(commit_estimate(&mut conn, &value, &gate.permit().unwrap()).is_err());
    assert_eq!(
        conn.query_row("SELECT count(*) FROM tps_body_estimate", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        0
    );
}

#[test]
fn positive_submillisecond_window_is_not_truncated_to_zero() {
    let mut conn = Connection::open_in_memory().unwrap();
    schema::init(&conn);
    migration::migrate(&conn).unwrap();
    let gate = CaptureGate::new(true);
    let mut value = fact("short-window");
    value.set_duration_ns(500_000);
    assert!(commit_estimate(&mut conn, &value, &gate.permit().unwrap()).unwrap());
}

#[test]
fn estimate_retention_uses_same_24_hour_cutoff() {
    let mut conn = Connection::open_in_memory().unwrap();
    schema::init(&conn);
    migration::migrate(&conn).unwrap();
    let gate = CaptureGate::new(true);
    let permit = gate.permit().unwrap();
    let mut old = fact("old");
    old.ended_at_ms = 999;
    let mut boundary = fact("boundary");
    boundary.ended_at_ms = 1000;
    assert!(commit_estimate(&mut conn, &old, &permit).unwrap());
    assert!(commit_estimate(&mut conn, &boundary, &permit).unwrap());
    store::purge(&conn, 1000 + store::TPS_RETENTION_MS).unwrap();
    assert_eq!(
        conn.query_row("SELECT count(*) FROM tps_body_estimate", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        1
    );
    assert_eq!(
        conn.query_row("SELECT ended_at_ms FROM tps_body_estimate", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        1000
    );
}

#[test]
fn event_queue_and_live_candidates_share_global_body_budget() {
    use multi_agents_manager_lib::services::{
        codex_desktop_ipc::observation::{
            body_event_channel, BodyChange, BodyFrame, CaptureClock, CaptureScope,
        },
        tps::body_observer::BodyObserver,
    };
    let (tx, rx) = body_event_channel(32);
    let mut observer = BodyObserver::with_budget(rx.memory_budget());
    let scope = CaptureScope {
        capture_generation: 1,
        owner_epoch: 1,
        owner_generation: 1,
        host_key: "h".into(),
        thread_key: "t".into(),
    };
    let frame = |revision, change| BodyFrame {
        scope: scope.clone(),
        revision,
        clock: CaptureClock {
            mono_ns: revision,
            wall_ms: revision as i64,
        },
        changes: vec![change],
    };
    observer.on_frame(frame(1, BodyChange::Baseline));
    for i in 0..15_u64 {
        let item = i.to_string();
        observer.on_frame(frame(
            2 + i * 2,
            BodyChange::EmptyItem {
                turn_id: "turn".into(),
                item_id: item.clone(),
                model: None,
                channel: None,
            },
        ));
        observer.on_frame(frame(
            3 + i * 2,
            BodyChange::Append {
                turn_id: "turn".into(),
                item_id: item,
                text: "a".repeat(1024 * 1024),
            },
        ));
    }
    let mut queued = frame(
        40,
        BodyChange::Append {
            turn_id: "turn".into(),
            item_id: "x".into(),
            text: "x".repeat(1024 * 1024),
        },
    );
    queued.changes.push(BodyChange::Append {
        turn_id: "turn".into(),
        item_id: "y".into(),
        text: "y".repeat(1024 * 1024),
    });
    assert!(
        !tx.try_send(queued),
        "live candidates and queued bytes together must not exceed 16 MiB"
    );
}

#[test]
fn finished_bodies_keep_budget_until_worker_releases_them() {
    use multi_agents_manager_lib::services::{
        codex_desktop_ipc::observation::{
            body_event_channel, BodyChange, BodyFrame, CaptureClock, CaptureScope,
        },
        tps::body_observer::BodyObserver,
    };
    let (tx, rx) = body_event_channel(32);
    let mut observer = BodyObserver::with_budget(rx.memory_budget());
    let scope = CaptureScope {
        capture_generation: 1,
        owner_epoch: 1,
        owner_generation: 1,
        host_key: "h".into(),
        thread_key: "t".into(),
    };
    let mut rev = 1u64;
    let frame = |revision, change| BodyFrame {
        scope: scope.clone(),
        revision,
        clock: CaptureClock {
            mono_ns: revision,
            wall_ms: revision as i64,
        },
        changes: vec![change],
    };
    observer.on_frame(frame(rev, BodyChange::Baseline));
    let mut finished = Vec::new();
    for i in 0..16 {
        let turn = format!("turn-{i}");
        let item = format!("item-{i}");
        rev += 1;
        observer.on_frame(frame(
            rev,
            BodyChange::EmptyItem {
                turn_id: turn.clone(),
                item_id: item.clone(),
                model: None,
                channel: None,
            },
        ));
        for _ in 0..2 {
            rev += 1;
            observer.on_frame(frame(
                rev,
                BodyChange::Append {
                    turn_id: turn.clone(),
                    item_id: item.clone(),
                    text: "a".repeat(512 * 1024),
                },
            ));
        }
        rev += 1;
        finished.extend(observer.on_frame(frame(
            rev,
            BodyChange::TurnTerminal {
                turn_id: turn,
                completed: true,
            },
        )));
    }
    assert_eq!(finished.len(), 16);
    let overflow = frame(
        100,
        BodyChange::Append {
            turn_id: "other".into(),
            item_id: "x".into(),
            text: "x".into(),
        },
    );
    assert!(
        !tx.try_send(overflow),
        "queued completed bodies must retain budget"
    );
    drop(finished.pop());
    let next = frame(
        101,
        BodyChange::Append {
            turn_id: "other".into(),
            item_id: "x".into(),
            text: "x".into(),
        },
    );
    assert!(
        tx.try_send(next),
        "released completed body must return budget"
    );
}

fn finished_body(
    turn: &str,
) -> multi_agents_manager_lib::services::tps::body_observer::FinishedBody {
    use multi_agents_manager_lib::services::{
        codex_desktop_ipc::observation::{BodyChange, BodyFrame, CaptureClock, CaptureScope},
        tps::body_observer::BodyObserver,
    };
    let scope = CaptureScope {
        capture_generation: 1,
        owner_epoch: 1,
        owner_generation: 1,
        host_key: "host".into(),
        thread_key: "thread".into(),
    };
    let frame = |revision, change| BodyFrame {
        scope: scope.clone(),
        revision,
        clock: CaptureClock {
            mono_ns: revision * 1_000_000_000,
            wall_ms: revision as i64 * 1000,
        },
        changes: vec![change],
    };
    let mut o = BodyObserver::default();
    o.on_frame(frame(1, BodyChange::Baseline));
    o.on_frame(frame(
        2,
        BodyChange::EmptyItem {
            turn_id: turn.into(),
            item_id: "item".into(),
            model: Some("model".into()),
            channel: None,
        },
    ));
    o.on_frame(frame(
        3,
        BodyChange::Append {
            turn_id: turn.into(),
            item_id: "item".into(),
            text: "hello ".into(),
        },
    ));
    o.on_frame(frame(
        4,
        BodyChange::Append {
            turn_id: turn.into(),
            item_id: "item".into(),
            text: "world".into(),
        },
    ));
    o.on_frame(frame(
        5,
        BodyChange::TurnTerminal {
            turn_id: turn.into(),
            completed: true,
        },
    ))
    .pop()
    .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn late_tokenization_cannot_cross_saved_master_disable_reenable() {
    use multi_agents_manager_lib::services::{
        tps::body_runtime::{BeforeCommitHook, BodyRuntime},
        usage::{model::UsageSettings, settings::save_to_conn_with_gate},
    };
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Condvar, Mutex,
    };
    let conn = Connection::open_in_memory().unwrap();
    schema::init(&conn);
    migration::migrate(&conn).unwrap();
    let db = Arc::new(Mutex::new(conn));
    let gate = CaptureGate::new(true);
    let old = gate.permit().unwrap();
    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let entered = Arc::new(Mutex::new(Some(entered_tx)));
    let latch = Arc::new((Mutex::new(false), Condvar::new()));
    let fired = Arc::new(AtomicBool::new(false));
    let hook: BeforeCommitHook = {
        let entered = entered.clone();
        let latch = latch.clone();
        let fired = fired.clone();
        Arc::new(move || {
            if !fired.swap(true, Ordering::SeqCst) {
                if let Some(tx) = entered.lock().unwrap().take() {
                    let _ = tx.send(());
                }
                let (lock, wake) = &*latch;
                let mut open = lock.lock().unwrap();
                while !*open {
                    open = wake.wait(open).unwrap();
                }
            }
        })
    };
    let runtime = BodyRuntime::new_with_gate(db.clone(), gate.clone(), Some(hook));
    let old_result = runtime
        .try_submit(finished_body("old"), old, "run".into(), "build".into())
        .expect("enqueue");
    tokio::time::timeout(std::time::Duration::from_secs(3), entered_rx)
        .await
        .expect("tokenizer must reach commit gate")
        .unwrap();
    {
        let conn = db.lock().unwrap();
        let mut settings = UsageSettings::default();
        settings.enabled = false;
        save_to_conn_with_gate(&conn, &settings, &gate).unwrap();
        settings.enabled = true;
        save_to_conn_with_gate(&conn, &settings, &gate).unwrap();
    }
    {
        let (lock, wake) = &*latch;
        *lock.lock().unwrap() = true;
        wake.notify_all();
    }
    assert!(
        !tokio::time::timeout(std::time::Duration::from_secs(3), old_result)
            .await
            .unwrap()
            .unwrap()
            .unwrap()
    );
    let new_result = runtime
        .try_submit(
            finished_body("new"),
            gate.permit().unwrap(),
            "run".into(),
            "build".into(),
        )
        .unwrap();
    assert!(
        tokio::time::timeout(std::time::Duration::from_secs(3), new_result)
            .await
            .unwrap()
            .unwrap()
            .unwrap()
    );
    assert_eq!(
        db.lock()
            .unwrap()
            .query_row("SELECT count(*) FROM tps_body_estimate", [], |r| r
                .get::<_, i64>(0))
            .unwrap(),
        1
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn disabling_source_drains_waiting_worker_text_even_when_worker_is_blocked() {
    use multi_agents_manager_lib::services::{
        codex_desktop_ipc::observation::{
            body_event_channel, BodyChange, BodyFrame, CaptureClock, CaptureScope,
        },
        tps::{
            body_observer::BodyObserver,
            body_runtime::{BeforeCommitHook, BodyRuntime},
            estimate_store::set_estimate_enabled_with_gate,
        },
    };
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Condvar, Mutex,
    };
    let conn = Connection::open_in_memory().unwrap();
    schema::init(&conn);
    migration::migrate(&conn).unwrap();
    let db = Arc::new(Mutex::new(conn));
    let gate = CaptureGate::new(true);
    let (_event_tx, event_rx) = body_event_channel(1);
    let budget = event_rx.memory_budget();
    let make = |turn: &str| {
        let scope = CaptureScope {
            capture_generation: 1,
            owner_epoch: 1,
            owner_generation: 1,
            host_key: "h".into(),
            thread_key: "t".into(),
        };
        let frame = |revision, change| BodyFrame {
            scope: scope.clone(),
            revision,
            clock: CaptureClock {
                mono_ns: revision * 1_000_000,
                wall_ms: revision as i64,
            },
            changes: vec![change],
        };
        let mut o = BodyObserver::with_budget(budget.clone());
        o.on_frame(frame(1, BodyChange::Baseline));
        o.on_frame(frame(
            2,
            BodyChange::EmptyItem {
                turn_id: turn.into(),
                item_id: "i".into(),
                model: None,
                channel: None,
            },
        ));
        o.on_frame(frame(
            3,
            BodyChange::Append {
                turn_id: turn.into(),
                item_id: "i".into(),
                text: "a".repeat(2048),
            },
        ));
        o.on_frame(frame(
            4,
            BodyChange::Append {
                turn_id: turn.into(),
                item_id: "i".into(),
                text: "b".repeat(2048),
            },
        ));
        o.on_frame(frame(
            5,
            BodyChange::TurnTerminal {
                turn_id: turn.into(),
                completed: true,
            },
        ))
        .pop()
        .unwrap()
    };
    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let entered = Arc::new(Mutex::new(Some(entered_tx)));
    let latch = Arc::new((Mutex::new(false), Condvar::new()));
    let fired = Arc::new(AtomicBool::new(false));
    let hook: BeforeCommitHook = {
        let entered = entered.clone();
        let latch = latch.clone();
        let fired = fired.clone();
        Arc::new(move || {
            if !fired.swap(true, Ordering::SeqCst) {
                if let Some(tx) = entered.lock().unwrap().take() {
                    let _ = tx.send(());
                }
                let (lock, wake) = &*latch;
                let mut open = lock.lock().unwrap();
                while !*open {
                    open = wake.wait(open).unwrap();
                }
            }
        })
    };
    let runtime = BodyRuntime::new_with_gate(db.clone(), gate.clone(), Some(hook));
    let first = runtime
        .try_submit(
            make("first"),
            gate.permit().unwrap(),
            "run".into(),
            "build".into(),
        )
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(3), entered_rx)
        .await
        .unwrap()
        .unwrap();
    let mut pending = Vec::new();
    for i in 0..32 {
        pending.push(
            runtime
                .try_submit(
                    make(&format!("t{i}")),
                    gate.permit().unwrap(),
                    "run".into(),
                    "build".into(),
                )
                .expect("queue capacity"),
        );
    }
    assert!(runtime
        .try_submit(
            make("overflow"),
            gate.permit().unwrap(),
            "run".into(),
            "build".into()
        )
        .is_none());
    {
        let conn = db.lock().unwrap();
        set_estimate_enabled_with_gate(&conn, false, &gate).unwrap();
    }
    let result = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while budget.used_bytes() != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await;
    {
        let (lock, wake) = &*latch;
        *lock.lock().unwrap() = true;
        wake.notify_all();
    }
    assert_eq!(
        runtime.diagnostics().queue_full,
        1,
        "queue saturation must be diagnosed"
    );
    result.expect("disable must release queued plaintext while worker is blocked");
    assert!(!first.await.unwrap().unwrap());
    for result in pending {
        assert!(!result.await.unwrap().unwrap());
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 3)]
async fn disable_waits_for_active_plaintext_to_be_released() {
    use multi_agents_manager_lib::services::tps::{
        body_runtime::{BeforeCommitHook, BodyRuntime},
        estimate_store::set_estimate_enabled_with_gate,
    };
    use std::sync::{Arc, Condvar, Mutex};
    let conn = Connection::open_in_memory().unwrap();
    schema::init(&conn);
    migration::migrate(&conn).unwrap();
    let db = Arc::new(Mutex::new(conn));
    let gate = CaptureGate::new(true);
    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let entered = Arc::new(Mutex::new(Some(entered_tx)));
    let latch = Arc::new((Mutex::new(false), Condvar::new()));
    let hook: BeforeCommitHook = {
        let entered = entered.clone();
        let latch = latch.clone();
        Arc::new(move || {
            if let Some(tx) = entered.lock().unwrap().take() {
                let _ = tx.send(());
            }
            let (lock, wake) = &*latch;
            let mut open = lock.lock().unwrap();
            while !*open {
                open = wake.wait(open).unwrap();
            }
        })
    };
    let runtime = BodyRuntime::new_with_test_hooks(db.clone(), gate.clone(), Some(hook), None);
    let result = runtime
        .try_submit(
            finished_body("inflight"),
            gate.permit().unwrap(),
            "run".into(),
            "build".into(),
        )
        .unwrap();
    let entered_result = tokio::time::timeout(std::time::Duration::from_secs(3), entered_rx).await;
    if entered_result.is_err() {
        let (lock, wake) = &*latch;
        *lock.lock().unwrap() = true;
        wake.notify_all();
    }
    entered_result
        .expect("worker must enter tokenizer gate")
        .unwrap();
    let db2 = db.clone();
    let gate2 = gate.clone();
    let mut stop = tokio::task::spawn_blocking(move || {
        let conn = db2.lock().unwrap();
        set_estimate_enabled_with_gate(&conn, false, &gate2)
    });
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(100), &mut stop)
            .await
            .is_err(),
        "stop must wait for active plaintext"
    );
    {
        let (lock, wake) = &*latch;
        *lock.lock().unwrap() = true;
        wake.notify_all();
    }
    tokio::time::timeout(std::time::Duration::from_secs(3), stop)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(!result.await.unwrap().unwrap());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn database_retry_uses_only_redacted_fact_and_is_bounded() {
    use multi_agents_manager_lib::services::{
        tps::{
            body_runtime::{BodyRuntime, CommitSink},
            estimate_store::commit_estimate,
        },
        usage::error::UsageError,
    };
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    };
    let conn = Connection::open_in_memory().unwrap();
    schema::init(&conn);
    migration::migrate(&conn).unwrap();
    let db = Arc::new(Mutex::new(conn));
    let attempts = Arc::new(AtomicUsize::new(0));
    let sink: CommitSink = {
        let db = db.clone();
        let attempts = attempts.clone();
        Arc::new(move |fact, permit| {
            if attempts.fetch_add(1, Ordering::SeqCst) < 2 {
                return Err(UsageError::new("usage-db-failed", "transient"));
            }
            commit_estimate(&mut db.lock().unwrap(), fact, permit)
        })
    };
    let gate = CaptureGate::new(true);
    let runtime = BodyRuntime::new_with_test_sink(sink, gate.clone());
    let result = runtime
        .try_submit(
            finished_body("retry"),
            gate.permit().unwrap(),
            "run".into(),
            "build".into(),
        )
        .unwrap();
    assert!(
        tokio::time::timeout(std::time::Duration::from_secs(3), result)
            .await
            .unwrap()
            .unwrap()
            .unwrap()
    );
    assert_eq!(attempts.load(Ordering::SeqCst), 3);
    assert_eq!(
        db.lock()
            .unwrap()
            .query_row("SELECT count(*) FROM tps_body_estimate", [], |r| r
                .get::<_, i64>(0))
            .unwrap(),
        1
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn late_tokenization_cannot_cross_saved_source_disable_reenable() {
    use multi_agents_manager_lib::services::tps::{
        body_runtime::{BeforeCommitHook, BodyRuntime},
        estimate_store::set_estimate_enabled_with_gate,
    };
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Condvar, Mutex,
    };
    let conn = Connection::open_in_memory().unwrap();
    schema::init(&conn);
    migration::migrate(&conn).unwrap();
    let db = Arc::new(Mutex::new(conn));
    let gate = CaptureGate::new(true);
    let old = gate.permit().unwrap();
    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let entered = Arc::new(Mutex::new(Some(entered_tx)));
    let latch = Arc::new((Mutex::new(false), Condvar::new()));
    let fired = Arc::new(AtomicBool::new(false));
    let hook: BeforeCommitHook = {
        let entered = entered.clone();
        let latch = latch.clone();
        let fired = fired.clone();
        Arc::new(move || {
            if !fired.swap(true, Ordering::SeqCst) {
                if let Some(tx) = entered.lock().unwrap().take() {
                    let _ = tx.send(());
                }
                let (lock, wake) = &*latch;
                let mut open = lock.lock().unwrap();
                while !*open {
                    open = wake.wait(open).unwrap();
                }
            }
        })
    };
    let runtime = BodyRuntime::new_with_gate(db.clone(), gate.clone(), Some(hook));
    let result = runtime
        .try_submit(
            finished_body("old-source"),
            old,
            "run".into(),
            "build".into(),
        )
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(3), entered_rx)
        .await
        .unwrap()
        .unwrap();
    {
        let conn = db.lock().unwrap();
        set_estimate_enabled_with_gate(&conn, false, &gate).unwrap();
        set_estimate_enabled_with_gate(&conn, true, &gate).unwrap();
    }
    {
        let (lock, wake) = &*latch;
        *lock.lock().unwrap() = true;
        wake.notify_all();
    }
    assert!(!result.await.unwrap().unwrap());
    let fresh = runtime
        .try_submit(
            finished_body("new-source"),
            gate.permit().unwrap(),
            "run".into(),
            "build".into(),
        )
        .unwrap();
    assert!(fresh.await.unwrap().unwrap());
    assert_eq!(
        db.lock()
            .unwrap()
            .query_row("SELECT count(*) FROM tps_body_estimate", [], |r| r
                .get::<_, i64>(0))
            .unwrap(),
        1
    );
}

#[test]
fn ending_capture_invalidates_old_jobs_without_disabling_future_capture() {
    let mut conn = Connection::open_in_memory().unwrap();
    schema::init(&conn);
    migration::migrate(&conn).unwrap();
    let gate = CaptureGate::new(true);
    let old = gate.permit().unwrap();
    gate.invalidate_capture();
    assert!(!commit_estimate(&mut conn, &fact("old-run"), &old).unwrap());
    assert!(commit_estimate(&mut conn, &fact("new-run"), &gate.permit().unwrap()).unwrap());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_waits_for_inflight_job_and_rejects_old_commit() {
    use multi_agents_manager_lib::services::tps::body_runtime::{BeforeCommitHook, BodyRuntime};
    use std::sync::{Arc, Condvar, Mutex};
    let conn = Connection::open_in_memory().unwrap();
    schema::init(&conn);
    migration::migrate(&conn).unwrap();
    let db = Arc::new(Mutex::new(conn));
    let gate = CaptureGate::new(true);
    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let entered = Arc::new(Mutex::new(Some(entered_tx)));
    let latch = Arc::new((Mutex::new(false), Condvar::new()));
    let hook: BeforeCommitHook = {
        let entered = entered.clone();
        let latch = latch.clone();
        Arc::new(move || {
            if let Some(tx) = entered.lock().unwrap().take() {
                let _ = tx.send(());
            }
            let (lock, wake) = &*latch;
            let mut open = lock.lock().unwrap();
            while !*open {
                open = wake.wait(open).unwrap();
            }
        })
    };
    let runtime = BodyRuntime::new_with_gate(db.clone(), gate.clone(), Some(hook));
    let result = runtime
        .try_submit(
            finished_body("shutdown"),
            gate.permit().unwrap(),
            "run".into(),
            "build".into(),
        )
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(3), entered_rx)
        .await
        .unwrap()
        .unwrap();
    gate.invalidate_capture();
    let mut stop = tokio::spawn(runtime.shutdown());
    let early = tokio::time::timeout(std::time::Duration::from_millis(100), &mut stop).await;
    let waited = early.is_err();
    {
        let (lock, wake) = &*latch;
        *lock.lock().unwrap() = true;
        wake.notify_all();
    }
    match early {
        Ok(result) => result.unwrap(),
        Err(_) => tokio::time::timeout(std::time::Duration::from_secs(3), stop)
            .await
            .unwrap()
            .unwrap(),
    };
    assert!(waited, "shutdown must await in-flight worker");
    assert!(!result.await.unwrap().unwrap());
    assert_eq!(
        db.lock()
            .unwrap()
            .query_row("SELECT count(*) FROM tps_body_estimate", [], |r| r
                .get::<_, i64>(0))
            .unwrap(),
        0
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn permanent_database_failure_is_bounded_and_diagnosed() {
    use multi_agents_manager_lib::services::{
        tps::body_runtime::{BodyRuntime, CommitSink},
        usage::error::UsageError,
    };
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };
    let attempts = Arc::new(AtomicUsize::new(0));
    let sink: CommitSink = {
        let attempts = attempts.clone();
        Arc::new(move |_, _| {
            attempts.fetch_add(1, Ordering::SeqCst);
            Err(UsageError::new("usage-db-failed", "synthetic"))
        })
    };
    let gate = CaptureGate::new(true);
    let runtime = BodyRuntime::new_with_test_sink(sink, gate.clone());
    let result = runtime
        .try_submit(
            finished_body("permanent-error"),
            gate.permit().unwrap(),
            "run".into(),
            "build".into(),
        )
        .unwrap();
    assert!(
        tokio::time::timeout(std::time::Duration::from_secs(3), result)
            .await
            .unwrap()
            .unwrap()
            .is_err()
    );
    assert_eq!(attempts.load(Ordering::SeqCst), 3);
    assert_eq!(runtime.diagnostics().database_failures, 1);
}
