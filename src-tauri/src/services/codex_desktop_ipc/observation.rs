//! 只读正文观察事件；不序列化临时正文。
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct CaptureScope {
    pub capture_generation: u64,
    pub owner_epoch: u64,
    pub owner_generation: u64,
    pub host_key: String,
    pub thread_key: String,
}
#[derive(Debug, Clone, Copy)]
pub struct CaptureClock {
    pub mono_ns: u64,
    pub wall_ms: i64,
}
pub enum BodyChange {
    Baseline,
    EmptyItem {
        turn_id: String,
        item_id: String,
        model: Option<String>,
        channel: Option<String>,
    },
    Append {
        turn_id: String,
        item_id: String,
        text: String,
    },
    InvalidatedItem {
        turn_id: String,
        item_id: Option<String>,
    },
    TurnTerminal {
        turn_id: String,
        completed: bool,
    },
}
pub struct BodyFrame {
    pub scope: CaptureScope,
    pub revision: u64,
    pub clock: CaptureClock,
    pub changes: Vec<BodyChange>,
}

/// 远程写入权限代际；关闭后已捕获的许可不得在重开时复活。
pub(crate) struct RemoteWriteGeneration(std::sync::atomic::AtomicU64);
impl Default for RemoteWriteGeneration {
    fn default() -> Self {
        Self(std::sync::atomic::AtomicU64::new(1))
    }
}
impl RemoteWriteGeneration {
    pub fn capture(&self) -> u64 {
        self.0.load(std::sync::atomic::Ordering::SeqCst)
    }
    pub fn stop(&self) {
        use std::sync::atomic::Ordering;
        // 不用 Atomic::fetch_update：新版 Rust 已把它标记为 deprecated，CI 的
        // `-D warnings` 会直接失败。CAS 循环语义相同，且不依赖版本。
        let mut current = self.0.load(Ordering::SeqCst);
        while current % 2 == 1 {
            match self.0.compare_exchange(
                current,
                current.wrapping_add(1),
                Ordering::SeqCst,
                Ordering::SeqCst,
            ) {
                Ok(_) => return,
                Err(actual) => current = actual,
            }
        }
    }
    pub fn resume(&self) {
        use std::sync::atomic::Ordering;
        // 同 stop：CAS 循环替代已弃用的 fetch_update。
        let mut current = self.0.load(Ordering::SeqCst);
        while current != 0 && current.is_multiple_of(2) {
            match self
                .0
                .compare_exchange(current, current + 1, Ordering::SeqCst, Ordering::SeqCst)
            {
                Ok(_) => return,
                Err(actual) => current = actual,
            }
        }
    }
    pub fn valid(&self, generation: u64) -> bool {
        generation % 2 == 1 && self.capture() == generation
    }
}

#[cfg(test)]
mod tests {
    use super::RemoteWriteGeneration;
    #[test]
    fn stop_reopen_rejects_stale_write_generation() {
        let gate = RemoteWriteGeneration::default();
        let old = gate.capture();
        assert!(gate.valid(old));
        gate.stop();
        assert!(!gate.valid(old));
        gate.resume();
        assert!(!gate.valid(old));
        assert!(gate.valid(gate.capture()));
    }
}

