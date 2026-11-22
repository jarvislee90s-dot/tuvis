//! R17/R18：**Codex 逐能力证据的生产持久化接线**。
//!
//! 说明：能力表/证据表本身早已入库；本模块把它们接到生产 hub 装配上，使
//! 「契约识别 → 落账 → 重启复用（核对当前包）→ 真实成功」走真实存储，而不是只在
//! 测试里注入内存实现。

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use crate::database::dao::codex_interactions::StoredInteractionHint;
use crate::database::dao::codex_ipc_probe::StoredContractEvidence;
use crate::remote::pairing::DeviceStore;
use crate::services::codex_desktop_ipc::async_question::{InteractionHint, InteractionHintStore};
use crate::services::codex_desktop_ipc::capability::{
    CodexOperation, ContractEvidenceStore, DesktopContractEvidence, ObservedOperationsSource,
    ObservedTarget,
};

/// 契约证据：生产 DB（`codex_ipc_contract_evidence`）。
pub struct StoreContractEvidence {
    store: DeviceStore,
}

impl StoreContractEvidence {
    pub fn new(store: DeviceStore) -> Self {
        Self { store }
    }
}

impl ContractEvidenceStore for StoreContractEvidence {
    fn load(&self, probe_key: &str, rule_version: u32) -> Option<Vec<StoredContractEvidence>> {
        self.store.with(|conn| {
            crate::database::dao::codex_ipc_probe::get_contract_evidence_conn(
                conn,
                probe_key,
                rule_version,
            )
            .ok()
        })
    }

    fn store(&self, evidence: &DesktopContractEvidence, at_ms: i64) {
        let rows = evidence_rows(evidence, at_ms);
        self.store.with(|conn| {
            for row in &rows {
                if let Err(e) =
                    crate::database::dao::codex_ipc_probe::upsert_contract_evidence_conn(conn, row)
                {
                    log::warn!("codex contract evidence 落账失败（忽略，下次重识别）: {e}");
                }
            }
        });
    }
}

/// 证据 → 持久行（与读侧 `from_rows` 对称；单点，避免两处字段漂移）。
pub(crate) fn evidence_rows(
    evidence: &DesktopContractEvidence,
    at_ms: i64,
) -> Vec<StoredContractEvidence> {
    evidence
        .matches
        .iter()
        .map(|(operation, matched)| StoredContractEvidence {
            probe_key: evidence.probe_key.clone(),
            operation: operation.as_str().to_string(),
            rule_version: evidence.rule_version,
            package_sha256: evidence.package_sha256.clone(),
            method: matched.method.clone(),
            base_version: matched.base_version,
            request_rule: matched.request_rule.clone(),
            response_rule: matched.response_rule.clone(),
            semantic_rule: matched.semantic_rule.clone(),
            evidenced_at: at_ms,
            observed_at: at_ms,
            package_size: evidence.package_size.map(|v| v as i64),
            package_mtime_ms: evidence.package_mtime_ms,
        })
        .collect()
}

type ObservedCacheEntry = (
    crate::services::codex_desktop_ipc::capability::ObservedTarget,
    std::time::Instant,
    BTreeMap<CodexOperation, bool>,
);

/// 本机**真实成功**证据：`codex_operation_success`（按完整目标 + 规则版本）。
///
/// H01：不再从动作台账的 Accepted 行推导——Accepted 只是传输受理，不代表当前目标
/// 有正式效果；跨 build／包内容／规则版本一律不通用（旧行只保留审计）。
/// 缓存按 `probe_key` 区分（10s TTL）。
pub struct TargetObservedOperations {
    store: DeviceStore,
    cache: Mutex<Option<ObservedCacheEntry>>,
    /// A2.2/T8：后台刷新自引用（装配点 `Arc` 包装后 `arm_background_refresh` 注入；
    /// 未注入 = 同步刷新回退，保持既有同步测试行为）。
    self_ref: Mutex<Option<std::sync::Weak<TargetObservedOperations>>>,
    /// A2.2/T8：刷新完成且事实变化时的回调（变化 → 重发布；None = 不通知）。
    on_refreshed: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
    /// A2.2/T8：后台刷新在飞去重。
    refreshing: std::sync::atomic::AtomicBool,
    /// A2/M3：最近一次查询的目标（install 只接受它——换目标后旧目标的迟到
    /// 结果不得覆盖；新目标自己的刷新结果必须可安装）。
    request_state: Mutex<ObservedRequestState>,
}

/// 终审 M3：目标代次 + 待接续请求（与安装资格同一把锁）。
#[derive(Default)]
struct ObservedRequestState {
    target: Option<ObservedTarget>,
    generation: u64,
    pending: Option<ObservedTarget>,
}

const OBSERVED_TTL: std::time::Duration = std::time::Duration::from_secs(10);

impl TargetObservedOperations {
    pub fn new(store: DeviceStore) -> Self {
        Self {
            store,
            cache: Mutex::new(None),
            self_ref: Mutex::new(None),
            on_refreshed: Mutex::new(None),
            refreshing: std::sync::atomic::AtomicBool::new(false),
            request_state: Mutex::new(ObservedRequestState::default()),
        }
    }

