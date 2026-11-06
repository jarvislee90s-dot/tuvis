//! 真实 SQLite 事务与租约提交栅栏的受控时序。
use super::super::types::ThreadKey;
use super::*;
use crate::database::dao::codex_interactions::{
    get_interaction_hints_conn, next_hint_epoch_conn, prepare_versioned_hints_conn,
    HintWriteOutcome, StoredInteractionHint,
};
use rusqlite::Connection;
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

fn authority() -> Arc<HintLeaseAuthority> {
    HintLeaseAuthority::new(OwnerLease {
        key: ThreadKey {
            host_id: "local".into(),
            thread_id: "t".into(),
        },
        client_id: "client".into(),
        epoch: 1,
        generation: 1,
    })
}
fn db() -> (Connection, i64) {
    let c = Connection::open_in_memory().unwrap();
    crate::database::schema::init(&c);
    crate::database::migration::migrate(&c).unwrap();
    let epoch = next_hint_epoch_conn(&c).unwrap();
    (c, epoch)
}
fn hint(epoch: i64) -> StoredInteractionHint {
    StoredInteractionHint {
        thread_id: "t".into(),
        source_turn_id: "turn".into(),
        source_item_id: "item".into(),
        question_index: 0,
        last_fact: "answered".into(),
        updated_at: 1,
        observation_epoch: Some(epoch),
        observation_seq: Some(1),
        proof_version: None,
        answer_turn_id: None,
        answer_item_id: None,
        answer_digest: None,
        evidence_state: None,
    }
}
fn recv<T>(rx: &mpsc::Receiver<T>) -> T {
    rx.recv_timeout(Duration::from_secs(10))
        .expect("测试线程超时")
}

#[test]
fn b1_sqlite_revoke_first_rolls_back_prepared_batch() {
    let (conn, epoch) = db();
    let a = authority();
    let permit = a.rotate_scope().unwrap();
    let batch = prepare_versioned_hints_conn(&conn, epoch, &[hint(epoch)]).unwrap();
    a.revoke_owner();
    assert_eq!(permit.lock().err(), Some(Fence::Owner));
    drop(batch);
    assert!(get_interaction_hints_conn(&conn, "t").unwrap().is_empty());
}

#[test]
fn b1_sqlite_commit_first_orders_revocation() {
    let (conn, epoch) = db();
    let a = authority();
    let permit = a.rotate_scope().unwrap();
    let batch = prepare_versioned_hints_conn(&conn, epoch, &[hint(epoch)]).unwrap();
    let guard = permit.lock().unwrap();
    let (start_tx, start_rx) = mpsc::channel();
    let (done_tx, done_rx) = mpsc::channel();
    let other = a.clone();
    let worker = std::thread::spawn(move || {
        start_tx.send(()).unwrap();
        other.revoke_owner();
        done_tx.send(()).unwrap();
    });
    recv(&start_rx);
    assert!(done_rx.try_recv().is_err());
    assert_eq!(
        batch.commit().unwrap(),
        vec![HintWriteOutcome::Applied(hint(epoch))]
    );
    drop(guard);
    recv(&done_rx);
    worker.join().unwrap();
    assert_eq!(permit.lock().err(), Some(Fence::Owner));
    assert_eq!(
        get_interaction_hints_conn(&conn, "t").unwrap(),
        vec![hint(epoch)]
    );
}

#[test]
fn b1_sqlite_db_queue_does_not_hold_gate() {
    let (conn, epoch) = db();
    let shared = Arc::new(Mutex::new(conn));
    let held = shared.lock().unwrap();
    let a = authority();
    let permit = a.rotate_scope().unwrap();
    let (start_tx, start_rx) = mpsc::channel();
    let (done_tx, done_rx) = mpsc::channel();
    let db_worker = shared.clone();
    let worker = std::thread::spawn(move || {
        start_tx.send(()).unwrap();
        let conn = db_worker.lock().unwrap();
        let batch = prepare_versioned_hints_conn(&conn, epoch, &[hint(epoch)]).unwrap();
        let fenced = permit.lock().err() == Some(Fence::Owner);
        drop(batch);
        done_tx.send(fenced).unwrap();
    });
    recv(&start_rx);
    a.revoke_owner();
    drop(held);
    assert!(recv(&done_rx));
    worker.join().unwrap();
    assert!(get_interaction_hints_conn(&shared.lock().unwrap(), "t")
        .unwrap()
        .is_empty());
}