/// 限量观察事件，队列丢帧显式通知消费者撤销未结算候选。
pub enum BodyEvent {
    Frame(BodyFrame),
    Lost,
}
const QUEUED_BODY_LIMIT: usize = 16 * 1024 * 1024;
const QUEUED_ITEM_LIMIT: usize = 1024 * 1024;
/// 事件、候选和后台分词任务共用的字节账本。
#[derive(Default)]
pub struct SharedBodyBudget {
    occupied: std::sync::atomic::AtomicUsize,
    rejected: std::sync::atomic::AtomicU64,
}
impl SharedBodyBudget {
    pub fn rejections(&self) -> u64 {
        self.rejected.load(std::sync::atomic::Ordering::Relaxed)
    }
    pub fn used_bytes(&self) -> usize {
        self.occupied.load(std::sync::atomic::Ordering::SeqCst)
    }
    pub(crate) fn record_rejection(&self) {
        self.rejected
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
    pub(crate) fn reserve(&self, bytes: usize) -> bool {
        use std::sync::atomic::Ordering;
        // 同 connection.rs Budget：CAS 循环替代已弃用的 fetch_update。
        let mut used = self.occupied.load(Ordering::SeqCst);
        let ok = loop {
            let Some(next) = used
                .checked_add(bytes)
                .filter(|next| *next <= QUEUED_BODY_LIMIT)
            else {
                break false;
            };
            match self
                .occupied
                .compare_exchange(used, next, Ordering::SeqCst, Ordering::SeqCst)
            {
                Ok(_) => break true,
                Err(actual) => used = actual,
            }
        };
        if !ok {
            self.record_rejection();
        }
        ok
    }
    pub(crate) fn release(&self, bytes: usize) {
        self.occupied
            .fetch_sub(bytes, std::sync::atomic::Ordering::SeqCst);
    }
}
pub(crate) struct BodyReservation {
    budget: std::sync::Arc<SharedBodyBudget>,
    bytes: usize,
}
impl BodyReservation {
    pub(crate) fn new(budget: std::sync::Arc<SharedBodyBudget>) -> Self {
        Self { budget, bytes: 0 }
    }
    pub(crate) fn grow(&mut self, bytes: usize) -> bool {
        if self.budget.reserve(bytes) {
            self.bytes += bytes;
            true
        } else {
            false
        }
    }
}
impl Drop for BodyReservation {
    fn drop(&mut self) {
        self.budget.release(self.bytes);
    }
}
struct QueuedBodyFrame {
    frame: Option<BodyFrame>,
    _reservation: BodyReservation,
}
impl QueuedBodyFrame {
    fn take_frame(&mut self) -> BodyFrame {
        self.frame.take().expect("frame present")
    }
}
struct ProducerState {
    active: usize,
    open: bool,
    signal: tokio::sync::watch::Sender<u64>,
    sequence: u64,
}
struct ProducerControl(std::sync::Mutex<ProducerState>);
pub(crate) struct BodyProducerGuard(std::sync::Arc<ProducerControl>);
impl Drop for BodyProducerGuard {
    fn drop(&mut self) {
        let mut state = self.0 .0.lock().unwrap_or_else(|e| e.into_inner());
        state.active = state.active.saturating_sub(1);
        state.sequence = state.sequence.wrapping_add(1);
        state.signal.send_replace(state.sequence);
    }
}
struct BodyEventPublisher {
    tx: tokio::sync::mpsc::Sender<(u64, QueuedBodyFrame)>,
    loss: tokio::sync::watch::Sender<u64>,
    epoch: u64,
    budget: std::sync::Arc<SharedBodyBudget>,
    producers: std::sync::Arc<ProducerControl>,
}
#[derive(Clone)]
pub struct BodyEventSender(std::sync::Arc<std::sync::Mutex<BodyEventPublisher>>);
pub struct BodyEventReceiver {
    rx: tokio::sync::mpsc::Receiver<(u64, QueuedBodyFrame)>,
    loss: tokio::sync::watch::Receiver<u64>,
    seen: u64,
    pending: Option<(u64, QueuedBodyFrame)>,
    delivered: Option<QueuedBodyFrame>,
    budget: std::sync::Arc<SharedBodyBudget>,
    producers: std::sync::Arc<ProducerControl>,
}
pub fn body_event_channel(capacity: usize) -> (BodyEventSender, BodyEventReceiver) {
    let (tx, rx) = tokio::sync::mpsc::channel(capacity);
    let (loss, loss_rx) = tokio::sync::watch::channel(0);
    let budget = std::sync::Arc::new(SharedBodyBudget::default());
    let producers = std::sync::Arc::new(ProducerControl(std::sync::Mutex::new(ProducerState {
        active: 0,
        open: true,
        signal: tokio::sync::watch::channel(0).0,
        sequence: 0,
    })));
    (
        BodyEventSender(std::sync::Arc::new(std::sync::Mutex::new(
            BodyEventPublisher {
                tx,
                loss,
                epoch: 0,
                budget: budget.clone(),
                producers: producers.clone(),
            },
        ))),
        BodyEventReceiver {
            rx,
            loss: loss_rx,
            seen: 0,
            pending: None,
            delivered: None,
            budget,
            producers,
        },
    )
}
impl BodyEventPublisher {
    fn invalidate(&mut self) {
        self.epoch = self.epoch.wrapping_add(1);
        self.loss.send_replace(self.epoch);
    }
}
impl BodyEventSender {
    pub(crate) fn reject_budget(&self) {
        let mut state = self.0.lock().unwrap_or_else(|e| e.into_inner());
        state.budget.record_rejection();
        state.invalidate();
    }
    pub(crate) fn try_reserve_temporary(&self, bytes: usize) -> Option<BodyReservation> {
        let mut state = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if !state
            .producers
            .0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .open
        {
            return None;
        }
        let mut reservation = BodyReservation::new(state.budget.clone());
        if reservation.grow(bytes) {
            Some(reservation)
        } else {
            state.invalidate();
            None
        }
    }
    pub(crate) fn begin_producer(&self) -> Option<BodyProducerGuard> {
        let publisher = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let control = publisher.producers.clone();
        let mut state = control.0.lock().unwrap_or_else(|e| e.into_inner());
        if !state.open {
            return None;
        }
        state.active = state.active.checked_add(1)?;
        drop(state);
        Some(BodyProducerGuard(control))
    }
    pub(crate) fn close(&self) {
        let publisher = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let mut state = publisher
            .producers
            .0
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        state.open = false;
    }
    pub fn invalidate(&self) {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .invalidate();
    }
    pub fn try_send(&self, frame: BodyFrame) -> bool {
        let mut state = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if !state
            .producers
            .0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .open
        {
            return false;
        }
        let mut bytes = 0usize;
        for change in &frame.changes {
            if let BodyChange::Append { text, .. } = change {
                if text.len() > QUEUED_ITEM_LIMIT {
                    state.budget.record_rejection();
                    state.invalidate();
                    return false;
                }
                bytes = bytes.saturating_add(text.len());
            }
        }
        let mut reservation = BodyReservation::new(state.budget.clone());
        if !reservation.grow(bytes) {
            state.invalidate();
            return false;
        }
        let queued = QueuedBodyFrame {
            frame: Some(frame),
            _reservation: reservation,
        };
        match state.tx.try_send((state.epoch, queued)) {
            Ok(()) => true,
            Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                state.invalidate();
                false
            }
            Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => false,
        }
    }
}
impl BodyEventReceiver {
    /// 禁止新的采集生产者并等待已有正文副本释放。
    pub async fn close_and_wait(&mut self) {
        let mut signal = {
            let mut state = self.producers.0.lock().unwrap_or_else(|e| e.into_inner());
            state.open = false;
            state.signal.subscribe()
        };
        loop {
            if self
                .producers
                .0
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .active
                == 0
            {
                break;
            }
            if signal.changed().await.is_err() {
                break;
            }
        }
    }
    pub fn memory_budget(&self) -> std::sync::Arc<SharedBodyBudget> {
        self.budget.clone()
    }
    /// 已交付帧处理完成后释放其缓冲预留。
    pub fn release_delivered(&mut self) {
        self.delivered = None;
    }
    pub async fn recv(&mut self) -> Option<BodyEvent> {
        self.release_delivered();
        loop {
            let epoch = *self.loss.borrow_and_update();
            if epoch != self.seen {
                self.seen = epoch;
                if self.pending.as_ref().is_some_and(|(v, _)| *v != epoch) {
                    self.pending = None;
                }
                while self.pending.is_none() {
                    match self.rx.try_recv() {
                        Ok((item_epoch, frame)) if item_epoch == epoch => {
                            self.pending = Some((item_epoch, frame));
                        }
                        Ok(_) => {}
                        Err(_) => break,
                    }
                }
                return Some(BodyEvent::Lost);
            }
            let item = if let Some(item) = self.pending.take() {
                Some(item)
            } else {
                tokio::select! { biased;
                    changed=self.loss.changed()=>{if changed.is_err(){return None;} continue;},
                    item=self.rx.recv()=>item,
                }
            };
            let (item_epoch, mut frame) = item?;
            let current = *self.loss.borrow();
            if current != self.seen {
                if item_epoch == current {
                    self.pending = Some((item_epoch, frame));
                }
                continue;
            }
            if item_epoch == self.seen {
                let body = frame.take_frame();
                self.delivered = Some(frame);
                return Some(BodyEvent::Frame(body));
            }
        }
    }
}