    /// A2.2/T8：装配点在 `Arc` 包装后调用——后台刷新需要克隆自身 Arc；
    /// 回调在 adapter 装配完成后由 `set_on_refreshed` 注入。
    pub fn arm_background_refresh(self: &Arc<Self>) {
        *self.self_ref.lock().unwrap_or_else(|e| e.into_inner()) = Some(Arc::downgrade(self));
    }

    /// A2.2/T8：刷新完成且事实变化时的重发布回调（sync Fn；内部自行 spawn）。
    /// A2/M3：回调持 adapter **弱引用**（upgrade 失败即跳过）——不形成
    /// adapter → source → adapter 强引用环。
    pub fn set_on_refreshed(&self, callback: Option<Arc<dyn Fn() + Send + Sync>>) {
        *self.on_refreshed.lock().unwrap_or_else(|e| e.into_inner()) = callback;
    }

    fn install(
        &self,
        target: &crate::services::codex_desktop_ipc::capability::ObservedTarget,
        generation: u64,
        value: BTreeMap<CodexOperation, bool>,
    ) -> bool {
        // 终审 M3：目标代次/身份核对与 cache 写入在同一锁序内完成
        //（request_state → cache），换目标后旧结果无法越过窗口写入；
        // 目标身份也计入「变化」判定（目标换了即使 map 相同也要重发布）。
        let request = self.request_state.lock().unwrap_or_else(|e| e.into_inner());
        if request.generation != generation || request.target.as_ref() != Some(target) {
            return false;
        }
        let mut cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        let changed = cache
            .as_ref()
            .is_none_or(|(cached, _, v)| *cached != *target || *v != value);
        *cache = Some((target.clone(), std::time::Instant::now(), value));
        changed
    }

    fn spawn_background_refresh(self: &Arc<Self>, target: ObservedTarget, generation: u64) {
        // A2/M3：在飞去重由调用方（observed）的 swap 负责——本函数只派发任务。
        let job = self.clone();
        tokio::spawn(async move {
            let value = {
                let job_read = job.clone();
                let target_read = target.clone();
                tokio::task::spawn_blocking(move || job_read.refresh(&target_read))
                    .await
                    .ok()
            };
            if let Some(value) = value {
                let changed = job.install(&target, generation, value);
                if changed {
                    if let Some(callback) = job
                        .on_refreshed
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .clone()
                    {
                        callback();
                    }
                }
            }
            // 终审 M3：单飞期间登记的新目标在此接续（不丢失最新请求）。
            let pending = {
                let mut request = job.request_state.lock().unwrap_or_else(|e| e.into_inner());
                match request.pending.take() {
                    Some(next) => (Some(next), request.generation),
                    None => {
                        job.refreshing
                            .store(false, std::sync::atomic::Ordering::Release);
                        (None, request.generation)
                    }
                }
            };
            if let (Some(next), generation) = pending {
                job.spawn_background_refresh(next, generation);
            }
        });
    }

    /// 终审 M3：按当前请求状态发起（必要时接续的）后台刷新。
    fn request_refresh(self: &Arc<Self>, target: ObservedTarget) {
        if self
            .refreshing
            .swap(true, std::sync::atomic::Ordering::AcqRel)
        {
            // 单飞中：登记为待接续（同一状态锁）。
            let mut request = self.request_state.lock().unwrap_or_else(|e| e.into_inner());
            request.pending = Some(target);
            return;
        }
        let generation = {
            let request = self.request_state.lock().unwrap_or_else(|e| e.into_inner());
            request.generation
        };
        self.spawn_background_refresh(target, generation);
    }

    fn refresh(
        &self,
        target: &crate::services::codex_desktop_ipc::capability::ObservedTarget,
    ) -> BTreeMap<CodexOperation, bool> {
        let operations: Vec<String> = self
            .store
            .with(|conn| {
                crate::database::dao::codex_ipc_probe::get_operation_success_conn(
                    conn,
                    &target.probe_key,
                    &target.package_sha256,
                    crate::services::codex_desktop_ipc::capability::CONTRACT_RULE_VERSION,
                )
                .ok()
            })
            .unwrap_or_default();
        let mut observed = BTreeMap::new();
        for word in operations {
            if let Some(operation) =
                crate::services::codex_desktop_ipc::capability::CodexOperation::parse(&word)
            {
                observed.insert(operation, true);
            }
        }
        observed
    }
}