#[test]
fn b1_sqlite_commit_failure_releases_gate_and_rolls_back() {
    let (conn, epoch) = db();
    conn.execute_batch("PRAGMA foreign_keys=ON; CREATE TABLE test_parent(id INTEGER PRIMARY KEY); CREATE TABLE test_child(parent_id INTEGER REFERENCES test_parent(id) DEFERRABLE INITIALLY DEFERRED); CREATE TRIGGER test_deferred AFTER INSERT ON codex_interaction_hints BEGIN INSERT INTO test_child(parent_id) VALUES(42); END;").unwrap();
    let a = authority();
    let permit = a.rotate_scope().unwrap();
    let batch = prepare_versioned_hints_conn(&conn, epoch, &[hint(epoch)]).unwrap();
    let guard = permit.lock().unwrap();
    assert!(batch.commit().is_err());
    drop(guard);
    a.revoke_owner();
    assert_eq!(permit.lock().err(), Some(Fence::Owner));
    assert!(get_interaction_hints_conn(&conn, "t").unwrap().is_empty());
}

#[test]
fn b1_sqlite_registry_revocations_fence_prepared_batch() {
    use super::super::owners::OwnerRegistry;
    for operation in ["bind", "invalidate_client", "release", "reset"] {
        let (conn, epoch) = db();
        let key = ThreadKey {
            host_id: "local".into(),
            thread_id: "t".into(),
        };
        let mut registry = OwnerRegistry::default();
        let lease = registry.bind(key.clone(), "client".into(), 1);
        let authority = registry.authority(&lease).unwrap();
        let permit = authority.rotate_scope().unwrap();
        let batch = prepare_versioned_hints_conn(&conn, epoch, &[hint(epoch)]).unwrap();
        let staged: i64 = conn
            .query_row("SELECT COUNT(*) FROM codex_interaction_hints", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(staged, 1, "{operation}：SQL 必须先完成");
        match operation {
            "bind" => {
                registry.bind(key.clone(), "replacement".into(), 1);
            }
            "invalidate_client" => registry.invalidate_client("client"),
            "release" => registry.release(&key),
            "reset" => registry.reset(2),
            _ => unreachable!(),
        }
        assert_eq!(permit.lock().err(), Some(Fence::Owner), "{operation}");
        drop(batch);
        assert!(
            get_interaction_hints_conn(&conn, "t").unwrap().is_empty(),
            "{operation}"
        );
        let stored_epoch: i64 = conn
            .query_row(
                "SELECT epoch FROM codex_hint_writer_state WHERE singleton=1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(stored_epoch, epoch, "{operation} 不应旋转 writer epoch");
    }
}

#[test]
fn b1_store_sqlite_commit_first_blocks_real_owner_revocation() {
    use super::super::async_question::{InteractionHint, InteractionHintStore};
    use super::super::hint_state::{HintVersion, VersionedHint, WriteOutcome};
    use crate::remote::codex_evidence::StoreInteractionHints;
    use crate::remote::pairing::DeviceStore;
    use rusqlite::{Error, ErrorCode};
    use std::sync::TryLockError;
    use std::time::Instant;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("commit-fence.db");
    let writer = Connection::open(&path).unwrap();
    let mode: String = writer
        .query_row("PRAGMA journal_mode=DELETE", [], |r| r.get(0))
        .unwrap();
    assert_eq!(mode.to_lowercase(), "delete");
    writer.busy_timeout(Duration::from_secs(30)).unwrap();
    crate::database::schema::init(&writer);
    crate::database::migration::migrate(&writer).unwrap();
    let epoch = next_hint_epoch_conn(&writer).unwrap();
    let reader = Connection::open(&path).unwrap();
    reader
        .execute_batch("BEGIN; SELECT count(*) FROM codex_interaction_hints;")
        .unwrap();
    let observer = Connection::open(&path).unwrap();
    observer.busy_timeout(Duration::ZERO).unwrap();
    let a = authority();
    let permit = a.rotate_scope().unwrap();
    let store = StoreInteractionHints::new(DeviceStore::Owned(Arc::new(Mutex::new(writer))));
    let fact = VersionedHint {
        hint: InteractionHint {
            thread_id: "t".into(),
            source_turn_id: "turn".into(),
            source_item_id: "item".into(),
            question_index: 0,
            last_fact: "answered".into(),
            witness: None,
            evidence_state: None,
        },
        version: HintVersion { epoch, sequence: 1 },
    };
    let worker_fact = fact.clone();
    let worker_permit = permit.clone();
    let worker =
        std::thread::spawn(move || store.write_batch(epoch, &[worker_fact], 1, &worker_permit));
    // DELETE journal 下，写事务持 RESERVED 时新读仍可进入；COMMIT 取得
    // PENDING 后新读才返回 BUSY。测试只在锁外观察，不在 gate 内注入钩子。
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut pending = false;
    while Instant::now() < deadline {
        let gate_held = matches!(a.gate.try_lock(), Err(TryLockError::WouldBlock));
        let new_read =
            observer.query_row("SELECT count(*) FROM codex_interaction_hints", [], |r| {
                r.get::<_, i64>(0)
            });
        if gate_held
            && matches!(new_read, Err(Error::SqliteFailure(ref e, _)) if e.code == ErrorCode::DatabaseBusy)
        {
            pending = true;
            break;
        }
        std::thread::yield_now();
    }
    // 无论观察结果如何都释放读锁并回收线程，避免测试失败时留下阻塞 worker。
    if !pending {
        reader.execute_batch("ROLLBACK").unwrap();
        let result = worker.join().unwrap();
        panic!("未能确定 gate 与 SQLite PENDING 同时成立；worker={result:?}");
    }
    let (start_tx, start_rx) = mpsc::channel();
    let (done_tx, done_rx) = mpsc::channel();
    let revoke_authority = a.clone();
    let revoke = std::thread::spawn(move || {
        start_tx.send(()).unwrap();
        revoke_authority.revoke_owner();
        done_tx.send(()).unwrap();
    });
    recv(&start_rx);
    let revoke_waiting = done_rx.try_recv().is_err();
    reader.execute_batch("ROLLBACK").unwrap();
    let result = worker.join().unwrap();
    recv(&done_rx);
    revoke.join().unwrap();
    assert!(revoke_waiting, "真实 COMMIT 未退出前撤销不可完成");
    assert_eq!(result.unwrap(), vec![WriteOutcome::Applied(fact)]);
    assert_eq!(permit.lock().err(), Some(Fence::Owner));
    assert_eq!(
        get_interaction_hints_conn(&observer, "t").unwrap()[0].last_fact,
        "answered"
    );
}

#[test]
fn b1_store_db_queue_allows_registry_revocation() {
    use super::super::async_question::{HintStoreError, InteractionHint, InteractionHintStore};
    use super::super::hint_state::{HintVersion, VersionedHint};
    use super::super::owners::OwnerRegistry;
    use crate::remote::codex_evidence::StoreInteractionHints;
    use crate::remote::pairing::DeviceStore;
    use std::sync::TryLockError;

    let (conn, epoch) = db();
    let db_lock = Arc::new(Mutex::new(conn));
    let held = db_lock.lock().unwrap();
    let store = StoreInteractionHints::new(DeviceStore::Owned(db_lock.clone()));
    let key = ThreadKey {
        host_id: "local".into(),
        thread_id: "t".into(),
    };
    let mut registry = OwnerRegistry::default();
    let lease = registry.bind(key, "client".into(), 1);
    let permit = registry.authority(&lease).unwrap().rotate_scope().unwrap();
    let worker_permit = permit.clone();
    let fact = VersionedHint {
        hint: InteractionHint {
            thread_id: "t".into(),
            source_turn_id: "turn".into(),
            source_item_id: "item".into(),
            question_index: 0,
            last_fact: "answered".into(),
            witness: None,
            evidence_state: None,
        },
        version: HintVersion { epoch, sequence: 1 },
    };
    let (start_tx, start_rx) = mpsc::channel();
    let (done_tx, done_rx) = mpsc::channel();
    let worker = std::thread::spawn(move || {
        start_tx.send(()).unwrap();
        let result = store.write_batch(epoch, &[fact], 1, &worker_permit);
        done_tx.send(result).unwrap();
    });
    recv(&start_rx);
    // 此信号证明 worker 已启动，锁由本线程持有；不能单凭它断言 worker
    // 已运行到 Mutex::lock 调用。
    assert!(matches!(db_lock.try_lock(), Err(TryLockError::WouldBlock)));
    assert!(done_rx.try_recv().is_err());
    registry.invalidate_client("client");
    assert_eq!(permit.lock().err(), Some(Fence::Owner));
    drop(held);
    assert_eq!(recv(&done_rx), Err(HintStoreError::OwnerFenced));
    worker.join().unwrap();
    assert!(get_interaction_hints_conn(&db_lock.lock().unwrap(), "t")
        .unwrap()
        .is_empty());
}

// 控制线程 panic/超时时先解除所有等待，再回收线程，避免阻塞测试进程。
struct ProbeCleanup {
    probe: Arc<super::super::hint_test_probe::TestProbe>,
    release_db: Arc<std::sync::atomic::AtomicBool>,
    threads: Vec<std::thread::JoinHandle<()>>,
}
impl Drop for ProbeCleanup {
    fn drop(&mut self) {
        use std::sync::atomic::Ordering;
        self.probe.abort();
        self.release_db.store(true, Ordering::Release);
        for thread in self.threads.drain(..) {
            let _ = thread.join();
        }
    }
}
fn wait_atomic(mut ready: impl FnMut() -> bool) {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while !ready() {
        assert!(std::time::Instant::now() < deadline, "等待受控测试点超时");
        std::thread::yield_now();
    }
}
fn versioned_hint(epoch: i64) -> super::super::hint_state::VersionedHint {
    use super::super::async_question::InteractionHint;
    use super::super::hint_state::{HintVersion, VersionedHint};
    VersionedHint {
        hint: InteractionHint {
            thread_id: "t".into(),
            source_turn_id: "turn".into(),
            source_item_id: "item".into(),
            question_index: 0,
            last_fact: "answered".into(),
            witness: None,
            evidence_state: None,
        },
        version: HintVersion { epoch, sequence: 1 },
    }
}

#[test]
fn b1_store_post_sql_registry_revocation_rolls_back() {
    use super::super::async_question::{HintStoreError, InteractionHintStore};
    use super::super::hint_test_probe::{TestPoint, TestProbe};
    use super::super::owners::OwnerRegistry;
    use crate::remote::codex_evidence::StoreInteractionHints;
    use crate::remote::pairing::DeviceStore;
    for operation in ["bind", "invalidate_client", "release", "reset"] {
        let (conn, epoch) = db();
        let mut original = hint(epoch);
        original.last_fact = "pending".into();
        original.updated_at = 17;
        conn.execute("INSERT INTO codex_interaction_hints (thread_id, source_turn_id, source_item_id, question_index, last_fact, updated_at, observation_epoch, observation_seq) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)", rusqlite::params![original.thread_id, original.source_turn_id, original.source_item_id, original.question_index, original.last_fact, original.updated_at, original.observation_epoch, original.observation_seq]).unwrap();
        let shared = Arc::new(Mutex::new(conn));
        let device = DeviceStore::Owned(shared.clone());
        let probe = Arc::new(TestProbe::new(TestPoint::AfterSqlPreparedBeforePermit));
        let store = StoreInteractionHints::with_probe(device, probe.clone());
        let key = ThreadKey {
            host_id: "local".into(),
            thread_id: "t".into(),
        };
        let mut registry = OwnerRegistry::default();
        let lease = registry.bind(key.clone(), "client".into(), 1);
        let authority = registry.authority(&lease).unwrap();
        let permit = authority.rotate_scope().unwrap();
        let worker_permit = permit.clone();
        let mut cleanup = ProbeCleanup {
            probe: probe.clone(),
            release_db: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            threads: vec![],
        };
        let (result_tx, result_rx) = mpsc::channel();
        cleanup.threads.push(std::thread::spawn(move || {
            let mut update = versioned_hint(epoch);
            update.version.sequence = 2;
            let mut inserted = versioned_hint(epoch);
            inserted.hint.source_item_id = "new-item".into();
            inserted.version.sequence = 3;
            let result = store.write_batch(epoch, &[update, inserted], 99, &worker_permit);
            let _ = result_tx.send(result);
        }));
        wait_atomic(|| probe.reached());
        assert!(
            permit.authority.gate.try_lock().is_ok(),
            "{operation}: SQL 后不得持有 gate"
        );
        let (revoked_tx, revoked_rx) = mpsc::channel();
        cleanup.threads.push(std::thread::spawn(move || {
            match operation {
                "bind" => {
                    registry.bind(key, "replacement".into(), 1);
                }
                "invalidate_client" => registry.invalidate_client("client"),
                "release" => registry.release(&key),
                "reset" => registry.reset(2),
                _ => unreachable!(),
            }
            let _ = revoked_tx.send(());
        }));
        recv(&revoked_rx);
        assert!(probe.resume(), "{operation}: probe did not resume");
        assert_eq!(
            recv(&result_rx),
            Err(HintStoreError::OwnerFenced),
            "{operation}"
        );
        let conn = shared.lock().unwrap();
        assert_eq!(
            get_interaction_hints_conn(&conn, "t").unwrap(),
            vec![original],
            "{operation}: 原行所有字段不变且新行不存在"
        );
        let actual_epoch: i64 = conn
            .query_row(
                "SELECT epoch FROM codex_hint_writer_state WHERE singleton=1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(actual_epoch, epoch, "{operation}");
    }
}

#[test]
fn b1_store_db_contention_precedes_gate() {
    use super::super::async_question::{HintStoreError, InteractionHintStore};
    use super::super::hint_test_probe::{TestPoint, TestProbe};
    use super::super::owners::OwnerRegistry;
    use crate::remote::codex_evidence::StoreInteractionHints;
    use crate::remote::pairing::DeviceStore;
    use std::sync::atomic::{AtomicBool, Ordering};
    let (conn, epoch) = db();
    let shared = Arc::new(Mutex::new(conn));
    let device = DeviceStore::Owned(shared.clone());
    let holder_device = device.clone();
    let probe = Arc::new(TestProbe::new(TestPoint::DbContended));
    let store = StoreInteractionHints::with_probe(device, probe.clone());
    let mut registry = OwnerRegistry::default();
    let key = ThreadKey {
        host_id: "local".into(),
        thread_id: "t".into(),
    };
    let lease = registry.bind(key, "client".into(), 1);
    let permit = registry.authority(&lease).unwrap().rotate_scope().unwrap();
    let holder_ready = Arc::new(AtomicBool::new(false));
    let release_db = Arc::new(AtomicBool::new(false));
    let mut cleanup = ProbeCleanup {
        probe: probe.clone(),
        release_db: release_db.clone(),
        threads: vec![],
    };
    let ready = holder_ready.clone();
    let release = release_db.clone();
    cleanup.threads.push(std::thread::spawn(move || {
        holder_device.with(|_| {
            ready.store(true, Ordering::Release);
            while !release.load(Ordering::Acquire) {
                std::thread::yield_now();
            }
        });
    }));
    wait_atomic(|| holder_ready.load(Ordering::Acquire));
    let worker_permit = permit.clone();
    let (result_tx, result_rx) = mpsc::channel();
    cleanup.threads.push(std::thread::spawn(move || {
        let result = store.write_batch(epoch, &[versioned_hint(epoch)], 1, &worker_permit);
        let _ = result_tx.send(result);
    }));
    wait_atomic(|| probe.reached());
    assert!(
        permit.authority.gate.try_lock().is_ok(),
        "DB 竞争时 worker 不得持 gate"
    );
    let (revoked_tx, revoked_rx) = mpsc::channel();
    cleanup.threads.push(std::thread::spawn(move || {
        registry.invalidate_client("client");
        let _ = revoked_tx.send(());
    }));
    recv(&revoked_rx);
    release_db.store(true, Ordering::Release);
    assert!(probe.resume());
    assert_eq!(recv(&result_rx), Err(HintStoreError::OwnerFenced));
    assert!(get_interaction_hints_conn(&shared.lock().unwrap(), "t")
        .unwrap()
        .is_empty());
}

#[test]
fn b1_store_post_sql_probe_abort_rolls_back() {
    use super::super::async_question::{HintStoreError, InteractionHintStore};
    use super::super::hint_test_probe::{TestPoint, TestProbe};
    use crate::remote::codex_evidence::StoreInteractionHints;
    use crate::remote::pairing::DeviceStore;
    let (conn, epoch) = db();
    let shared = Arc::new(Mutex::new(conn));
    let probe = Arc::new(TestProbe::new(TestPoint::AfterSqlPreparedBeforePermit));
    let store =
        StoreInteractionHints::with_probe(DeviceStore::Owned(shared.clone()), probe.clone());
    let authority = authority();
    let permit = authority.rotate_scope().unwrap();
    let worker_permit = permit.clone();
    let mut cleanup = ProbeCleanup {
        probe: probe.clone(),
        release_db: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        threads: vec![],
    };
    let (result_tx, result_rx) = mpsc::channel();
    cleanup.threads.push(std::thread::spawn(move || {
        let result = store.write_batch(epoch, &[versioned_hint(epoch)], 1, &worker_permit);
        let _ = result_tx.send(result);
    }));
    wait_atomic(|| probe.reached());
    probe.abort();
    assert_eq!(recv(&result_rx), Err(HintStoreError::Storage));
    assert!(permit.lock().is_ok());
    let conn = shared.lock().unwrap();
    assert!(get_interaction_hints_conn(&conn, "t").unwrap().is_empty());
    let actual_epoch: i64 = conn
        .query_row(
            "SELECT epoch FROM codex_hint_writer_state WHERE singleton=1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(actual_epoch, epoch);
}