#[cfg(test)]
mod queue_tests {
    use super::*;
    fn frame(revision: u64) -> BodyFrame {
        BodyFrame {
            scope: CaptureScope {
                capture_generation: 1,
                owner_epoch: 1,
                owner_generation: 1,
                host_key: "h".into(),
                thread_key: "t".into(),
            },
            revision,
            clock: CaptureClock {
                mono_ns: revision,
                wall_ms: revision as i64,
            },
            changes: vec![],
        }
    }
    #[tokio::test]
    async fn overflow_invalidates_before_any_old_frame_is_delivered() {
        let (tx, mut rx) = body_event_channel(1);
        assert!(tx.try_send(frame(1)));
        assert!(!tx.try_send(frame(2)));
        assert!(matches!(rx.recv().await, Some(BodyEvent::Lost)));
        assert!(tx.try_send(frame(3)));
        assert!(matches!(rx.recv().await,Some(BodyEvent::Frame(f)) if f.revision==3));
    }
    #[tokio::test]
    async fn loss_wakes_waiting_receiver_without_followup_frame() {
        let (tx, mut rx) = body_event_channel(1);
        let waiting = tokio::spawn(async move { rx.recv().await });
        tokio::task::yield_now().await;
        tx.invalidate();
        let event = tokio::time::timeout(std::time::Duration::from_secs(1), waiting)
            .await
            .expect("loss must wake receiver")
            .unwrap();
        assert!(matches!(event, Some(BodyEvent::Lost)));
    }
    #[tokio::test]
    async fn oversize_body_event_is_dropped_and_invalidates() {
        let (tx, mut rx) = body_event_channel(2);
        let mut big = frame(1);
        big.changes.push(BodyChange::Append {
            turn_id: "t".into(),
            item_id: "i".into(),
            text: "a".repeat(1024 * 1024 + 1),
        });
        let budget = rx.memory_budget();
        assert!(!tx.try_send(big));
        assert_eq!(
            budget.rejections(),
            1,
            "budget exclusion must be diagnosable"
        );
        assert!(matches!(rx.recv().await, Some(BodyEvent::Lost)));
    }
    #[tokio::test]
    async fn queued_body_budget_is_global_and_reclaimed_on_loss() {
        let (tx, mut rx) = body_event_channel(32);
        for n in 0..16 {
            let mut f = frame(n + 1);
            f.changes.push(BodyChange::Append {
                turn_id: "t".into(),
                item_id: format!("{n}"),
                text: "a".repeat(1024 * 1024),
            });
            assert!(tx.try_send(f));
        }
        let mut excess = frame(17);
        excess.changes.push(BodyChange::Append {
            turn_id: "t".into(),
            item_id: "overflow".into(),
            text: "x".into(),
        });
        assert!(!tx.try_send(excess));
        assert!(matches!(rx.recv().await, Some(BodyEvent::Lost)));
        let mut next = frame(18);
        next.changes.push(BodyChange::Append {
            turn_id: "t".into(),
            item_id: "next".into(),
            text: "a".repeat(1024 * 1024),
        });
        assert!(tx.try_send(next));
        assert!(matches!(rx.recv().await,Some(BodyEvent::Frame(f)) if f.revision==18));
    }
    #[tokio::test]
    async fn patch_temporary_copies_share_global_budget() {
        let (tx, mut rx) = body_event_channel(2);
        let budget = rx.memory_budget();
        let mut candidates = BodyReservation::new(budget);
        assert!(candidates.grow(15 * 1024 * 1024));
        assert!(tx.try_reserve_temporary(2 * 1024 * 1024).is_none());
        assert!(matches!(rx.recv().await, Some(BodyEvent::Lost)));
    }
}