impl ObservedOperationsSource for TargetObservedOperations {
    fn observed(&self, target: Option<&ObservedTarget>) -> BTreeMap<CodexOperation, bool> {
        // 没有完整目标（缺 probe key 或包 SHA）= 无证据（不猜）。
        let Some(target) = target
            .filter(|target| !target.probe_key.is_empty() && !target.package_sha256.is_empty())
        else {
            // 终审 Major 3：切到无目标也推进代次（旧目标迟到结果不得安装）。
            let mut request = self.request_state.lock().unwrap_or_else(|e| e.into_inner());
            if request.target.is_some() {
                request.generation = request.generation.wrapping_add(1);
                request.target = None;
                request.pending = None;
            }
            return BTreeMap::new();
        };
        // 终审 M3：登记目标 + 推进代次（install 的授权基准，同一把锁）。
        let generation = {
            let mut request = self.request_state.lock().unwrap_or_else(|e| e.into_inner());
            if request.target.as_ref() != Some(target) {
                request.generation = request.generation.wrapping_add(1);
                request.target = Some(target.clone());
            }
            request.generation
        };
        let cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((cached, at, value)) = cache.as_ref() {
            if cached == target && at.elapsed() < OBSERVED_TTL {
                return value.clone();
            }
        }
        // A2.2/T8：TTL 过期/无缓存 → 有缓存值就如实返回旧事实（只读成功历史，
        // 迟到只意味着少报新成功，保守方向），并触发后台刷新——镜像临界区内
        // 不再等待慢存储。无自引用（同步测试上下文/未装配）保持原同步刷新。
        let stale = cache
            .as_ref()
            .filter(|(cached, _, _)| *cached == *target)
            .map(|(_, _, value)| value.clone());
        drop(cache);
        let armed = self
            .self_ref
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .and_then(|weak| weak.upgrade());
        match armed {
            Some(self_arc) => {
                // 终审 M3：仅运行时在位时后台刷新；无运行时（同步测试）就地
                // 同步刷新并放回去重旗标（回调经 safe_notify 调度，不裸 spawn）。
                match tokio::runtime::Handle::try_current() {
                    Ok(_) => self_arc.request_refresh(target.clone()),
                    Err(_) => {
                        if !self_arc
                            .refreshing
                            .swap(true, std::sync::atomic::Ordering::AcqRel)
                        {
                            self_arc
                                .refreshing
                                .store(false, std::sync::atomic::Ordering::Release);
                            let value = self.refresh(target);
                            let changed = self.install(target, generation, value.clone());
                            if changed {
                                if let Some(callback) = self
                                    .on_refreshed
                                    .lock()
                                    .unwrap_or_else(|e| e.into_inner())
                                    .clone()
                                {
                                    callback();
                                }
                            }
                            return value;
                        }
                    }
                }
                stale.unwrap_or_default()
            }
            None => {
                let value = self.refresh(target);
                // 同步回退同样经 install（目标代次/身份与写入同锁序）。
                let _ = self.install(target, generation, value.clone());
                value
            }
        }
    }
}

/// H01：把一条**已确认效果**的成功证据写入当前目标（幂等；无目标 → 不写）。
pub fn record_operation_success(
    store: &DeviceStore,
    probe_key: &str,
    package_sha256: &str,
    operation: &str,
) {
    if probe_key.is_empty() || package_sha256.is_empty() {
        return;
    }
    store.with(|conn| {
        if let Err(e) = crate::database::dao::codex_ipc_probe::record_operation_success_conn(
            conn,
            probe_key,
            package_sha256,
            operation,
            crate::services::codex_desktop_ipc::capability::CONTRACT_RULE_VERSION,
            chrono::Utc::now().timestamp_millis(),
        ) {
            log::warn!("codex operation success 落账失败（忽略）: {e}");
        }
    });
}

/// 异步题持久提示：生产 DB（`codex_interaction_hints`；无正文）。
pub struct StoreInteractionHints {
    store: DeviceStore,
    #[cfg(test)]
    probe: Option<std::sync::Arc<crate::services::codex_desktop_ipc::hint_test_probe::TestProbe>>,
}

impl StoreInteractionHints {
    pub fn new(store: DeviceStore) -> Self {
        Self {
            store,
            #[cfg(test)]
            probe: None,
        }
    }

    #[cfg(test)]
    pub(crate) fn with_probe(
        store: DeviceStore,
        probe: std::sync::Arc<crate::services::codex_desktop_ipc::hint_test_probe::TestProbe>,
    ) -> Self {
        Self {
            store,
            probe: Some(probe),
        }
    }
}

impl InteractionHintStore for StoreInteractionHints {
    fn load(&self, thread_id: &str) -> Option<Vec<StoredInteractionHint>> {
        self.store.with(|conn| {
            crate::database::dao::codex_interactions::get_interaction_hints_conn(conn, thread_id)
                .ok()
        })
    }

    fn begin_epoch(
        &self,
    ) -> Result<i64, crate::services::codex_desktop_ipc::async_question::HintStoreError> {
        self.store
            .with(crate::database::dao::codex_interactions::next_hint_epoch_conn)
            .map_err(|error| match error {
                // 单例缺失、损坏或 epoch 已耗尽都不会因定时重试自行恢复。
                rusqlite::Error::QueryReturnedNoRows => {
                    crate::services::codex_desktop_ipc::async_question::HintStoreError::InvalidData
                }
                _ => crate::services::codex_desktop_ipc::async_question::HintStoreError::Storage,
            })
    }

