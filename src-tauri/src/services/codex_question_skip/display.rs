//! 题卡展示冻结缓存：数据库读写在镜像锁外，发布只消费带版本的不可变观察。

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use rusqlite::Connection;

use super::live::{FrozenSource, LiveRefusal};
use super::types::{DisplayDecision, GroupIdentity, NativeEffect, SkipReason};
use crate::database::dao::codex_question_display::{self as dao, HistoryEntry, StoreError};
use crate::remote::pairing::DeviceStore;
use crate::services::codex_desktop_ipc::types::{
    QuestionDisplayDecision, QuestionDisplayGroupView, QuestionGroupIdentity4,
    QuestionNativeEffect, QuestionSkipHistoryQuestion, QuestionSkipHistoryView, ThreadKey,
    ThreadProjection,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct DisplayChanged;

#[derive(Clone)]
pub(crate) struct FrozenHistory {
    key: ThreadKey,
    epoch: u64,
    entries: Arc<Vec<HistoryEntry>>,
}

// 冻结快照的读取访问器只服务测试断言（生产经 overlay_projection 直接读字段）。
#[cfg(test)]
impl FrozenHistory {
    pub(crate) fn epoch(&self) -> u64 {
        self.epoch
    }
    pub(crate) fn entries(&self) -> &[HistoryEntry] {
        &self.entries
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DisplayObservationStamp {
    thread: ThreadKey,
    epoch: u64,
}

// 同上：stamp 访问器仅测试使用（生产直接比较字段）。
#[cfg(test)]
impl DisplayObservationStamp {
    pub(crate) fn thread(&self) -> &ThreadKey {
        &self.thread
    }
    pub(crate) fn epoch(&self) -> u64 {
        self.epoch
    }
}

#[derive(Default)]
struct ThreadCache {
    epoch: u64,
    writers: usize,
    frozen: Option<FrozenHistory>,
}

#[derive(Default)]
struct CacheState {
    clock: u64,
    exhausted: bool,
    global_writers: usize,
    threads: HashMap<ThreadKey, ThreadCache>,
}

impl CacheState {
    fn next_epoch(&mut self) -> Option<u64> {
        if self.exhausted {
            return None;
        }
        match self.clock.checked_add(1) {
            Some(next) => {
                self.clock = next;
                Some(next)
            }
            None => {
                self.exhausted = true;
                for slot in self.threads.values_mut() {
                    slot.frozen = None;
                }
                None
            }
        }
    }
}

pub(crate) struct QuestionDisplayStore {
    store: DeviceStore,
    cache: Arc<Mutex<CacheState>>,
}

/// 同步写即使 panic 退出也不能让旧读重新取得资格；未完成的写保持 Unknown。
struct MutationTicket {
    cache: Arc<Mutex<CacheState>>,
    key: Option<ThreadKey>,
    epoch: u64,
}

impl MutationTicket {
    fn complete(self, entries: Option<Vec<HistoryEntry>>) {
        if let Ok(mut state) = self.cache.lock() {
            let global_writers = state.global_writers;
            let exhausted = state.exhausted;
            if let Some(key) = &self.key {
                if let Some(slot) = state.threads.get_mut(key) {
                    if !exhausted
                        && global_writers == 0
                        && slot.writers == 1
                        && slot.epoch == self.epoch
                    {
                        slot.frozen = entries.map(|entries| FrozenHistory {
                            key: key.clone(),
                            epoch: self.epoch,
                            entries: Arc::new(entries),
                        });
                    }
                }
            }
        }
        // Drop 在缓存安装后扣除本写的在途计数，二者之间没有 await。
    }
}

impl Drop for MutationTicket {
    fn drop(&mut self) {
        if let Ok(mut state) = self.cache.lock() {
            match &self.key {
                Some(key) => {
                    if let Some(slot) = state.threads.get_mut(key) {
                        slot.writers = slot.writers.saturating_sub(1);
                    }
                }
                None => state.global_writers = state.global_writers.saturating_sub(1),
            }
        }
    }
}

impl QuestionDisplayStore {
    pub(crate) fn new(store: DeviceStore) -> Arc<Self> {
        Arc::new(Self {
            store,
            cache: Arc::new(Mutex::new(CacheState::default())),
        })
    }

    pub(crate) async fn ensure_loaded(&self, key: &ThreadKey) {
        let epoch = {
            let Ok(mut state) = self.cache.lock() else {
                return;
            };
            if state.exhausted || state.global_writers != 0 {
                return;
            }
            if let Some(slot) = state.threads.get(key) {
                if slot.writers != 0 || slot.frozen.is_some() {
                    return;
                }
            }
            // 并发只读可共用世代；任意写在 SQL 前推进 epoch，旧读必失配。
            if !state.threads.contains_key(key) {
                let Some(epoch) = state.next_epoch() else {
                    return;
                };
                state.threads.insert(
                    key.clone(),
                    ThreadCache {
                        epoch,
                        ..Default::default()
                    },
                );
            }
            state.threads[key].epoch
        };
        let store = self.store.clone();
        let read_key = key.clone();
        let entries = tokio::task::spawn_blocking(move || {
            store.with(|conn| dao::list_thread_history(conn, &read_key.thread_id))
        })
        .await;
        let Ok(Ok(entries)) = entries else { return };
        let Ok(mut state) = self.cache.lock() else {
            return;
        };
        if state.exhausted || state.global_writers != 0 {
            return;
        }
        if let Some(slot) = state.threads.get_mut(key) {
            if slot.epoch == epoch && slot.writers == 0 {
                slot.frozen = Some(FrozenHistory {
                    key: key.clone(),
                    epoch,
                    entries: Arc::new(entries),
                });
            }
        }
    }

    pub(crate) fn try_frozen(&self, key: &ThreadKey) -> Option<FrozenHistory> {
        let state = self.cache.try_lock().ok()?;
        if state.exhausted || state.global_writers != 0 {
            return None;
        }
        let slot = state.threads.get(key)?;
        if slot.writers != 0 {
            return None;
        }
        slot.frozen.clone()
    }

    pub(crate) fn with_publish_eligibility<R>(
        &self,
        stamp: &DisplayObservationStamp,
        reserve: impl FnOnce() -> R,
    ) -> Result<R, DisplayChanged> {
        let state = self.cache.try_lock().map_err(|_| DisplayChanged)?;
        if state.exhausted || state.global_writers != 0 {
            return Err(DisplayChanged);
        }
        let slot = state.threads.get(&stamp.thread).ok_or(DisplayChanged)?;
        if slot.writers != 0 || slot.epoch != stamp.epoch || slot.frozen.is_none() {
            return Err(DisplayChanged);
        }
        Ok(reserve())
    }

    pub(crate) fn run_mutation<R>(
        &self,
        key: Option<&ThreadKey>,
        write: impl FnOnce(&Connection) -> Result<R, StoreError>,
    ) -> Result<R, StoreError> {
        let ticket = {
            let mut state = self.cache.lock().map_err(|_| StoreError::Database)?;
            let epoch = state.next_epoch().ok_or(StoreError::Database)?;
            match key {
                Some(key) => {
                    let slot = state.threads.entry(key.clone()).or_default();
                    slot.epoch = epoch;
                    slot.frozen = None;
                    slot.writers = slot.writers.checked_add(1).ok_or(StoreError::Database)?;
                }
                None => {
                    state.global_writers = state
                        .global_writers
                        .checked_add(1)
                        .ok_or(StoreError::Database)?;
                    for slot in state.threads.values_mut() {
                        slot.epoch = epoch;
                        slot.frozen = None;
                    }
                }
            }
            MutationTicket {
                cache: self.cache.clone(),
                key: key.cloned(),
                epoch,
            }
        };
        // 同一个连接串行执行原 SQL 和冻结读；没有缓存锁/镜像锁嵌套。
        let (result, entries) = self.store.with(|conn| {
            let result = write(conn);
            let entries = if result.is_ok() {
                key.and_then(|key| dao::list_thread_history(conn, &key.thread_id).ok())
            } else {
                None
            };
            (result, entries)
        });
        ticket.complete(entries);
        result
    }

    pub(crate) fn store(&self) -> &DeviceStore {
        &self.store
    }
}

pub(crate) fn group_wire(group: &GroupIdentity) -> QuestionGroupIdentity4 {
    QuestionGroupIdentity4 {
        thread_id: group.thread_id.clone(),
        source_turn_id: group.source_turn_id.clone(),
        source_item_id: group.source_item_id.clone(),
        content_generation: group.content_generation.clone(),
    }
}

pub(crate) fn decision_wire(decision: &DisplayDecision) -> QuestionDisplayDecision {
    match decision {
        DisplayDecision::Active => QuestionDisplayDecision::Active,
        DisplayDecision::Skipped => QuestionDisplayDecision::Skipped,
    }
}

pub(crate) fn effect_wire(effect: &NativeEffect) -> QuestionNativeEffect {
    match effect {
        NativeEffect::ClosedVerified => QuestionNativeEffect::ClosedVerified,
        NativeEffect::NotPresentVerified => QuestionNativeEffect::NotPresentVerified,
        NativeEffect::Unresolved => QuestionNativeEffect::Unresolved,
        NativeEffect::Unsupported => QuestionNativeEffect::Unsupported,
    }
}

pub(crate) fn reason_wire(reason: &SkipReason) -> Option<String> {
    let code = match reason {
        SkipReason::None => return None,
        SkipReason::BindingRequired => "binding_required",
        SkipReason::LeaseExpired => "lease_expired",
        SkipReason::ScopeChanged => "scope_changed",
        SkipReason::SourceIncomplete => "source_incomplete",
        SkipReason::Ambiguous => "ambiguous",
        SkipReason::ContentChanged => "content_changed",
        SkipReason::NativeCountUnknown => "native_count_unknown",
        SkipReason::UnsupportedGroup => "unsupported_group",
        SkipReason::UnsupportedBuild => "unsupported_build",
        SkipReason::InvokeUnavailable => "invoke_unavailable",
        SkipReason::NativeUnavailable => "native_unavailable",
        SkipReason::ConcurrentAction => "concurrent_action",
        SkipReason::InvokedUnconfirmed => "invoked_unconfirmed",
        SkipReason::ActionConflict => "action_conflict",
        SkipReason::LeaseConsumed => "lease_consumed",
        SkipReason::StoreUnavailable => "store_unavailable",
    };
    Some(code.into())
}

pub(crate) fn overlay_projection(
    projection: &mut ThreadProjection,
    source: Result<&FrozenSource, LiveRefusal>,
    history: Option<&FrozenHistory>,
) -> Option<DisplayObservationStamp> {
    projection.question_display_groups = None;
    projection.question_skip_history = None;
    projection.display_stamp = None;
    let history = history.filter(|history| history.key.thread_id == projection.thread_id)?;
    projection.question_skip_history = Some(
        history
            .entries
            .iter()
            .map(|entry| QuestionSkipHistoryView {
                group: group_wire(&entry.group),
                revision: entry.revision,
                decision: decision_wire(&entry.decision),
                latest_native_effect: entry
                    .latest_skip
                    .as_ref()
                    .map(|skip| effect_wire(&skip.native.effect)),
                reason_code: entry
                    .latest_skip
                    .as_ref()
                    .and_then(|skip| reason_wire(&skip.native.reason)),
                question: entry
                    .snapshot
                    .as_ref()
                    .map(|question| QuestionSkipHistoryQuestion {
                        question_key: question.question_key.clone(),
                        question_index: question.index,
                        title: question.title.clone(),
                        options: question.options.clone(),
                    }),
            })
            .collect(),
    );
    if let Ok(source) = source {
        if source.proof.is_live()
            && !source.snapshot.fence.stale
            && !source.snapshot.fence.budget_refused
            && !source.snapshot.fence.truncated
            && source.snapshot.fence.raw_complete
            && source
                .snapshot
                .groups
                .iter()
                .all(|group| group.selector.group.thread_id == history.key.thread_id)
        {
            projection.question_display_groups = Some(
                source
                    .snapshot
                    .groups
                    .iter()
                    .map(|group| {
                        let entry = history
                            .entries
                            .iter()
                            .find(|entry| entry.group == group.selector.group);
                        QuestionDisplayGroupView {
                            group: group_wire(&group.selector.group),
                            revision: entry.map_or(0, |entry| entry.revision),
                            decision: entry.map_or(QuestionDisplayDecision::Active, |entry| {
                                decision_wire(&entry.decision)
                            }),
                            latest_native_effect: entry
                                .and_then(|entry| entry.latest_skip.as_ref())
                                .map(|skip| effect_wire(&skip.native.effect)),
                            reason_code: entry
                                .and_then(|entry| entry.latest_skip.as_ref())
                                .and_then(|skip| reason_wire(&skip.native.reason)),
                            source_complete: group.selector.source_complete,
                            source_question_count: group.declared_count,
                        }
                    })
                    .collect(),
            );
        }
    }
    let stamp = DisplayObservationStamp {
        thread: history.key.clone(),
        epoch: history.epoch,
    };
    projection.display_stamp = Some(stamp.clone());
    Some(stamp)
}