// 只在历史结构变化时重建 canonical 引用，正文补丁只访问被修改的 item。
#[derive(Default)]
pub(crate) struct BodyPatchTracker {
    references: std::collections::HashSet<String>,
    active: std::collections::HashMap<String, String>,
    complete: bool,
}
struct ItemBefore {
    id: String,
    text: String,
}
struct ItemTouch {
    entity: String,
    index: usize,
    operation: String,
    before: Option<ItemBefore>,
    last_proposed: Option<String>,
    invalid: bool,
    structural: bool,
}
pub(crate) struct PreparedBodyPatch {
    items: Vec<ItemTouch>,
    turns: std::collections::HashSet<String>,
    added: std::collections::HashSet<String>,
    invalid: std::collections::HashSet<String>,
    structural: bool,
    reset: bool,
}
fn body_entities(state: &serde_json::Value) -> Option<&serde_json::Map<String, serde_json::Value>> {
    state
        .pointer("/turnHistory/history/entitiesByKey")?
        .as_object()
}
/// 识别指向 `items[<index>]` 的补丁路径；非该形状返回 None。
fn item_patch_path(path: &[serde_json::Value]) -> Option<(&str, usize)> {
    if path.first().and_then(|v| v.as_str()) != Some("turnHistory")
        || path.get(1).and_then(|v| v.as_str()) != Some("history")
        || path.get(2).and_then(|v| v.as_str()) != Some("entitiesByKey")
        || path.get(4).and_then(|v| v.as_str()) != Some("items")
    {
        return None;
    }
    let entity = path.get(3).and_then(|v| v.as_str())?;
    let index = path
        .get(5)
        .and_then(|v| v.as_u64())
        .and_then(|v| usize::try_from(v).ok())?;
    Some((entity, index))
}
fn body_item(state: &serde_json::Value, entity: &str, index: usize) -> Option<ItemBefore> {
    let item = body_entities(state)?
        .get(entity)?
        .get("items")?
        .as_array()?
        .get(index)?;
    if item.get("type")?.as_str()? != "agentMessage" {
        return None;
    }
    let text = item.get("text")?.as_str()?;
    if text.len() > 1024 * 1024 {
        return None;
    }
    Some(ItemBefore {
        id: item.get("id")?.as_str()?.to_owned(),
        text: text.to_owned(),
    })
}
impl BodyPatchTracker {
    fn refresh_references(&mut self, state: &serde_json::Value) {
        self.references.clear();
        if let Some(islands) = state
            .pointer("/turnHistory/history/islands")
            .and_then(|v| v.as_array())
        {
            for island in islands {
                if let Some(entries) = island.get("entries").and_then(|v| v.as_array()) {
                    for entry in entries {
                        if let Some(value) = entry.get("value").and_then(|v| v.as_str()) {
                            self.references.insert(value.to_owned());
                        }
                    }
                }
            }
        }
        self.complete = state
            .pointer("/turnHistory/history/isComplete")
            .and_then(|v| v.as_bool())
            == Some(true);
    }
    pub(crate) fn baseline(&mut self, state: &serde_json::Value) {
        self.active.clear();
        self.refresh_references(state);
        if !self.complete {
            return;
        }
        let Some(entities) = body_entities(state) else {
            return;
        };
        let mut counts = std::collections::HashMap::<&str, usize>::new();
        for key in &self.references {
            if let Some(id) = entities
                .get(key)
                .and_then(|turn| turn.get("turnId"))
                .and_then(|id| id.as_str())
            {
                *counts.entry(id).or_default() += 1;
            }
        }
        for key in &self.references {
            let Some(turn) = entities.get(key) else {
                continue;
            };
            let Some(id) = turn
                .get("turnId")
                .and_then(|value| value.as_str())
                .filter(|id| !id.is_empty())
            else {
                continue;
            };
            if turn.get("status").and_then(|value| value.as_str()) != Some("inProgress")
                || counts.get(id) != Some(&1)
            {
                continue;
            }
            let Some(items) = turn.get("items").and_then(|value| value.as_array()) else {
                continue;
            };
            let mut ids = std::collections::HashSet::new();
            if items.iter().any(
                |item| match item.get("id").and_then(|value| value.as_str()) {
                    Some(item_id) if !item_id.is_empty() => !ids.insert(item_id),
                    _ => item.get("type").and_then(|value| value.as_str()) == Some("agentMessage"),
                },
            ) {
                continue;
            }
            self.active.insert(key.clone(), id.to_owned());
        }
    }
    pub(crate) fn exceeds_item_limit(
        &self,
        state: &serde_json::Value,
        change: &serde_json::Value,
    ) -> bool {
        let Some(ops) = change.get("patches").and_then(|v| v.as_array()) else {
            return false;
        };
        ops.iter().any(|op| {
            let Some(path) = op.get("path").and_then(|v| v.as_array()) else {
                return false;
            };
            let Some((entity, index)) = item_patch_path(path) else {
                return false;
            };
            if !self.active.contains_key(entity) {
                return false;
            }
            let value = op.get("value");
            if path.len() == 6 {
                return value.is_some_and(|v| {
                    v.get("type").and_then(|t| t.as_str()) == Some("agentMessage")
                        && v.get("text")
                            .and_then(|t| t.as_str())
                            .is_some_and(|text| text.len() > QUEUED_ITEM_LIMIT)
                });
            }
            if path.len() != 7 || path[6].as_str() != Some("text") {
                return false;
            }
            let agent = body_entities(state)
                .and_then(|e| e.get(entity))
                .and_then(|t| t.get("items"))
                .and_then(|v| v.as_array())
                .and_then(|items| items.get(index))
                .is_some_and(|item| {
                    item.get("type").and_then(|t| t.as_str()) == Some("agentMessage")
                });
            agent
                && value
                    .and_then(|v| v.as_str())
                    .is_some_and(|text| text.len() > QUEUED_ITEM_LIMIT)
        })
    }
    /// 复制受影响正文前的保守预算；不复制正文，也不遍历整个历史。
    pub(crate) fn temporary_bytes(
        &self,
        state: &serde_json::Value,
        change: &serde_json::Value,
    ) -> usize {
        let Some(ops) = change.get("patches").and_then(|value| value.as_array()) else {
            return 0;
        };
        // 同一 item 的多次修改只保留一份旧值和一份最新值；不得按操作数重复预留。
        let mut items = std::collections::HashMap::<(&str, usize), (usize, usize, bool)>::new();
        let mut structural = std::collections::HashMap::<&str, usize>::new();
        for op in ops {
            let Some(path) = op.get("path").and_then(|value| value.as_array()) else {
                continue;
            };
            let Some((entity, index)) = item_patch_path(path) else {
                continue;
            };
            if !self.active.contains_key(entity) {
                continue;
            }
            let old_item = body_entities(state)
                .and_then(|entities| entities.get(entity))
                .and_then(|turn| turn.get("items"))
                .and_then(|items| items.as_array())
                .and_then(|items| items.get(index));
            let old_agent = old_item.is_some_and(|item| {
                item.get("type").and_then(|kind| kind.as_str()) == Some("agentMessage")
            });
            if path.len() == 7 && !old_agent {
                continue;
            }
            let op_name = op.get("op").and_then(|v| v.as_str()).unwrap_or("");
            let old = if old_agent && !(path.len() == 6 && op_name == "add") {
                old_item
                    .and_then(|item| item.get("text"))
                    .and_then(|text| text.as_str())
                    .map_or(0, str::len)
                    .min(QUEUED_ITEM_LIMIT)
            } else {
                0
            };
            let value = op.get("value");
            let new = if path.len() == 7 {
                value.and_then(|v| v.as_str()).map_or(0, str::len)
            } else {
                value
                    .filter(|v| {
                        v.get("type").and_then(|kind| kind.as_str()) == Some("agentMessage")
                    })
                    .and_then(|v| v.get("text"))
                    .and_then(|v| v.as_str())
                    .map_or(0, str::len)
            }
            .min(QUEUED_ITEM_LIMIT);
            let is_structural = path.len() == 6;
            if is_structural {
                *structural.entry(entity).or_default() += 1;
            }
            let entry = items.entry((entity, index)).or_insert((old, 0, false));
            entry.0 = entry.0.max(old);
            entry.1 = entry.1.max(new);
            entry.2 |= is_structural;
        }
        items
            .into_iter()
            .fold(0usize, |total, ((entity, _), (old, new, is_structural))| {
                let new = if is_structural && structural.get(entity).copied().unwrap_or(0) > 1 {
                    new.max(QUEUED_ITEM_LIMIT)
                } else {
                    new
                };
                total.saturating_add(
                    old.saturating_add(new.saturating_mul(2))
                        .saturating_add(new.saturating_sub(old)),
                )
            })
    }
    pub(crate) fn prepare(
        &self,
        state: &serde_json::Value,
        change: &serde_json::Value,
    ) -> PreparedBodyPatch {
        let mut p = PreparedBodyPatch {
            items: Vec::new(),
            turns: Default::default(),
            added: Default::default(),
            invalid: Default::default(),
            structural: false,
            reset: false,
        };
        let Some(ops) = change.get("patches").and_then(|v| v.as_array()) else {
            p.reset = true;
            return p;
        };
        for op in ops {
            let Some(path) = op.get("path").and_then(|v| v.as_array()) else {
                p.reset = true;
                continue;
            };
            if path.first().and_then(|v| v.as_str()) != Some("turnHistory") {
                continue;
            }
            if path.get(1).and_then(|v| v.as_str()) != Some("history") {
                p.reset = true;
                continue;
            }
            match path.get(2).and_then(|v| v.as_str()) {
                Some("islands") => {
                    p.structural = true;
                }
                Some("isComplete") => {
                    p.reset = true;
                }
                Some("entitiesByKey") => {
                    let Some(entity) = path.get(3).and_then(|v| v.as_str()) else {
                        p.reset = true;
                        continue;
                    };
                    let name = op.get("op").and_then(|v| v.as_str()).unwrap_or("");
                    if path.len() == 4 {
                        if name == "add"
                            && body_entities(state).and_then(|e| e.get(entity)).is_none()
                        {
                            p.added.insert(entity.to_owned());
                        } else {
                            p.invalid.insert(entity.to_owned());
                        }
                        continue;
                    }
                    match path.get(4).and_then(|v| v.as_str()) {
                        Some("status") => {
                            p.turns.insert(entity.to_owned());
                        }
                        Some("items") => {
                            if !self.active.contains_key(entity) {
                                continue;
                            }
                            let item_type = op
                                .get("value")
                                .and_then(|v| v.get("type"))
                                .and_then(|v| v.as_str());
                            if matches!(item_type, Some("error" | "retry")) {
                                p.invalid.insert(entity.to_owned());
                                continue;
                            }
                            let Some(index) = path
                                .get(5)
                                .and_then(|v| v.as_u64())
                                .and_then(|v| usize::try_from(v).ok())
                            else {
                                p.invalid.insert(entity.to_owned());
                                continue;
                            };
                            // 思考摘要和工具状态不是正文。先按旧 item 类型区分，
                            // 不能因其嵌套增量路径就撤销整个回合的正文观察。
                            // id/type 改动仍交给下面的身份保护处理。
                            if path.len() > 6 && !matches!(path[6].as_str(), Some("id" | "type")) {
                                let kind = body_entities(state)
                                    .and_then(|entities| entities.get(entity))
                                    .and_then(|turn| turn.get("items"))
                                    .and_then(|items| items.as_array())
                                    .and_then(|items| items.get(index))
                                    .and_then(|item| item.get("type"))
                                    .and_then(|kind| kind.as_str());
                                if matches!(kind, Some("error" | "retry")) {
                                    p.invalid.insert(entity.to_owned());
                                    continue;
                                }
                                if kind.is_some_and(|kind| kind != "agentMessage") {
                                    continue;
                                }
                            }
                            if path.len() > 7
                                || (path.len() == 7 && path[6].as_str() != Some("text"))
                            {
                                if path.len() == 7
                                    && matches!(
                                        path[6].as_str(),
                                        Some(
                                            "startedAtMs"
                                                | "completedAtMs"
                                                | "phase"
                                                | "memoryCitation"
                                                | "delivery"
                                                | "questions"
                                        )
                                    )
                                {
                                    continue;
                                }
                                p.invalid.insert(entity.to_owned());
                                continue;
                            }
                            if path.len() == 7 {
                                let old_item = body_entities(state)
                                    .and_then(|entities| entities.get(entity))
                                    .and_then(|turn| turn.get("items"))
                                    .and_then(|items| items.as_array())
                                    .and_then(|items| items.get(index));
                                if !old_item.is_some_and(|item| {
                                    item.get("type").and_then(|kind| kind.as_str())
                                        == Some("agentMessage")
                                }) {
                                    continue;
                                }
                            }
                            let proposed = if path.len() == 7 && name == "replace" {
                                op.get("value")
                                    .and_then(|v| v.as_str())
                                    .filter(|text| text.len() <= 1024 * 1024)
                            } else {
                                None
                            };
                            if let Some(existing) = p
                                .items
                                .iter_mut()
                                .find(|item| item.entity == entity && item.index == index)
                            {
                                let prior = existing.last_proposed.as_deref();
                                match (prior, proposed) {
                                    (Some(old), Some(next)) if next.starts_with(old) => {
                                        existing.last_proposed = Some(next.to_owned())
                                    }
                                    _ => existing.invalid = true,
                                }
                            } else {
                                let before = if path.len() == 6 && name == "add" {
                                    None
                                } else {
                                    body_item(state, entity, index)
                                };
                                let invalid = match (&before, proposed) {
                                    (Some(old), Some(next)) => !next.starts_with(&old.text),
                                    _ => false,
                                };
                                p.items.push(ItemTouch {
                                    entity: entity.to_owned(),
                                    index,
                                    operation: name.to_owned(),
                                    before,
                                    last_proposed: proposed.map(str::to_owned),
                                    invalid,
                                    structural: path.len() == 6,
                                });
                            }
                        }
                        // 实机观察到的完成标记、工具计时和 hook 元数据不改变正文。
                        // 回合身份、状态和 items 仍由各自分支严格检查。
                        Some(
                            "params"
                            | "completedItemIds"
                            | "commandExecutionStartedAtMsById"
                            | "hookRuns"
                            | "durationMs"
                            | "turnStartedAtMs"
                            | "finalAssistantStartedAtMs"
                            | "firstTurnWorkItemStartedAtMs"
                            | "aeonAssistantMessageStartedAtMsById"
                            | "agentMessageCompletedAtMsById"
                            | "diff"
                            | "localMetadata"
                            | "requestedReasoningEffort"
                            | "mcpAppModelContextAttachments"
                            | "automationKind"
                            | "automationNotificationDecision",
                        ) => {}
                        Some(_) | None => {
                            p.invalid.insert(entity.to_owned());
                        }
                    }
                }
                Some(_) => {}
                None => {
                    p.reset = true;
                }
            }
        }
        p
    }
    pub(crate) fn finish(
        &mut self,
        state: &serde_json::Value,
        p: PreparedBodyPatch,
    ) -> Vec<BodyChange> {
        if p.reset {
            self.baseline(state);
            return vec![BodyChange::Baseline];
        }
        if p.structural {
            self.refresh_references(state);
        }
        if !self.complete {
            self.baseline(state);
            return vec![BodyChange::Baseline];
        }
        let Some(entities) = body_entities(state) else {
            self.baseline(state);
            return vec![BodyChange::Baseline];
        };
        let mut changes = Vec::new();
        let mut identities = std::collections::HashMap::<&str, usize>::new();
        if p.structural || !p.added.is_empty() {
            for key in &self.references {
                if let Some(id) = entities
                    .get(key)
                    .and_then(|turn| turn.get("turnId"))
                    .and_then(|id| id.as_str())
                {
                    *identities.entry(id).or_default() += 1;
                }
            }
        }
        if p.structural || !p.added.is_empty() {
            let removed: Vec<_> = self
                .active
                .iter()
                .filter(|(key, id)| {
                    !self.references.contains(*key)
                        || identities.get(id.as_str()).copied() != Some(1)
                })
                .map(|(key, _)| key.clone())
                .collect();
            for entity in removed {
                if let Some(turn_id) = self.active.remove(&entity) {
                    changes.push(BodyChange::InvalidatedItem {
                        turn_id,
                        item_id: None,
                    });
                }
            }
        }
        for entity in p.invalid {
            if let Some(turn_id) = self.active.remove(&entity) {
                changes.push(BodyChange::InvalidatedItem {
                    turn_id,
                    item_id: None,
                });
            }
        }
        for entity in p.added {
            let Some(turn) = entities.get(&entity) else {
                continue;
            };
            if !self.references.contains(&entity)
                || turn.get("status").and_then(|v| v.as_str()) != Some("inProgress")
            {
                continue;
            }
            let Some(id) = turn.get("turnId").and_then(|v| v.as_str()) else {
                continue;
            };
            let Some(items) = turn.get("items").and_then(|v| v.as_array()) else {
                continue;
            };
            let mut item_ids = std::collections::HashSet::new();
            let ambiguous =
                items
                    .iter()
                    .any(|item| match item.get("id").and_then(|v| v.as_str()) {
                        Some(item_id) if !item_id.is_empty() => !item_ids.insert(item_id),
                        _ => item.get("type").and_then(|v| v.as_str()) == Some("agentMessage"),
                    });
            if ambiguous
                || items
                    .iter()
                    .any(|item| item.get("type").and_then(|v| v.as_str()) == Some("error"))
                || identities.get(id).copied() != Some(1)
                || self.active.values().any(|value| value == id)
            {
                continue;
            }
            let model = turn
                .pointer("/params/collaborationMode/settings/model")
                .and_then(|v| v.as_str())
                .map(str::to_owned);
            let channel = turn
                .pointer("/params/modelProvider")
                .and_then(|v| v.as_str())
                .map(str::to_owned);
            for (index, item) in items.iter().enumerate() {
                if item.get("type").and_then(|v| v.as_str()) == Some("agentMessage")
                    && item.get("text").and_then(|v| v.as_str()) == Some("")
                    && !p
                        .items
                        .iter()
                        .any(|touch| touch.entity == entity && touch.index == index)
                {
                    if let Some(item_id) = item.get("id").and_then(|v| v.as_str()) {
                        changes.push(BodyChange::EmptyItem {
                            turn_id: id.to_owned(),
                            item_id: item_id.to_owned(),
                            model: model.clone(),
                            channel: channel.clone(),
                        });
                    }
                }
            }
            self.active.insert(entity, id.to_owned());
        }
        let changed_items: std::collections::HashSet<_> = p
            .items
            .iter()
            .filter(|touch| touch.structural)
            .map(|touch| touch.entity.clone())
            .collect();
        for entity in changed_items {
            let Some(turn_id) = self.active.get(&entity).cloned() else {
                continue;
            };
            let Some(items) = entities
                .get(&entity)
                .and_then(|turn| turn.get("items"))
                .and_then(|items| items.as_array())
            else {
                self.active.remove(&entity);
                changes.push(BodyChange::InvalidatedItem {
                    turn_id,
                    item_id: None,
                });
                continue;
            };
            let mut seen = std::collections::HashSet::new();
            let ambiguous =
                items
                    .iter()
                    .any(|item| match item.get("id").and_then(|id| id.as_str()) {
                        Some(id) if !id.is_empty() => !seen.insert(id),
                        _ => {
                            item.get("type").and_then(|kind| kind.as_str()) == Some("agentMessage")
                        }
                    });
            if ambiguous {
                self.active.remove(&entity);
                changes.push(BodyChange::InvalidatedItem {
                    turn_id,
                    item_id: None,
                });
            }
        }
        for touch in p.items {
            let Some(turn_id) = self.active.get(&touch.entity).cloned() else {
                continue;
            };
            if !self.references.contains(&touch.entity) {
                self.active.remove(&touch.entity);
                changes.push(BodyChange::InvalidatedItem {
                    turn_id,
                    item_id: None,
                });
                continue;
            }
            if touch.operation == "remove" {
                if let Some(before) = touch.before {
                    changes.push(BodyChange::InvalidatedItem {
                        turn_id,
                        item_id: Some(before.id),
                    });
                }
                continue;
            }
            let after = body_item(state, &touch.entity, touch.index);
            if touch.invalid {
                if let Some(before) = touch.before {
                    changes.push(BodyChange::InvalidatedItem {
                        turn_id,
                        item_id: Some(before.id),
                    });
                }
                continue;
            }
            match (touch.before, after) {
                (None, Some(after)) if touch.operation == "add" && after.text.is_empty() => {
                    let turn = &entities[&touch.entity];
                    let model = turn
                        .pointer("/params/collaborationMode/settings/model")
                        .and_then(|v| v.as_str())
                        .map(str::to_owned);
                    let channel = turn
                        .pointer("/params/modelProvider")
                        .and_then(|v| v.as_str())
                        .map(str::to_owned);
                    changes.push(BodyChange::EmptyItem {
                        turn_id,
                        item_id: after.id,
                        model,
                        channel,
                    });
                }
                (Some(before), Some(after))
                    if before.id == after.id && touch.operation != "remove" =>
                {
                    if let Some(suffix) = after.text.strip_prefix(&before.text) {
                        if !suffix.is_empty() {
                            changes.push(BodyChange::Append {
                                turn_id,
                                item_id: after.id,
                                text: suffix.to_owned(),
                            });
                        }
                    } else {
                        changes.push(BodyChange::InvalidatedItem {
                            turn_id,
                            item_id: Some(before.id),
                        });
                    }
                }
                (Some(before), _) => changes.push(BodyChange::InvalidatedItem {
                    turn_id,
                    item_id: Some(before.id),
                }),
                _ => {}
            }
        }
        for entity in p.turns {
            let Some(turn_id) = self.active.get(&entity).cloned() else {
                continue;
            };
            let status = entities
                .get(&entity)
                .and_then(|v| v.get("status"))
                .and_then(|v| v.as_str());
            if matches!(
                status,
                Some("completed" | "failed" | "interrupted" | "cancelled")
            ) {
                changes.push(BodyChange::TurnTerminal {
                    turn_id,
                    completed: status == Some("completed"),
                });
                self.active.remove(&entity);
            }
        }
        changes
    }
}