    fn write_batch(
        &self,
        epoch: i64,
        hints: &[crate::services::codex_desktop_ipc::hint_state::VersionedHint],
        at_ms: i64,
        permit: &crate::services::codex_desktop_ipc::hint_commit::HintCommitPermit,
    ) -> Result<
        crate::services::codex_desktop_ipc::async_question::BatchWriteReport,
        crate::services::codex_desktop_ipc::async_question::HintStoreError,
    > {
        use crate::database::dao::codex_interactions::{
            prepare_versioned_hints_conn, HintWriteError, HintWriteOutcome,
        };
        use crate::services::codex_desktop_ipc::async_question::HintStoreError;
        use crate::services::codex_desktop_ipc::hint_state::{
            HintVersion, VersionedHint, WriteOutcome,
        };
        if hints
            .iter()
            .any(|h| h.hint.thread_id != permit.lease().key.thread_id)
        {
            return Err(HintStoreError::Fenced);
        }
        let rows: Vec<_> = hints
            .iter()
            .map(|h| {
                h.hint
                    .to_stored(at_ms, Some(h.version.epoch), Some(h.version.sequence))
            })
            .collect();
        let map_error = |e| match e {
            HintWriteError::Fenced => HintStoreError::Fenced,
            HintWriteError::VersionCollision => HintStoreError::VersionCollision,
            HintWriteError::InvalidVersion => HintStoreError::InvalidData,
            HintWriteError::Storage(rusqlite::Error::InvalidColumnType(..)) => {
                HintStoreError::InvalidData
            }
            HintWriteError::Storage(_) => HintStoreError::Storage,
        };
        let write = |conn: &rusqlite::Connection| {
            let batch = prepare_versioned_hints_conn(conn, epoch, &rows).map_err(map_error)?;
            #[cfg(test)]
            if let Some(probe) = &self.probe {
                probe.hit(crate::services::codex_desktop_ipc::hint_test_probe::TestPoint::AfterSqlPreparedBeforePermit)
                    .map_err(|_| HintStoreError::Storage)?;
            }
            let _guard = permit.lock().map_err(|reason| match reason {
                crate::services::codex_desktop_ipc::hint_commit::Fence::Owner => {
                    HintStoreError::OwnerFenced
                }
                crate::services::codex_desktop_ipc::hint_commit::Fence::Scope => {
                    HintStoreError::ScopeFenced
                }
            })?;
            batch.commit().map_err(map_error)
        };
        #[cfg(test)]
        let report = self
            .store
            .with_probe(self.probe.as_deref(), write)
            .map_err(|_| HintStoreError::Storage)??;
        #[cfg(not(test))]
        let report = self.store.with(write)?;
        Ok(report
            .into_iter()
            .map(|outcome| {
                let convert = |row: StoredInteractionHint| VersionedHint {
                    version: HintVersion {
                        epoch: row.observation_epoch.expect("versioned report"),
                        sequence: row.observation_seq.expect("versioned report"),
                    },
                    hint: InteractionHint::from_stored(row),
                };
                match outcome {
                    HintWriteOutcome::Applied(row) => WriteOutcome::Applied(convert(row)),
                    HintWriteOutcome::AlreadyCurrent(row) => {
                        WriteOutcome::AlreadyCurrent(convert(row))
                    }
                    HintWriteOutcome::Superseded(row) => WriteOutcome::Superseded(convert(row)),
                }
            })
            .collect())
    }

