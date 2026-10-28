//! R17/R18：**Codex 逐能力证据的生产持久化接线**。
//!
//! 说明：能力表/证据表本身早已入库；本模块把它们接到生产 hub 装配上，使
//! 「契约识别 → 落账 → 重启复用（核对当前包）→ 真实成功」走真实存储，而不是只在
//! 测试里注入内存实现。

use std::collections::BTreeMap;
use std::sync::Mutex;

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
}

impl TargetObservedOperations {
    pub fn new(store: DeviceStore) -> Self {
        Self {
            store,
            cache: Mutex::new(None),
        }
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
            return BTreeMap::new();
        };
        let mut cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((cached, at, value)) = cache.as_ref() {
            if cached == target && at.elapsed() < std::time::Duration::from_secs(10) {
                return value.clone();
            }
        }
        let value = self.refresh(target);
        *cache = Some((target.clone(), std::time::Instant::now(), value.clone()));
        value
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
}

impl StoreInteractionHints {
    pub fn new(store: DeviceStore) -> Self {
        Self { store }
    }
}

impl InteractionHintStore for StoreInteractionHints {
    fn load(&self, thread_id: &str) -> Option<Vec<StoredInteractionHint>> {
        self.store.with(|conn| {
            crate::database::dao::codex_interactions::get_interaction_hints_conn(conn, thread_id)
                .ok()
        })
    }

    fn upsert(&self, hint: &InteractionHint, at_ms: i64) {
        let row = StoredInteractionHint {
            thread_id: hint.thread_id.clone(),
            source_turn_id: hint.source_turn_id.clone(),
            source_item_id: hint.source_item_id.clone(),
            question_index: hint.question_index,
            last_fact: hint.last_fact.clone(),
            updated_at: at_ms,
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
        DeviceStore::Owned(Arc::new(Mutex::new(conn)))
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
        };
        hints.upsert(&hint, 1_000);
        let loaded = hints.load("th").expect("rows");
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].last_fact, "unknown");
        assert_eq!(loaded[0].question_index, 2);
    }
}