#[cfg(test)]
mod patch_tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn reasoning_and_tool_updates_do_not_retire_body_turn() {
        let mut state = json!({"turnHistory":{"history":{"isComplete":true,"islands":[{"entries":[{"value":"turn:x"}]}],"entitiesByKey":{"turn:x":{"turnId":"x","status":"inProgress","items":[{"type":"reasoning","id":"r","summary":[{"text":""}]},{"type":"commandExecution","id":"c","status":"inProgress"}]}}}}});
        let mut tracker = BodyPatchTracker::default();
        tracker.baseline(&state);
        for (index, tail, value) in [
            (
                0,
                vec![json!("summary"), json!(0), json!("text")],
                json!("synthetic reasoning"),
            ),
            (1, vec![json!("status")], json!("completed")),
        ] {
            let mut path = vec![
                json!("turnHistory"),
                json!("history"),
                json!("entitiesByKey"),
                json!("turn:x"),
                json!("items"),
                json!(index),
            ];
            path.extend(tail);
            let prepared = tracker.prepare(
                &state,
                &json!({"patches":[{"op":"replace","path":path,"value":value}]}),
            );
            if index == 0 {
                state["turnHistory"]["history"]["entitiesByKey"]["turn:x"]["items"][0]["summary"]
                    [0]["text"] = value;
            } else {
                state["turnHistory"]["history"]["entitiesByKey"]["turn:x"]["items"][1]["status"] =
                    value;
            }
            assert!(
                tracker.finish(&state, prepared).is_empty(),
                "non-body updates must not invalidate the turn"
            );
        }
        for field in [
            "completedItemIds",
            "commandExecutionStartedAtMsById",
            "hookRuns",
            "durationMs",
            "turnStartedAtMs",
            "finalAssistantStartedAtMs",
            "firstTurnWorkItemStartedAtMs",
            "aeonAssistantMessageStartedAtMsById",
            "agentMessageCompletedAtMsById",
            "diff",
            "localMetadata",
            "requestedReasoningEffort",
            "mcpAppModelContextAttachments",
            "automationKind",
            "automationNotificationDecision",
        ] {
            let change = json!({"patches":[{"op":"add","path":["turnHistory","history","entitiesByKey","turn:x",field],"value":{}}]});
            let prepared = tracker.prepare(&state, &change);
            state["turnHistory"]["history"]["entitiesByKey"]["turn:x"][field] = json!({});
            assert!(
                tracker.finish(&state, prepared).is_empty(),
                "observed bookkeeping fields must not invalidate the turn"
            );
        }
        let change = json!({"patches":[{"op":"add","path":["turnHistory","history","entitiesByKey","turn:x","items",2],"value":{"type":"agentMessage","id":"new","text":""}}]});
        let prepared = tracker.prepare(&state, &change);
        state["turnHistory"]["history"]["entitiesByKey"]["turn:x"]["items"]
            .as_array_mut()
            .unwrap()
            .push(json!({"type":"agentMessage","id":"new","text":""}));
        assert!(tracker
            .finish(&state, prepared)
            .iter()
            .any(|e| matches!(e,BodyChange::EmptyItem{item_id,..} if item_id=="new")));
        for field in [
            "startedAtMs",
            "completedAtMs",
            "phase",
            "memoryCitation",
            "delivery",
            "questions",
        ] {
            let change = json!({"patches":[{"op":"add","path":["turnHistory","history","entitiesByKey","turn:x","items",2,field],"value":null}]});
            let prepared = tracker.prepare(&state, &change);
            state["turnHistory"]["history"]["entitiesByKey"]["turn:x"]["items"][2][field] =
                json!(null);
            assert!(
                tracker.finish(&state, prepared).is_empty(),
                "body metadata must not invalidate the turn"
            );
        }
        let prepared=tracker.prepare(&state,&json!({"patches":[{"op":"replace","path":["turnHistory","history","entitiesByKey","turn:x","status"],"value":"completed"}]}));
        state["turnHistory"]["history"]["entitiesByKey"]["turn:x"]["status"] = json!("completed");
        assert!(
            tracker.finish(&state, prepared).iter().any(|e| matches!(
                e,
                BodyChange::TurnTerminal {
                    completed: true,
                    ..
                }
            )),
            "completion remains observable after metadata updates"
        );
    }
    #[test]
    fn baseline_existing_turn_observes_only_new_empty_item() {
        let mut state = json!({"turnHistory":{"history":{"isComplete":true,"islands":[{"entries":[{"value":"turn:x"}]}],"entitiesByKey":{"turn:x":{"turnId":"x","status":"inProgress","items":[{"type":"agentMessage","id":"old","text":"existing"}]}}}}});
        let mut tracker = BodyPatchTracker::default();
        tracker.baseline(&state);
        let change = json!({"patches":[{"op":"add","path":["turnHistory","history","entitiesByKey","turn:x","items",1],"value":{"type":"agentMessage","id":"new","text":""}}]});
        let prepared = tracker.prepare(&state, &change);
        state["turnHistory"]["history"]["entitiesByKey"]["turn:x"]["items"]
            .as_array_mut()
            .unwrap()
            .push(json!({"type":"agentMessage","id":"new","text":""}));
        let events = tracker.finish(&state, prepared);
        assert!(events.iter().any(|event|matches!(event,BodyChange::EmptyItem{turn_id,item_id,..} if turn_id=="x" && item_id=="new")));
        assert!(!events
            .iter()
            .any(|event| matches!(event,BodyChange::EmptyItem{item_id,..} if item_id=="old")));
    }
    #[test]
    fn rebaseline_never_continues_old_item_but_finishes_new_item() {
        use crate::services::tps::body_observer::BodyObserver;
        for scenario in ["initial", "reconnect", "lost"] {
            let mut state = json!({"turnHistory":{"history":{"isComplete":true,"islands":[{"entries":[{"value":"turn:x"}]}],"entitiesByKey":{"turn:x":{"turnId":"x","status":"inProgress","items":[{"type":"agentMessage","id":"old","text":"already present"}]}}}}});
            let mut tracker = BodyPatchTracker::default();
            tracker.baseline(&state);
            let mut scope = CaptureScope {
                capture_generation: 1,
                owner_epoch: 1,
                owner_generation: 1,
                host_key: "h".into(),
                thread_key: "t".into(),
            };
            let mut observer = BodyObserver::default();
            if scenario != "initial" {
                observer.on_frame(BodyFrame {
                    scope: scope.clone(),
                    revision: 1,
                    clock: CaptureClock {
                        mono_ns: 1,
                        wall_ms: 1,
                    },
                    changes: vec![BodyChange::Baseline],
                });
                observer.on_frame(BodyFrame {
                    scope: scope.clone(),
                    revision: 2,
                    clock: CaptureClock {
                        mono_ns: 2,
                        wall_ms: 2,
                    },
                    changes: vec![BodyChange::EmptyItem {
                        turn_id: "x".into(),
                        item_id: "old".into(),
                        model: None,
                        channel: None,
                    }],
                });
                observer.on_frame(BodyFrame {
                    scope: scope.clone(),
                    revision: 3,
                    clock: CaptureClock {
                        mono_ns: 3,
                        wall_ms: 3,
                    },
                    changes: vec![BodyChange::Append {
                        turn_id: "x".into(),
                        item_id: "old".into(),
                        text: "already present".into(),
                    }],
                });
                if scenario == "lost" {
                    observer.invalidate(&scope);
                } else {
                    scope.owner_epoch += 1;
                }
            }
            observer.on_frame(BodyFrame {
                scope: scope.clone(),
                revision: 1,
                clock: CaptureClock {
                    mono_ns: 1,
                    wall_ms: 1,
                },
                changes: vec![BodyChange::Baseline],
            });
            for (step,patch) in [
                json!({"op":"add","path":["turnHistory","history","entitiesByKey","turn:x","items",1],"value":{"type":"agentMessage","id":"new","text":""}}),
                json!({"op":"replace","path":["turnHistory","history","entitiesByKey","turn:x","items",1,"text"],"value":"one"}),
                json!({"op":"replace","path":["turnHistory","history","entitiesByKey","turn:x","items",1,"text"],"value":"one two"}),
                json!({"op":"replace","path":["turnHistory","history","entitiesByKey","turn:x","status"],"value":"completed"}),
            ].into_iter().enumerate() {
                let prepared=tracker.prepare(&state,&json!({"patches":[patch]}));
                match step { 0=>state["turnHistory"]["history"]["entitiesByKey"]["turn:x"]["items"].as_array_mut().unwrap().push(json!({"type":"agentMessage","id":"new","text":""})), 1=>state["turnHistory"]["history"]["entitiesByKey"]["turn:x"]["items"][1]["text"]=json!("one"), 2=>state["turnHistory"]["history"]["entitiesByKey"]["turn:x"]["items"][1]["text"]=json!("one two"), _=>state["turnHistory"]["history"]["entitiesByKey"]["turn:x"]["status"]=json!("completed") }
                let changes=tracker.finish(&state,prepared);
                let result=observer.on_frame(BodyFrame{scope:scope.clone(),revision:step as u64+2,clock:CaptureClock{mono_ns:(step as u64+2)*1000,wall_ms:step as i64+2},changes});
                if step==3 {assert_eq!(result.len(),1);assert_eq!(result[0].item_id,"new");assert_eq!(result[0].text,"one two");} else {assert!(result.is_empty());}
            }
        }
    }
    #[test]
    fn multiple_text_ops_in_one_frame_append_once() {
        let state = json!({"turnHistory":{"history":{"isComplete":true,"islands":[{"entries":[{"value":"turn:x"}]}],"entitiesByKey":{"turn:x":{"turnId":"x","status":"inProgress","items":[{"type":"agentMessage","id":"i","text":"a"}]}}}}});
        let mut tracker = BodyPatchTracker::default();
        tracker.baseline(&state);
        tracker.active.insert("turn:x".into(), "x".into());
        let change = json!({"patches":[{"op":"replace","path":["turnHistory","history","entitiesByKey","turn:x","items",0,"text"],"value":"ab"},{"op":"replace","path":["turnHistory","history","entitiesByKey","turn:x","items",0,"text"],"value":"abc"}]});
        let prepared = tracker.prepare(&state, &change);
        let mut after = state.clone();
        after["turnHistory"]["history"]["entitiesByKey"]["turn:x"]["items"][0]["text"] =
            json!("abc");
        let events = tracker.finish(&after, prepared);
        assert_eq!(events.len(), 1);
        assert!(matches!(&events[0],BodyChange::Append {text,..} if text=="bc"));
    }
    #[test]
    fn removed_canonical_reference_invalidates_active_turn() {
        let state = json!({"turnHistory":{"history":{"isComplete":true,"islands":[{"entries":[{"value":"turn:x"}]}],"entitiesByKey":{"turn:x":{"turnId":"x","status":"inProgress","items":[]}}}}});
        let mut tracker = BodyPatchTracker::default();
        tracker.baseline(&state);
        tracker.active.insert("turn:x".into(), "x".into());
        let change = json!({"patches":[{"op":"remove","path":["turnHistory","history","islands",0,"entries",0]}]});
        let prepared = tracker.prepare(&state, &change);
        let mut after = state.clone();
        after["turnHistory"]["history"]["islands"][0]["entries"] = json!([]);
        let events = tracker.finish(&after, prepared);
        assert!(events.iter().any(
            |e| matches!(e,BodyChange::InvalidatedItem{turn_id,item_id:None} if turn_id=="x")
        ));
    }
    #[test]
    fn error_item_invalidates_active_turn() {
        let state = json!({"turnHistory":{"history":{"isComplete":true,"islands":[{"entries":[{"value":"turn:x"}]}],"entitiesByKey":{"turn:x":{"turnId":"x","status":"inProgress","items":[{"type":"agentMessage","id":"i","text":"a"}]}}}}});
        let mut tracker = BodyPatchTracker::default();
        tracker.baseline(&state);
        tracker.active.insert("turn:x".into(), "x".into());
        let change = json!({"patches":[{"op":"add","path":["turnHistory","history","entitiesByKey","turn:x","items",1],"value":{"type":"error","id":"err"}}]});
        let prepared = tracker.prepare(&state, &change);
        let mut after = state.clone();
        after["turnHistory"]["history"]["entitiesByKey"]["turn:x"]["items"]
            .as_array_mut()
            .unwrap()
            .push(json!({"type":"error","id":"err"}));
        let events = tracker.finish(&after, prepared);
        assert!(events.iter().any(
            |e| matches!(e,BodyChange::InvalidatedItem{turn_id,item_id:None} if turn_id=="x")
        ));
    }
    #[test]
    fn new_turn_can_include_user_item_and_empty_agent_placeholder() {
        let state = json!({"turnHistory":{"history":{"isComplete":true,"islands":[{"entries":[]}],"entitiesByKey":{}}}});
        let mut tracker = BodyPatchTracker::default();
        tracker.baseline(&state);
        let turn = json!({"turnId":"x","status":"inProgress","items":[{"type":"userMessage","id":"u"},{"type":"agentMessage","id":"i","text":""}]});
        let change = json!({"patches":[{"op":"add","path":["turnHistory","history","entitiesByKey","turn:x"],"value":turn},{"op":"add","path":["turnHistory","history","islands",0,"entries",0],"value":{"value":"turn:x"}}]});
        let prepared = tracker.prepare(&state, &change);
        let mut after = state.clone();
        after["turnHistory"]["history"]["entitiesByKey"]["turn:x"] = turn;
        after["turnHistory"]["history"]["islands"][0]["entries"] = json!([{"value":"turn:x"}]);
        let events = tracker.finish(&after, prepared);
        assert!(events.iter().any(|event| matches!(event, BodyChange::EmptyItem {turn_id,item_id,..} if turn_id == "x" && item_id == "i")));
    }
    #[test]
    fn duplicate_turn_identity_is_not_observable() {
        let state = json!({"turnHistory":{"history":{"isComplete":true,"islands":[{"entries":[{"value":"turn:old"}]}],"entitiesByKey":{"turn:old":{"turnId":"x","status":"completed","items":[]}}}}});
        let mut tracker = BodyPatchTracker::default();
        tracker.baseline(&state);
        let change = json!({"patches":[{"op":"add","path":["turnHistory","history","entitiesByKey","turn:new"],"value":{"turnId":"x","status":"inProgress","items":[]}},{"op":"add","path":["turnHistory","history","islands",0,"entries",1],"value":{"value":"turn:new"}}]});
        let prepared = tracker.prepare(&state, &change);
        let mut after = state.clone();
        after["turnHistory"]["history"]["entitiesByKey"]["turn:new"] =
            json!({"turnId":"x","status":"inProgress","items":[]});
        after["turnHistory"]["history"]["islands"][0]["entries"]
            .as_array_mut()
            .unwrap()
            .push(json!({"value":"turn:new"}));
        tracker.finish(&after, prepared);
        assert!(!tracker.active.contains_key("turn:new"));
    }
    #[test]
    fn duplicate_item_identity_invalidates_turn() {
        let state = json!({"turnHistory":{"history":{"isComplete":true,"islands":[{"entries":[{"value":"turn:x"}]}],"entitiesByKey":{"turn:x":{"turnId":"x","status":"inProgress","items":[{"type":"agentMessage","id":"i","text":"a"}]}}}}});
        let mut tracker = BodyPatchTracker::default();
        tracker.baseline(&state);
        tracker.active.insert("turn:x".into(), "x".into());
        let change = json!({"patches":[{"op":"add","path":["turnHistory","history","entitiesByKey","turn:x","items",1],"value":{"type":"agentMessage","id":"i","text":""}}]});
        let prepared = tracker.prepare(&state, &change);
        let mut after = state.clone();
        after["turnHistory"]["history"]["entitiesByKey"]["turn:x"]["items"]
            .as_array_mut()
            .unwrap()
            .push(json!({"type":"agentMessage","id":"i","text":""}));
        let events = tracker.finish(&after, prepared);
        assert!(events.iter().any(
            |e| matches!(e,BodyChange::InvalidatedItem{turn_id,item_id:None} if turn_id=="x")
        ));
    }
    #[test]
    fn temporary_budget_coalesces_repeated_changes_to_same_item() {
        let state = json!({"turnHistory":{"history":{"isComplete":true,"islands":[{"entries":[{"value":"turn:x"}]}],"entitiesByKey":{"turn:x":{"turnId":"x","status":"inProgress","items":[{"type":"agentMessage","id":"i","text":"a"}]}}}}});
        let mut tracker = BodyPatchTracker::default();
        tracker.baseline(&state);
        tracker.active.insert("turn:x".into(), "x".into());
        let op = json!({"op":"replace","path":["turnHistory","history","entitiesByKey","turn:x","items",0,"text"],"value":"ab"});
        let change = json!({"patches":vec![op;100]});
        assert!(
            tracker.temporary_bytes(&state, &change) < 100,
            "same item must not reserve 100 full copies"
        );
    }
    #[test]
    fn tool_text_patch_never_enters_body_temporary_copies() {
        let state = json!({"turnHistory":{"history":{"isComplete":true,"islands":[{"entries":[{"value":"turn:x"}]}],"entitiesByKey":{"turn:x":{"turnId":"x","status":"inProgress","items":[{"type":"commandExecution","id":"tool","text":"tool output"}]}}}}});
        let mut tracker = BodyPatchTracker::default();
        tracker.baseline(&state);
        tracker.active.insert("turn:x".into(), "x".into());
        let change = json!({"patches":[{"op":"replace","path":["turnHistory","history","entitiesByKey","turn:x","items",0,"text"],"value":"tool output changed"}]});
        assert_eq!(tracker.temporary_bytes(&state, &change), 0);
        assert!(tracker.prepare(&state, &change).items.is_empty());
    }
    #[test]
    fn oversized_agent_patch_is_detected_before_copying() {
        let state = json!({"turnHistory":{"history":{"isComplete":true,"islands":[{"entries":[{"value":"turn:x"}]}],"entitiesByKey":{"turn:x":{"turnId":"x","status":"inProgress","items":[{"type":"agentMessage","id":"i","text":""}]}}}}});
        let mut tracker = BodyPatchTracker::default();
        tracker.baseline(&state);
        tracker.active.insert("turn:x".into(), "x".into());
        let change = json!({"patches":[{"op":"replace","path":["turnHistory","history","entitiesByKey","turn:x","items",0,"text"],"value":"a".repeat(1024*1024+1)}]});
        assert!(tracker.exceeds_item_limit(&state, &change));
    }
}