    #[cfg(test)]
    fn upsert(&self, hint: &InteractionHint, at_ms: i64) {
        let row = StoredInteractionHint {
            thread_id: hint.thread_id.clone(),
            source_turn_id: hint.source_turn_id.clone(),
            source_item_id: hint.source_item_id.clone(),
            question_index: hint.question_index,
            last_fact: hint.last_fact.clone(),
            updated_at: at_ms,
            observation_epoch: None,
            observation_seq: None,
            proof_version: None,
            answer_turn_id: None,
            answer_item_id: None,
            answer_digest: None,
            evidence_state: None,
        };
        self.store.with(|conn| {
            if let Err(e) =
                crate::database::dao::codex_interactions::upsert_interaction_hint_conn(conn, &row)
            {
                log::warn!("codex interaction hint 落账失败（忽略）: {e}");
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::codex_desktop_ipc::capability::{ContractMatch, CONTRACT_RULE_VERSION};
    use std::sync::{Arc, Mutex};

    fn store() -> DeviceStore {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        crate::database::schema::init(&conn);
        crate::database::migration::migrate(&conn).unwrap();
        DeviceStore::Owned(Arc::new(Mutex::new(conn)))
    }

    #[test]
    fn b1_store_batch_reports_and_fences() {
        use crate::services::codex_desktop_ipc::hint_state::{
            HintVersion, VersionedHint, WriteOutcome,
        };
        let db = store();
        let ledger = StoreInteractionHints::new(db);
        let authority = crate::services::codex_desktop_ipc::hint_commit::HintLeaseAuthority::new(
            crate::services::codex_desktop_ipc::types::OwnerLease {
                key: crate::services::codex_desktop_ipc::types::ThreadKey {
                    host_id: "h".into(),
                    thread_id: "t".into(),
                },
                client_id: "c".into(),
                epoch: 1,
                generation: 1,
            },
        );
        let permit = authority.rotate_scope().unwrap();
        let epoch = ledger.begin_epoch().unwrap();
        let hint = VersionedHint {
            hint: InteractionHint {
                thread_id: "t".into(),
                source_turn_id: "turn".into(),
                source_item_id: "item".into(),
                question_index: 0,
                last_fact: "pending".into(),
                witness: None,
                evidence_state: None,
            },
            version: HintVersion { epoch, sequence: 1 },
        };
        assert_eq!(
            ledger
                .write_batch(epoch, std::slice::from_ref(&hint), 5, &permit)
                .unwrap(),
            vec![WriteOutcome::Applied(hint.clone())]
        );
        assert_eq!(
            ledger
                .write_batch(epoch, std::slice::from_ref(&hint), 6, &permit)
                .unwrap(),
            vec![WriteOutcome::AlreadyCurrent(hint.clone())]
        );
        ledger.begin_epoch().unwrap();
        assert_eq!(
            ledger.write_batch(epoch, &[hint], 7, &permit),
            Err(crate::services::codex_desktop_ipc::async_question::HintStoreError::Fenced)
        );
    }

    #[test]
    fn b1_hint_batch_real_lock_retry_is_idempotent() {
        use crate::services::codex_desktop_ipc::{
            async_question::HintStoreError,
            hint_commit::HintLeaseAuthority,
            hint_state::{HintVersion, VersionedHint, WriteOutcome},
            types::{OwnerLease, ThreadKey},
        };
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("isolated-hints.sqlite");
        let first = rusqlite::Connection::open(&path).unwrap();
        crate::database::schema::init(&first);
        crate::database::migration::migrate(&first).unwrap();
        let second = rusqlite::Connection::open(&path).unwrap();
        second.busy_timeout(std::time::Duration::ZERO).unwrap();
        let shared = Arc::new(Mutex::new(second));
        let ledger = StoreInteractionHints::new(DeviceStore::Owned(shared.clone()));
        let epoch = ledger.begin_epoch().unwrap();
        let authority = HintLeaseAuthority::new(OwnerLease {
            key: ThreadKey {
                host_id: "h".into(),
                thread_id: "t".into(),
            },
            client_id: "c".into(),
            epoch: 1,
            generation: 1,
        });
        let permit = authority.rotate_scope().unwrap();
        let hint = VersionedHint {
            hint: InteractionHint {
                thread_id: "t".into(),
                source_turn_id: "turn".into(),
                source_item_id: "item".into(),
                question_index: 0,
                last_fact: "unknown".into(),
                witness: None,
                evidence_state: None,
            },
            version: HintVersion { epoch, sequence: 1 },
        };
        first.execute_batch("BEGIN IMMEDIATE").unwrap();
        let lock_error = shared
            .lock()
            .unwrap()
            .execute_batch("BEGIN IMMEDIATE")
            .unwrap_err();
        assert!(
            matches!(lock_error, rusqlite::Error::SqliteFailure(ref error, _) if error.code == rusqlite::ErrorCode::DatabaseBusy)
        );
        let blocked = ledger.write_batch(epoch, std::slice::from_ref(&hint), 1, &permit);
        first.execute_batch("ROLLBACK").unwrap();
        assert_eq!(blocked, Err(HintStoreError::Storage));
        assert!(ledger.load("t").unwrap().is_empty());
        assert_eq!(
            ledger
                .write_batch(epoch, std::slice::from_ref(&hint), 2, &permit)
                .unwrap(),
            vec![WriteOutcome::Applied(hint.clone())]
        );
        assert_eq!(
            ledger
                .write_batch(epoch, std::slice::from_ref(&hint), 3, &permit)
                .unwrap(),
            vec![WriteOutcome::AlreadyCurrent(hint)]
        );
        let rows = ledger.load("t").unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].last_fact, "unknown");
        assert_eq!(
            rows[0].updated_at, 2,
            "idempotent repeat must not rewrite timestamp"
        );
        assert_eq!(rows[0].observation_epoch, Some(epoch));
        assert_eq!(rows[0].observation_seq, Some(1));
    }

    #[test]
    fn b1_store_latest_invalidation_survives_lock_and_late_retry() {
        use crate::services::codex_desktop_ipc::{
            async_question::HintStoreError,
            hint_commit::HintLeaseAuthority,
            hint_state::{HintVersion, VersionedHint, WriteOutcome},
            types::{OwnerLease, ThreadKey},
        };
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("isolated-invalidation.sqlite");
        let first = rusqlite::Connection::open(&path).unwrap();
        crate::database::schema::init(&first);
        crate::database::migration::migrate(&first).unwrap();
        let second = rusqlite::Connection::open(&path).unwrap();
        second.busy_timeout(std::time::Duration::ZERO).unwrap();
        let ledger = StoreInteractionHints::new(DeviceStore::Owned(Arc::new(Mutex::new(second))));
        let epoch = ledger.begin_epoch().unwrap();
        let authority = HintLeaseAuthority::new(OwnerLease {
            key: ThreadKey {
                host_id: "h".into(),
                thread_id: "t".into(),
            },
            client_id: "c".into(),
            epoch: 1,
            generation: 1,
        });
        let permit = authority.rotate_scope().unwrap();
        let answered = VersionedHint {
            hint: InteractionHint {
                thread_id: "t".into(),
                source_turn_id: "turn".into(),
                source_item_id: "item".into(),
                question_index: 0,
                last_fact: "answered".into(),
                witness: None,
                evidence_state: None,
            },
            version: HintVersion {
                epoch,
                sequence: 10,
            },
        };
        let mut unknown = answered.clone();
        unknown.hint.last_fact = "unknown".into();
        unknown.version.sequence = 11;
        assert_eq!(
            ledger
                .write_batch(epoch, std::slice::from_ref(&answered), 1, &permit)
                .unwrap(),
            vec![WriteOutcome::Applied(answered.clone())]
        );
        first.execute_batch("BEGIN IMMEDIATE").unwrap();
        let blocked = ledger.write_batch(epoch, &[unknown.clone()], 2, &permit);
        first.execute_batch("ROLLBACK").unwrap();
        assert_eq!(blocked, Err(HintStoreError::Storage));
        let before_retry = ledger.load("t").unwrap();
        assert_eq!(before_retry.len(), 1);
        assert_eq!(before_retry[0].last_fact, "answered");
        assert_eq!(before_retry[0].observation_seq, Some(10));
        assert_eq!(
            ledger
                .write_batch(epoch, &[unknown.clone()], 3, &permit)
                .unwrap(),
            vec![WriteOutcome::Applied(unknown.clone())]
        );
        let late = ledger.write_batch(epoch, &[answered], 4, &permit).unwrap();
        assert!(
            matches!(late.as_slice(), [WriteOutcome::Superseded(row)] if row.hint.last_fact == "unknown" && row.version == unknown.version)
        );
        let rows = ledger.load("t").unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].last_fact, "unknown");
        assert_eq!(rows[0].observation_seq, Some(11));
    }

