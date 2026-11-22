//! Owner Registry：`(hostId, threadId) → current ownerClientId` 的双向索引与恢复代际。
//!
//! owner 是临时身份（spec F6/3.2）：断开、Desktop 重启、`client-status-changed`
//! (disconnected) 都使映射全部失效并触发重新 discovery；旧 owner 的迟到数据不得
//! 复用（epoch/generation 双代际拒绝）。
//!
//! 同步结构、无 await：跟随管理属于上层单 actor（adapter），不在持锁期间等待。

use super::types::{OwnerLease, ProtocolError, ThreadKey};
use std::collections::{HashMap, HashSet};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

#[derive(Clone, Debug)]
pub(crate) struct OwnerLeaseProof {
    lease: OwnerLease,
    live: Arc<AtomicBool>,
}

impl OwnerLeaseProof {
    pub(crate) fn lease(&self) -> &OwnerLease {
        &self.lease
    }
    pub(crate) fn is_live(&self) -> bool {
        self.live.load(Ordering::Acquire)
    }
}
use tokio::sync::watch;

/// 单连接可跟随 Thread 上限（计划「有界资源」）。
pub const MAX_FOLLOWINGS: usize = 64;

struct LeaseEntry {
    client_id: String,
    epoch: u64,
    generation: u64,
    validity_tx: watch::Sender<bool>,
    live: Arc<AtomicBool>,
    authority: std::sync::Arc<super::hint_commit::HintLeaseAuthority>,
}

/// owner 租约登记表。generation 按 Thread 单调增加；reset 后 epoch 增加。
/// A4/T10：撤销的 gate 等待不再内联（同步阻塞提交等待会拖死 forwarder）——
/// 变更点把待撤销 authority 收进 [`Self::pending_revocations`]，调用方在
/// 临界区外 drain（async 后台撤销 / 同步测试就地撤销）。
#[derive(Default)]
pub struct OwnerRegistry {
    epoch: u64,
    threads: HashMap<ThreadKey, LeaseEntry>,
    by_client: HashMap<String, HashSet<ThreadKey>>,
    gens: HashMap<ThreadKey, u64>,
    pending_revocations: Vec<Arc<super::hint_commit::HintLeaseAuthority>>,
}

impl OwnerRegistry {
    /// 绑定 owner（发现成功后调用）。旧绑定（同 thread 或同 client 旧映射）全部
    /// 失效并向其 validity 信号发 false；generation 单调 +1。epoch 只前进不后退：
    /// 低于当前 epoch 的 bind（旧连接迟到）**直接拒绝**——不驱逐现有绑定、不触碰
    /// 既有 validity 信号、registry epoch 不回退，返回永不生效的死 lease
    /// （generation=0，过不了任何 current/is_current/validity 校验），调用方经
    /// adapter 的「换代兜底」按失败重试。（表内没有「存档」条目的读者，存档无意义。）
    pub fn bind(&mut self, key: ThreadKey, client_id: String, epoch: u64) -> OwnerLease {
        if epoch > self.epoch {
            self.reset(epoch);
        }
        if epoch < self.epoch {
            // 旧连接迟到：保持 current 不变（回归测试
            // old_epoch_bind_does_not_evict_current_binding 防此路径回潮）。
            return OwnerLease {
                key,
                client_id,
                epoch,
                generation: 0,
            };
        }
        // 旧绑定失效（换代）
        if let Some(old) = self.threads.get(&key) {
            old.live.store(false, Ordering::Release);
        }
        if let Some(old) = self.threads.remove(&key) {
            let previous_client = old.client_id.clone();
            self.retire_entry(old);
            if let Some(set) = self.by_client.get_mut(&previous_client) {
                set.remove(&key);
                if set.is_empty() {
                    self.by_client.remove(&previous_client);
                }
            }
        }
        let generation = {
            let g = self.gens.entry(key.clone()).or_insert(0);
            *g += 1;
            *g
        };
        let (validity_tx, _) = watch::channel(true);
        self.threads.insert(
            key.clone(),
            LeaseEntry {
                client_id: client_id.clone(),
                epoch,
                generation,
                validity_tx,
                live: Arc::new(AtomicBool::new(true)),
                authority: super::hint_commit::HintLeaseAuthority::new(OwnerLease {
                    key: key.clone(),
                    client_id: client_id.clone(),
                    epoch,
                    generation,
                }),
            },
        );
        self.by_client
            .entry(client_id.clone())
            .or_default()
            .insert(key.clone());
        OwnerLease {
            key,
            client_id,
            epoch,
            generation,
        }
    }

    /// `client-status-changed(disconnected)`：清该 client 的全部映射。
    /// A4/T10：gate 撤销不内联——authority 进 pending 列表，调用方 drain。
    pub fn invalidate_client(&mut self, client_id: &str) {
        if let Some(keys) = self.by_client.get(client_id) {
            for key in keys {
                if let Some(entry) = self.threads.get(key) {
                    entry.live.store(false, Ordering::Release);
                }
            }
        }
        let Some(keys) = self.by_client.remove(client_id) else {
            return;
        };
        for key in keys {
            if let Some(entry) = self.threads.remove(&key) {
                self.retire_entry(entry);
            }
        }
    }

    /// 单 thread 主动退场（2026-10-17 复审 C4：unfollow 最后一引用退出时回收——
    /// lease 移除 + validity false，后续该 owner 的帧不再有授权可套）。
    pub fn release(&mut self, key: &ThreadKey) {
        if let Some(old) = self.threads.get(key) {
            old.live.store(false, Ordering::Release);
        }
        if let Some(old) = self.threads.remove(key) {
            let previous_client = old.client_id.clone();
            self.retire_entry(old);
            if let Some(set) = self.by_client.get_mut(&previous_client) {
                set.remove(key);
                if set.is_empty() {
                    self.by_client.remove(&previous_client);
                }
            }
        }
    }

    /// 重连/Desktop 重启：清全部 lease（epoch 由 supervisor 递增），generation
    /// 记忆保留（跨 reset 仍单调）。
    pub fn reset(&mut self, new_epoch: u64) {
        // bind 可能先按新连接代际隐式重置并装订；迟到的同代或旧代清理不得撤销它。
        if new_epoch <= self.epoch {
            return;
        }
        for entry in self.threads.values() {
            entry.live.store(false, Ordering::Release);
        }
        self.epoch = new_epoch;
        let drained: Vec<LeaseEntry> = self.threads.drain().map(|(_, entry)| entry).collect();
        for entry in drained {
            self.retire_entry(entry);
        }
        self.by_client.clear();
    }

    /// A4/T10：登记退场（live/validity 立即失效关闭新准入；gate 撤销延后到
    /// 调用方 drain——同步阻塞提交等待不得发生在 owners 临界区/forwarder 内）。
    fn retire_entry(&mut self, entry: LeaseEntry) {
        entry.live.store(false, Ordering::Release);
        let _ = entry.validity_tx.send(false);
        self.pending_revocations.push(entry.authority);
    }

    /// A4/T10：取走待撤销 authority（临界区外执行；async 调用方后台撤销，
    /// 同步测试就地撤销）。旧实例的迟到撤销恒安全：换代 bind 已建新
    /// HintLeaseAuthority 实例，旧实例 fence 不触碰新 lease。
    pub(crate) fn take_pending_revocations(
        &mut self,
    ) -> Vec<Arc<super::hint_commit::HintLeaseAuthority>> {
        std::mem::take(&mut self.pending_revocations)
    }

    pub(crate) fn live_proof(&self, lease: &OwnerLease) -> Option<OwnerLeaseProof> {
        if lease.generation == 0 || !self.is_current(lease) {
            return None;
        }
        let entry = self.threads.get(&lease.key)?;
        Some(OwnerLeaseProof {
            lease: lease.clone(),
            live: entry.live.clone(),
        })
    }

    pub(crate) fn authority(
        &self,
        lease: &OwnerLease,
    ) -> Option<std::sync::Arc<super::hint_commit::HintLeaseAuthority>> {
        if !self.is_current(lease) {
            return None;
        }
        self.threads
            .get(&lease.key)
            .map(|entry| entry.authority.clone())
    }

    /// 当前绑定（epoch 不符视为无绑定）。
    pub fn current(&self, key: &ThreadKey) -> Option<OwnerLease> {
        let entry = self.threads.get(key)?;
        if entry.epoch != self.epoch {
            return None;
        }
        Some(OwnerLease {
            key: key.clone(),
            client_id: entry.client_id.clone(),
            epoch: entry.epoch,
            generation: entry.generation,
        })
    }

    /// lease 是否仍是当前绑定（client/epoch/generation 三者全匹配）。
    pub fn is_current(&self, lease: &OwnerLease) -> bool {
        match self.threads.get(&lease.key) {
            Some(entry) => {
                entry.client_id == lease.client_id
                    && entry.epoch == lease.epoch
                    && entry.generation == lease.generation
                    && entry.epoch == self.epoch
            }
            None => false,
        }
    }