    #[test]
    fn b1_epoch_real_sqlite_lock_is_storage_and_retryable_after_release() {
        use crate::services::codex_desktop_ipc::async_question::HintStoreError;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("isolated.sqlite");
        let first = rusqlite::Connection::open(&path).unwrap();
        crate::database::schema::init(&first);
        crate::database::migration::migrate(&first).unwrap();
        let second = rusqlite::Connection::open(&path).unwrap();
        second.busy_timeout(std::time::Duration::ZERO).unwrap();
        let ledger = StoreInteractionHints::new(DeviceStore::Owned(Arc::new(Mutex::new(second))));
        first.execute_batch("BEGIN IMMEDIATE").unwrap();
        let while_locked = ledger.begin_epoch();
        first.execute_batch("ROLLBACK").unwrap();
        assert_eq!(while_locked, Err(HintStoreError::Storage));
        assert_eq!(
            ledger.begin_epoch(),
            Ok(1),
            "release allows a fresh local attempt"
        );
    }

    #[test]
    fn b1_epoch_invalid_state_is_not_transient_storage_failure() {
        use crate::services::codex_desktop_ipc::async_question::HintStoreError;
        for sql in [
            "UPDATE codex_hint_writer_state SET epoch=9223372036854775807 WHERE singleton=1",
            "DELETE FROM codex_hint_writer_state WHERE singleton=1",
        ] {
            let db = store();
            db.with(|conn| conn.execute_batch(sql).unwrap());
            let ledger = StoreInteractionHints::new(db.clone());
            assert_eq!(
                ledger.begin_epoch(),
                Err(HintStoreError::InvalidData),
                "invalid writer state must not be treated as a transient DB error"
            );
            let rows: i64 = db.with(|conn| {
                conn.query_row("SELECT count(*) FROM codex_interaction_hints", [], |row| {
                    row.get(0)
                })
                .unwrap()
            });
            assert_eq!(rows, 0);
        }
        let db = store();
        db.with(|conn| {
            conn.execute_batch("DROP TABLE codex_hint_writer_state")
                .unwrap()
        });
        let ledger = StoreInteractionHints::new(db);
        assert_eq!(
            ledger.begin_epoch(),
            Err(HintStoreError::Storage),
            "SQL failure must remain distinguishable from invalid singleton state"
        );
    }

    #[test]
    fn b1_store_malformed_persisted_value_is_not_retryable_storage_error() {
        use crate::services::codex_desktop_ipc::{
            async_question::HintStoreError,
            hint_commit::HintLeaseAuthority,
            hint_state::{HintVersion, VersionedHint},
            types::{OwnerLease, ThreadKey},
        };
        let db = store();
        let ledger = StoreInteractionHints::new(db.clone());
        let epoch = ledger.begin_epoch().unwrap();
        db.with(|conn| conn.execute_batch("INSERT INTO codex_interaction_hints(thread_id,source_turn_id,source_item_id,question_index,last_fact,updated_at) VALUES('t','turn','broken',0,'pending','not-an-integer')").unwrap());
        let authority = HintLeaseAuthority::new(OwnerLease {
            key: ThreadKey {
                host_id: "h".into(),
                thread_id: "t".into(),
            },
            client_id: "c".into(),
            epoch: 1,
            generation: 1,
        });
        let permit = authority.rotate_scope().unwrap();
        let make_hint = |item: &str, sequence| VersionedHint {
            hint: InteractionHint {
                thread_id: "t".into(),
                source_turn_id: "turn".into(),
                source_item_id: item.into(),
                question_index: 0,
                last_fact: "unknown".into(),
                witness: None,
                evidence_state: None,
            },
            version: HintVersion { epoch, sequence },
        };
        let result = ledger.write_batch(
            epoch,
            &[make_hint("good", 1), make_hint("broken", 2)],
            1,
            &permit,
        );
        assert_eq!(
            result,
            Err(HintStoreError::InvalidData),
            "malformed stored type must not be retried as transient storage failure"
        );
        let (count, original): (i64, String) = db.with(|conn| {
            let count = conn
                .query_row("SELECT count(*) FROM codex_interaction_hints", [], |row| {
                    row.get(0)
                })
                .unwrap();
            let original = conn
                .query_row(
                    "SELECT updated_at FROM codex_interaction_hints WHERE source_item_id='broken'",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            (count, original)
        });
        assert_eq!(count, 1, "the earlier staged row must roll back");
        assert_eq!(
            original, "not-an-integer",
            "invalid legacy data must not be enhanced or overwritten"
        );
    }

    #[test]
    fn b1_store_invalid_version_is_not_transient_storage_failure() {
        use crate::services::codex_desktop_ipc::{
            async_question::HintStoreError,
            hint_commit::HintLeaseAuthority,
            hint_state::{HintVersion, VersionedHint},
            types::{OwnerLease, ThreadKey},
        };
        let ledger = StoreInteractionHints::new(store());
        let authority = HintLeaseAuthority::new(OwnerLease {
            key: ThreadKey {
                host_id: "h".into(),
                thread_id: "t".into(),
            },
            client_id: "c".into(),
            epoch: 1,
            generation: 1,
        });
        let permit = authority.rotate_scope().unwrap();
        let epoch = ledger.begin_epoch().unwrap();
        let hint = VersionedHint {
            hint: InteractionHint {
                thread_id: "t".into(),
                source_turn_id: "turn".into(),
                source_item_id: "item".into(),
                question_index: 0,
                last_fact: "pending".into(),
                witness: None,
                evidence_state: None,
            },
            version: HintVersion { epoch, sequence: 0 },
        };
        let mut valid = hint.clone();
        valid.hint.source_item_id = "valid-first".into();
        valid.version.sequence = 1;
        let result = ledger.write_batch(epoch, &[valid, hint], 1, &permit);
        assert!(result.is_err());
        assert_eq!(
            result,
            Err(HintStoreError::InvalidData),
            "invalid version must be classified as permanent data failure"
        );
        assert!(
            ledger.load("t").unwrap().is_empty(),
            "invalid batch must not persist"
        );
    }

    #[test]
    fn b1_store_revoked_owner_rolls_back_real_batch() {
        use crate::services::codex_desktop_ipc::{
            async_question::HintStoreError,
            hint_commit::HintLeaseAuthority,
            hint_state::{HintVersion, VersionedHint},
            types::{OwnerLease, ThreadKey},
        };
        let db = store();
        let ledger = StoreInteractionHints::new(db.clone());
        let authority = HintLeaseAuthority::new(OwnerLease {
            key: ThreadKey {
                host_id: "h".into(),
                thread_id: "t".into(),
            },
            client_id: "c".into(),
            epoch: 1,
            generation: 1,
        });
        let permit = authority.rotate_scope().unwrap();
        let epoch = ledger.begin_epoch().unwrap();
        let hint = VersionedHint {
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
        authority.revoke_owner();
        assert_eq!(
            ledger.write_batch(epoch, &[hint], 1, &permit),
            Err(HintStoreError::OwnerFenced)
        );
        assert!(ledger.load("t").unwrap().is_empty());
    }

    #[test]
    fn b1_store_real_commit_failure_does_not_return_success() {
        use crate::services::codex_desktop_ipc::{
            async_question::HintStoreError,
            hint_commit::HintLeaseAuthority,
            hint_state::{HintVersion, VersionedHint},
            types::{OwnerLease, ThreadKey},
        };
        let db = store();
        db.with(|conn| conn.execute_batch("PRAGMA foreign_keys=ON; CREATE TABLE test_parent(id INTEGER PRIMARY KEY); CREATE TABLE test_child(parent_id INTEGER REFERENCES test_parent(id) DEFERRABLE INITIALLY DEFERRED); CREATE TRIGGER test_deferred AFTER INSERT ON codex_interaction_hints BEGIN INSERT INTO test_child(parent_id) VALUES(42); END;").unwrap());
        let ledger = StoreInteractionHints::new(db);
        let authority = HintLeaseAuthority::new(OwnerLease {
            key: ThreadKey {
                host_id: "h".into(),
                thread_id: "t".into(),
            },
            client_id: "c".into(),
            epoch: 1,
            generation: 1,
        });
        let permit = authority.rotate_scope().unwrap();
        let epoch = ledger.begin_epoch().unwrap();
        let hint = VersionedHint {
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
        assert_eq!(
            ledger.write_batch(epoch, &[hint], 1, &permit),
            Err(HintStoreError::Storage)
        );
        authority.revoke_owner();
        assert!(ledger.load("t").unwrap().is_empty());
    }
    fn evidence() -> DesktopContractEvidence {
        let mut matches = BTreeMap::new();
        matches.insert(
            CodexOperation::AsyncAnswerIdle,
            ContractMatch {
                method: "thread-follower-start-turn".into(),
                base_version: 2,
                request_rule: "conversationId+turnStart".into(),
                response_rule: "result.result".into(),
                semantic_rule: "envelope".into(),
            },
        );
        DesktopContractEvidence {
            probe_key: "macos|26.1021.31345|cli|1".into(),
            package_sha256: "a".repeat(64),
            rule_version: CONTRACT_RULE_VERSION,
            matches,
            package_size: Some(4096),
            package_mtime_ms: Some(1_700_000_000_000),
        }
    }

    #[test]
    fn r17_contract_evidence_round_trips_and_pins_to_current_package() {
        let store = store();
        let ledger = StoreContractEvidence::new(store.clone());
        ledger.store(&evidence(), 1_000);
        let rows = ledger
            .load("macos|26.1021.31345|cli|1", CONTRACT_RULE_VERSION)
            .expect("rows");
        let restored =
            DesktopContractEvidence::from_rows(&rows, CONTRACT_RULE_VERSION).expect("restore");
        assert!(restored.matched(CodexOperation::AsyncAnswerIdle));
        // 当前包指纹一致 → 可复用
        assert!(restored
            .clone()
            .pinned_to_current_package(Some((4096, 1_700_000_000_000)))
            .is_some());
        // 包内容改变（大小不同）→ 旧结论不得安装
        assert!(restored
            .clone()
            .pinned_to_current_package(Some((4097, 1_700_000_000_000)))
            .is_none());
        // 未记录的旧行 → 不猜
        let mut unpinned = restored;
        unpinned.package_size = None;
        unpinned.package_mtime_ms = None;
        assert!(unpinned
            .pinned_to_current_package(Some((4096, 1_700_000_000_000)))
            .is_none());
    }

    /// H01：台账 `accepted` 行**不**等于成功证据；只有按目标记录的效果才生效，
    /// 且跨目标/跨规则不通用。
    #[test]
    fn h01_success_is_target_scoped_not_ledger_derived() {
        let store = store();
        store.with(|conn| {
            conn.execute(
                "INSERT INTO codex_actions
                  (action_id, device_id, device_name, thread_id, kind, payload_hash,
                   client_message_id, state, route, created_at, updated_at)
                 VALUES ('a1','dev','iPhone','th','async_question','h','c','accepted',
                         'desktop_ipc',1,1)",
                [],
            )
            .unwrap();
        });
        let target = |key: &str, sha: &str| ObservedTarget {
            probe_key: key.into(),
            package_sha256: sha.into(),
        };
        let observed = TargetObservedOperations::new(store.clone());
        assert!(
            observed
                .observed(Some(&target("macos|build-a|cli|1", "sha-a")))
                .is_empty(),
            "仅 Accepted 不得成为真实成功证据"
        );
        assert!(observed.observed(None).is_empty(), "无目标 = 无证据");
        record_operation_success(&store, "macos|build-a|cli|1", "sha-a", "async_answer_idle");
        // 缓存按目标分桶（10s）；新证据在下一轮求值生效。
        let observed = TargetObservedOperations::new(store.clone());
        let map = observed.observed(Some(&target("macos|build-a|cli|1", "sha-a")));
        assert_eq!(map.get(&CodexOperation::AsyncAnswerIdle), Some(&true));
        assert!(
            observed
                .observed(Some(&target("macos|build-b|cli|1", "sha-a")))
                .is_empty(),
            "成功证据不得跨目标（probe key）复用"
        );
        assert!(
            observed
                .observed(Some(&target("macos|build-a|cli|1", "sha-b")))
                .is_empty(),
            "同 build/CLI 但包内容不同也不得复用"
        );
    }

    #[test]
    fn r18_hint_store_round_trips_without_bodies() {
        let store = store();
        let hints = StoreInteractionHints::new(store);
        let hint = InteractionHint {
            thread_id: "th".into(),
            source_turn_id: "t1".into(),
            source_item_id: "i1".into(),
            question_index: 2,
            last_fact: "unknown".into(),
            witness: None,
            evidence_state: None,
        };
        hints.upsert(&hint, 1_000);
        let loaded = hints.load("th").expect("rows");
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].last_fact, "unknown");
        assert_eq!(loaded[0].question_index, 2);
    }
}