    /// lease 的活性信号：失效/换代时收到 false（adapter 将它传给写请求）。
    /// 传入旧 lease（已非当前绑定）返回已为 false 的信号。
    pub fn validity(&self, lease: &OwnerLease) -> watch::Receiver<bool> {
        if let Some(entry) = self.threads.get(&lease.key) {
            if entry.client_id == lease.client_id
                && entry.epoch == lease.epoch
                && entry.generation == lease.generation
            {
                return entry.validity_tx.subscribe();
            }
        }
        // 旧 lease：永久失活信号
        let (tx, rx) = watch::channel(false);
        let _ = tx; // 无发送端也可订阅（closed 即读 false？——watch 关闭后 borrow 返回最后值）
        rx
    }
}

impl Drop for OwnerRegistry {
    fn drop(&mut self) {
        for entry in self.threads.values() {
            entry.live.store(false, Ordering::Release);
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum FollowPurpose {
    Remote,
    TokenizerEstimate,
}

/// 跟随集合（有界 64；adapter 消费——看板/详情/有待发消息的 Thread 才跟随）。
#[derive(Default)]
pub struct FollowingSet {
    keys: Vec<FollowingEntry>,
    next_ticket: u64,
}
struct FollowingEntry {
    key: ThreadKey,
    remote: bool,
    estimate_tickets: std::collections::HashSet<u64>,
}

impl FollowingSet {
    pub fn acquire_estimate(&mut self, key: ThreadKey) -> Option<u64> {
        let index = self.keys.iter().position(|entry| entry.key == key);
        if index.is_none() && self.keys.len() >= MAX_FOLLOWINGS {
            return None;
        }
        let ticket = self.next_ticket.checked_add(1)?;
        self.next_ticket = ticket;
        if let Some(index) = index {
            self.keys[index].estimate_tickets.insert(ticket);
        } else {
            self.keys.push(FollowingEntry {
                key,
                remote: false,
                estimate_tickets: [ticket].into_iter().collect(),
            });
        }
        Some(ticket)
    }
    /// 返回（票据存在，最后一个用途已释放）。旧票据绝不释放新跟随。
    pub fn release_estimate(&mut self, key: &ThreadKey, ticket: u64) -> (bool, bool) {
        let Some(index) = self.keys.iter().position(|entry| &entry.key == key) else {
            return (false, false);
        };
        let entry = &mut self.keys[index];
        if !entry.estimate_tickets.remove(&ticket) {
            return (false, false);
        }
        if !entry.remote && entry.estimate_tickets.is_empty() {
            self.keys.remove(index);
            (true, true)
        } else {
            (true, false)
        }
    }
    /// 满员时仅让测速独占的跟随让位，调用方负责撤销旧跟随并使候选失效。
    pub fn reserve_remote(&mut self, key: ThreadKey) -> Option<Option<ThreadKey>> {
        if self.is_following(&key) || self.keys.len() < MAX_FOLLOWINGS {
            return self.follow(key).then_some(None);
        }
        let index = self
            .keys
            .iter()
            .position(|entry| !entry.remote && !entry.estimate_tickets.is_empty())?;
        let victim = self.keys.remove(index).key;
        let reserved = self.follow(key);
        debug_assert!(reserved);
        Some(Some(victim))
    }
    pub fn acquire_purpose(&mut self, key: ThreadKey, purpose: FollowPurpose) -> bool {
        if purpose == FollowPurpose::TokenizerEstimate {
            return self.acquire_estimate(key).is_some();
        }
        if let Some(entry) = self.keys.iter_mut().find(|entry| entry.key == key) {
            entry.remote = true;
            return true;
        }
        if self.keys.len() >= MAX_FOLLOWINGS {
            return false;
        }
        self.keys.push(FollowingEntry {
            key,
            remote: true,
            estimate_tickets: Default::default(),
        });
        true
    }
    pub fn release_purpose(&mut self, key: &ThreadKey, purpose: FollowPurpose) -> bool {
        let Some(index) = self.keys.iter().position(|entry| &entry.key == key) else {
            return false;
        };
        if purpose == FollowPurpose::TokenizerEstimate {
            let Some(ticket) = self.keys[index].estimate_tickets.iter().next().copied() else {
                return false;
            };
            return self.release_estimate(key, ticket).1;
        }
        let entry = &mut self.keys[index];
        entry.remote = false;
        if entry.estimate_tickets.is_empty() {
            self.keys.remove(index);
            true
        } else {
            false
        }
    }
    pub fn has_estimate_ticket(&self, key: &ThreadKey, ticket: u64) -> bool {
        self.keys
            .iter()
            .any(|entry| &entry.key == key && entry.estimate_tickets.contains(&ticket))
    }
    pub fn is_remote_following(&self, key: &ThreadKey) -> bool {
        self.keys
            .iter()
            .any(|entry| &entry.key == key && entry.remote)
    }
    pub fn is_estimate_following(&self, key: &ThreadKey) -> bool {
        self.keys
            .iter()
            .any(|entry| &entry.key == key && !entry.estimate_tickets.is_empty())
    }
    pub fn remote_keys(&self) -> Vec<ThreadKey> {
        self.keys
            .iter()
            .filter(|entry| entry.remote)
            .map(|entry| entry.key.clone())
            .collect()
    }
    pub fn is_following(&self, key: &ThreadKey) -> bool {
        self.keys.iter().any(|entry| &entry.key == key)
    }
    pub fn follow(&mut self, key: ThreadKey) -> bool {
        self.acquire_purpose(key, FollowPurpose::Remote)
    }
    pub fn unfollow(&mut self, key: &ThreadKey) -> bool {
        self.release_purpose(key, FollowPurpose::Remote)
    }
    pub fn len(&self) -> usize {
        self.keys.len()
    }
    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }
}

/// Step 1：owner discovery → handledByClientId（owner 身份来源）。
pub async fn discover_owner(
    conn: &super::connection::ConnectionHandle,
    key: &ThreadKey,
) -> Result<String, ProtocolError> {
    let version = conn.version_of("thread-owner-discovery")?;
    let outcome = conn
        .request(super::connection::RpcRequest {
            method: "thread-owner-discovery".into(),
            params: serde_json::json!({
                "hostId": key.host_id,
                "conversationId": key.thread_id,
            }),
            version: Some(version),
            target: None,
            mutating: false,
            validity: None,
            source_client_id: None,
            timeout_ms: None,
            action_id: None,
            compat_licensed: false,
            deadline: None,
            pre_dispatch_turns: Default::default(),
            interaction_recheck: None,
            async_write_recheck: None,
            wire_text: None,
        })
        .await;
    match outcome.delivery {
        super::types::Delivery::Accepted => {}
        super::types::Delivery::Rejected => {
            // no-client-found 等：不能以此判定 A 写请求没送达；此处按发现失败处理
            return Err(ProtocolError::InvalidState(format!(
                "owner discovery rejected: {:?}",
                outcome.result
            )));
        }
        super::types::Delivery::NotSent => {
            return Err(ProtocolError::Disconnected(
                "owner discovery not sent".into(),
            ));
        }
        super::types::Delivery::UnknownDelivery => {
            return Err(ProtocolError::Timeout(
                "owner discovery outcome unknown".into(),
            ));
        }
    }
    outcome.handled_by.ok_or_else(|| {
        ProtocolError::InvalidState("owner discovery missing handledByClientId".into())
    })
}

/// Step 2：绑定 lease（同步、短临界区；调用方持锁调用）。
pub fn bind_lease(
    owners: &mut OwnerRegistry,
    key: &ThreadKey,
    owner_client_id: String,
    epoch: u64,
) -> OwnerLease {
    owners.bind(key.clone(), owner_client_id, epoch)
}

/// Step 3：声明 following（broadcast v1；已跟随再声明会触发新 snapshot）。
pub async fn declare_following(
    conn: &super::connection::ConnectionHandle,
    key: &ThreadKey,
) -> Result<(), ProtocolError> {
    let version = conn.version_of("thread-stream-following-changed")?;
    conn.broadcast(
        "thread-stream-following-changed",
        version,
        serde_json::json!({
            "conversationId": key.thread_id,
            "hostId": key.host_id,
            "following": true,
        }),
    )
    .await
}

/// Step 4：等 owner 下发的 targeted snapshot（v11）。
///
/// **早快照契约（订阅时序是正确性的一部分）**：tokio broadcast 的新订阅者只收
/// 订阅之后发布的事件（`new_receiver` 从当前 tail 起，无回放）。Desktop 的
/// targeted snapshot 可能在 declare 回执之后、本任务恢复/订阅之前就由 reader
/// 发布——迟到订阅会**永久丢失**唯一快照，15s 后按 Timeout 失败（回归
/// `b1_follow_observes_snapshot_delivered_before_waiter_runs` 的根因）。因此
/// 订阅必须发生在 declare（任何可能触发快照的写入）**之前**：
/// [`discover_and_follow`] 与 adapter `follow_thread` 各自先
/// `conn.subscribe()` 再 declare，然后把已订阅的接收器交给本函数。
pub async fn wait_first_snapshot(
    conn: &super::connection::ConnectionHandle,
    key: &ThreadKey,
) -> Result<(), ProtocolError> {
    let mut rx = conn.subscribe();
    wait_first_snapshot_rx(&mut rx, key).await
}

/// 已订阅形态的 Step 4（订阅时序由调用方保证先于 declare；见
/// [`wait_first_snapshot`] 的早快照契约）。
pub async fn wait_first_snapshot_rx(
    rx: &mut tokio::sync::broadcast::Receiver<super::connection::ConnectionEvent>,
    key: &ThreadKey,
) -> Result<(), ProtocolError> {
    loop {
        match tokio::time::timeout(super::connection::RPC_TIMEOUT, rx.recv()).await {
            Ok(Ok(super::connection::ConnectionEvent::Broadcast { frame, .. })) => {
                if frame.get("method").and_then(|m| m.as_str())
                    == Some("thread-stream-state-changed")
                    && frame["params"]["conversationId"]
                        == serde_json::Value::String(key.thread_id.clone())
                {
                    return Ok(());
                }
            }
            Ok(Ok(_)) => continue,
            Ok(Err(_)) => return Err(ProtocolError::Disconnected("event channel closed".into())),
            Err(_) => {
                return Err(ProtocolError::Timeout(
                    "no snapshot after following declaration".into(),
                ));
            }
        }
    }
}

#[cfg(test)]
type SnapshotWaitTestGate = (
    String,
    tokio::sync::oneshot::Sender<()>,
    tokio::sync::oneshot::Receiver<()>,
);

#[cfg(test)]
static SNAPSHOT_WAIT_TEST_GATE: std::sync::Mutex<Option<SnapshotWaitTestGate>> =
    std::sync::Mutex::new(None);

#[cfg(test)]
async fn test_gate_before_snapshot_wait(key: &ThreadKey) {
    let gate = {
        let mut gate = SNAPSHOT_WAIT_TEST_GATE.lock().unwrap();
        if gate.as_ref().is_some_and(|(id, _, _)| id == &key.thread_id) {
            gate.take()
        } else {
            None
        }
    };
    if let Some((_, entered, resume)) = gate {
        let _ = entered.send(());
        let _ = resume.await;
    }
}

/// 发现 owner 并声明 following，等到首帧 snapshot 才算完成（无 following 不允许
/// 假装同步完成；重连恢复 = 重新 discovery → 重新 following → 等 snapshot）。
/// adapter 等需要分步控制的调用方直接使用四个原语。
///
/// declare 后核验 lease 活性（与 adapter `follow_thread` 的「declare 后复查」同款）：
/// 旧 epoch 连接的 discovery 迟到回执会让 [`bind_lease`] 返回永不生效的死 lease
/// （generation=0），不得把它当成功返回——失效 → `StaleContext`，调用方按失败重试。
pub async fn discover_and_follow(
    conn: &super::connection::ConnectionHandle,
    owners: &mut OwnerRegistry,
    key: &ThreadKey,
) -> Result<OwnerLease, ProtocolError> {
    let owner_id = discover_owner(conn, key).await?;
    let lease = bind_lease(owners, key, owner_id, conn.epoch());
    // 早快照契约：先订阅再 declare（见 wait_first_snapshot 文档）——唯一快照
    // 可能在 declare 回执后、本任务恢复前就被 reader 发布。
    let mut snapshot_rx = conn.subscribe();
    declare_following(conn, key).await?;
    // 死 lease 复查：bind 即死的 lease（旧 epoch 迟到回执）与 declare 窗口内失活的
    // lease 都在此拦下，杜绝 Ok(死 lease)——调用方拿到它做写授权会全军覆没。
    if !owners.is_current(&lease) {
        return Err(ProtocolError::StaleContext(
            "lease lost during follow (old epoch late receipt?)".into(),
        ));
    }
    #[cfg(test)]
    test_gate_before_snapshot_wait(key).await;
    wait_first_snapshot_rx(&mut snapshot_rx, key).await?;
    Ok(lease)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(id: &str) -> ThreadKey {
        ThreadKey {
            host_id: "local".into(),
            thread_id: id.into(),
        }
    }

    #[test]
    fn b1_late_same_epoch_reset_does_not_invalidate_new_lease() {
        let mut owners = OwnerRegistry::default();
        // 与 bind 先隐式重置、连接初始化随后执行同代 reset 的顺序一致。
        let lease = owners.bind(key("t"), "owner".into(), 1);
        let mut validity = owners.validity(&lease);
        let permit = owners.authority(&lease).unwrap().rotate_scope().unwrap();
        owners.reset(1);
        assert!(owners.is_current(&lease));
        assert!(*validity.borrow_and_update());
        assert!(permit.lock().is_ok());
    }
    #[test]
    fn b1_late_older_epoch_reset_cannot_revoke_new_lease() {
        let mut owners = OwnerRegistry::default();
        let lease = owners.bind(key("t"), "owner".into(), 2);
        let mut validity = owners.validity(&lease);
        let permit = owners.authority(&lease).unwrap().rotate_scope().unwrap();
        owners.reset(1);
        assert!(owners.is_current(&lease));
        assert!(*validity.borrow_and_update());
        assert!(permit.lock().is_ok());
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn b1_follow_observes_snapshot_delivered_before_waiter_runs() {
        use crate::services::codex_desktop_ipc::{
            codec,
            connection::{self, ConnectionEvent},
            profile::profile_for,
        };
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        async fn recv(peer: &mut tokio::io::DuplexStream) -> serde_json::Value {
            let mut decoder = codec::FrameDecoder::new();
            let mut bytes = vec![0; 16384];
            loop {
                let n = peer.read(&mut bytes).await.unwrap();
                assert!(n > 0);
                let mut frames = decoder.push(&bytes[..n]).unwrap();
                if !frames.is_empty() {
                    return frames.remove(0);
                }
            }
        }
        async fn send(peer: &mut tokio::io::DuplexStream, frame: serde_json::Value) {
            peer.write_all(&codec::encode_frame(&frame).unwrap())
                .await
                .unwrap();
        }
        let (client, mut peer) = tokio::io::duplex(65536);
        let conn = connection::spawn(
            Box::new(client),
            profile_for("macos", "26.1013.31256").unwrap(),
            1,
        );
        let init = recv(&mut peer).await;
        send(&mut peer, serde_json::json!({"type":"response","requestId":init["requestId"],"resultType":"success","method":"initialize","handledByClientId":"owner","result":{"clientId":"owner"}})).await;
        let thread = key("b1-early-snapshot");
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
        let (resume_tx, resume_rx) = tokio::sync::oneshot::channel();
        *SNAPSHOT_WAIT_TEST_GATE.lock().unwrap() =
            Some((thread.thread_id.clone(), entered_tx, resume_rx));
        let mut tap = conn.subscribe();
        let follow_conn = conn.clone();
        let follow_key = thread.clone();
        let mut follow = tokio::spawn(async move {
            let mut registry = OwnerRegistry::default();
            discover_and_follow(&follow_conn, &mut registry, &follow_key).await
        });
        let discovery = recv(&mut peer).await;
        assert_eq!(discovery["method"], "thread-owner-discovery");
        send(&mut peer, serde_json::json!({"type":"response","requestId":discovery["requestId"],"resultType":"success","method":"thread-owner-discovery","handledByClientId":"owner","result":{"supportsUntrustedAppInput":true}})).await;
        let following = recv(&mut peer).await;
        assert_eq!(following["method"], "thread-stream-following-changed");
        entered_rx.await.unwrap();
        send(&mut peer, serde_json::json!({"type":"broadcast","method":"thread-stream-state-changed","version":11,"sourceClientId":"owner","params":{"conversationId":thread.thread_id,"hostId":"local","change":{"type":"snapshot","revision":7,"conversationState":{}}}})).await;
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            loop {
                if let Ok(ConnectionEvent::Broadcast { frame, .. }) = tap.recv().await {
                    if frame["params"]["change"]["revision"] == 7 {
                        break;
                    }
                }
            }
        })
        .await
        .expect("reader must publish the sole snapshot before waiter resumes");
        resume_tx.send(()).unwrap();
        let result = tokio::time::timeout(std::time::Duration::from_secs(1), &mut follow).await;
        if result.is_err() {
            follow.abort();
            let _ = follow.await;
        }
        conn.shutdown().await;
        assert!(
            matches!(result, Ok(Ok(Ok(_)))),
            "follow must observe the already delivered snapshot"
        );
    }

    #[test]
    fn b1_higher_epoch_bind_revokes_other_threads() {
        let mut r = OwnerRegistry::default();
        let old = r.bind(key("old"), "owner".into(), 1);
        let permit = r.authority(&old).unwrap().rotate_scope().unwrap();
        r.bind(key("new"), "other".into(), 2);
        // A4/T10：撤销后台化——同步测试就地 drain 保持 fencing 即时性（意图不变）。
        for authority in r.take_pending_revocations() {
            authority.revoke_owner();
        }
        assert!(!r.is_current(&old));
        assert!(permit.lock().is_err());
    }

    #[test]
    fn b1_registry_entrypoints_revoke_authority() {
        for mode in 0..4 {
            let mut r = OwnerRegistry::default();
            let lease = r.bind(key("t"), "owner".into(), 1);
            let authority = r.authority(&lease).unwrap();
            let permit = authority.rotate_scope().unwrap();
            match mode {
                0 => {
                    r.bind(key("t"), "new".into(), 1);
                }
                1 => r.invalidate_client("owner"),
                2 => r.release(&key("t")),
                _ => r.reset(2),
            }
            // A4/T10：撤销后台化——同步测试就地 drain 保持 fencing 即时性（意图不变）。
            for authority in r.take_pending_revocations() {
                authority.revoke_owner();
            }
            assert!(permit.lock().is_err(), "entrypoint {mode} did not revoke");
        }
    }

    #[test]
    fn disconnect_invalidates_every_thread_of_owner() {
        let mut r = OwnerRegistry::default();
        let a = r.bind(key("t1"), "owner-a".into(), 1);
        let b = r.bind(key("t2"), "owner-a".into(), 1);
        r.invalidate_client("owner-a");
        assert!(!r.is_current(&a));
        assert!(!r.is_current(&b));
        assert!(r.current(&key("t1")).is_none());
        assert!(r.current(&key("t2")).is_none());
        // 无关 owner 不受影响
        let c = r.bind(key("t3"), "owner-c".into(), 1);
        assert!(r.is_current(&c));
    }

    #[test]
    fn old_epoch_never_becomes_current() {
        let mut r = OwnerRegistry::default();
        let a = r.bind(key("t1"), "owner-a".into(), 1);
        assert!(r.is_current(&a));
        r.reset(2);
        assert!(!r.is_current(&a), "reset 后旧 epoch lease 一律失效");
        assert!(r.current(&key("t1")).is_none());
        // 旧 epoch 的 bind 不能通过 current 复活
        let b = r.bind(key("t2"), "owner-b".into(), 1); // 旧 epoch 再次 bind
        assert!(!r.is_current(&b), "epoch=1 ≠ registry.epoch=2");
        assert!(r.current(&key("t2")).is_none());
    }

    #[test]
    fn old_epoch_bind_does_not_evict_current_binding() {
        let mut r = OwnerRegistry::default();
        // 新连接（epoch=2）先完成 discovery 并绑定
        r.reset(2);
        let fresh = r.bind(key("t1"), "owner-a".into(), 2);
        assert!(r.is_current(&fresh));
        let mut v = r.validity(&fresh);
        assert!(*v.borrow_and_update());

        // 旧连接（epoch=1）的迟到 discovery 回执晚于上面的 bind 到达：不得驱逐
        let stale = r.bind(key("t1"), "owner-a".into(), 1);
        assert!(!r.is_current(&stale), "拒绝返回的死 lease 永不生效");
        assert!(
            !*r.validity(&stale).borrow_and_update(),
            "死 lease 的 validity 恒 false"
        );
        assert_eq!(
            r.current(&key("t1")),
            Some(fresh.clone()),
            "current 仍返回新 epoch 绑定，registry 状态无 epoch 回退"
        );
        assert!(
            *v.borrow_and_update(),
            "新 lease 的 validity 不得被旧 epoch bind 置 false"
        );
        assert_eq!(stale.generation, 0, "拒绝的 bind 不占用 generation");

        // 佐证 registry epoch 仍停在 2：同 epoch 的后续绑定照常生效
        let other = r.bind(key("t2"), "owner-a".into(), 2);
        assert!(r.is_current(&other));
    }

    #[test]
    fn evicted_old_estimate_lease_cannot_release_reacquired_following() {
        let mut set = FollowingSet::default();
        let target = key("target");
        let old = set.acquire_estimate(target.clone()).unwrap();
        for i in 1..MAX_FOLLOWINGS {
            set.acquire_estimate(key(&format!("e{i}"))).unwrap();
        }
        assert_eq!(
            set.reserve_remote(key("remote")).unwrap(),
            Some(target.clone())
        );
        assert!(set.release_purpose(&key("remote"), FollowPurpose::Remote));
        let new = set.acquire_estimate(target.clone()).unwrap();
        assert_ne!(old, new);
        assert_eq!(set.release_estimate(&target, old), (false, false));
        assert!(set.is_estimate_following(&target));
        assert_eq!(set.release_estimate(&target, new), (true, true));
    }

    #[test]
    fn remote_reservation_evicts_estimate_only_at_capacity() {
        let mut set = FollowingSet::default();
        for i in 0..MAX_FOLLOWINGS {
            assert!(set.acquire_purpose(key(&format!("e{i}")), FollowPurpose::TokenizerEstimate));
        }
        let victim = set
            .reserve_remote(key("remote"))
            .expect("remote must have priority")
            .expect("estimate victim");
        assert!(!set.is_following(&victim));
        assert!(set.is_following(&key("remote")));
        assert_eq!(set.len(), MAX_FOLLOWINGS);
    }

    #[test]
    fn remote_and_estimate_followings_release_independently() {
        for first in [FollowPurpose::Remote, FollowPurpose::TokenizerEstimate] {
            let mut set = FollowingSet::default();
            let k = key("shared");
            assert!(set.acquire_purpose(k.clone(), FollowPurpose::Remote));
            assert!(set.acquire_purpose(k.clone(), FollowPurpose::TokenizerEstimate));
            assert!(!set.release_purpose(&k, first));
            assert!(set.is_following(&k));
            let other = if first == FollowPurpose::Remote {
                FollowPurpose::TokenizerEstimate
            } else {
                FollowPurpose::Remote
            };
            assert!(set.release_purpose(&k, other));
            assert!(!set.is_following(&k));
            assert!(!set.release_purpose(&k, other));
        }
    }

    #[test]
    fn generation_is_monotonic_per_thread() {
        let mut r = OwnerRegistry::default();
        let a = r.bind(key("t1"), "owner-a".into(), 1);
        assert_eq!(a.generation, 1);
        let a2 = r.bind(key("t1"), "owner-b".into(), 1);
        assert_eq!(a2.generation, 2, "换代 generation 单调 +1");
        assert!(!r.is_current(&a), "旧 generation 失效");
        r.reset(2);
        let a3 = r.bind(key("t1"), "owner-c".into(), 2);
        assert_eq!(a3.generation, 3, "跨 reset 仍单调");
    }

    #[test]
    fn validity_signal_fires_false_on_invalidate_and_rebind() {
        let mut r = OwnerRegistry::default();
        let a = r.bind(key("t1"), "owner-a".into(), 1);
        let mut v = r.validity(&a);
        assert!(*v.borrow_and_update());
        r.invalidate_client("owner-a");
        assert!(!*v.borrow_and_update(), "invalidate 后信号转 false");

        let a2 = r.bind(key("t1"), "owner-b".into(), 1);
        let mut v_old = r.validity(&a); // 旧 lease 查询
        assert!(!*v_old.borrow_and_update());
        let mut v2 = r.validity(&a2);
        assert!(*v2.borrow_and_update());
        let _ = r.bind(key("t1"), "owner-c".into(), 1); // 换代
        assert!(!*v2.borrow_and_update(), "换代后旧 lease 信号 false");
    }

    #[test]
    fn following_set_is_bounded_and_releases_slots() {
        let mut s = FollowingSet::default();
        for i in 0..MAX_FOLLOWINGS {
            assert!(s.follow(key(&format!("t{i}"))));
        }
        assert!(!s.follow(key("overflow")), "满员拒绝，不挤既有跟随");
        assert_eq!(s.len(), MAX_FOLLOWINGS);
        assert!(s.is_following(&key("t0")));
        assert!(s.unfollow(&key("t0")), "退出释放名额");
        assert!(s.follow(key("overflow")), "释放后可再跟随");
        assert_eq!(s.len(), MAX_FOLLOWINGS);
        assert!(!s.unfollow(&key("不存在的")), "移除不存在的键返回 false");
        assert_eq!(s.len(), MAX_FOLLOWINGS);
    }
}
