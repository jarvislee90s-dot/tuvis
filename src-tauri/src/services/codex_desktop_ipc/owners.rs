//! Owner Registry：`(hostId, threadId) → current ownerClientId` 的双向索引与恢复代际。
//!
//! owner 是临时身份（spec F6/3.2）：断开、Desktop 重启、`client-status-changed`
//! (disconnected) 都使映射全部失效并触发重新 discovery；旧 owner 的迟到数据不得
//! 复用（epoch/generation 双代际拒绝）。
//!
//! 同步结构、无 await：跟随管理属于上层单 actor（adapter），不在持锁期间等待。

use super::types::{OwnerLease, ProtocolError, ThreadKey};
use std::collections::{HashMap, HashSet};
use tokio::sync::watch;

/// 单连接可跟随 Thread 上限（计划「有界资源」）。
pub const MAX_FOLLOWINGS: usize = 64;

struct LeaseEntry {
    client_id: String,
    epoch: u64,
    generation: u64,
    validity_tx: watch::Sender<bool>,
}

/// owner 租约登记表。generation 按 Thread 单调增加；reset 后 epoch 增加。
#[derive(Default)]
pub struct OwnerRegistry {
    epoch: u64,
    threads: HashMap<ThreadKey, LeaseEntry>,
    by_client: HashMap<String, HashSet<ThreadKey>>,
    gens: HashMap<ThreadKey, u64>,
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
            self.epoch = epoch;
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
        if let Some(old) = self.threads.remove(&key) {
            let _ = old.validity_tx.send(false);
            if let Some(set) = self.by_client.get_mut(&old.client_id) {
                set.remove(&key);
                if set.is_empty() {
                    self.by_client.remove(&old.client_id);
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
    pub fn invalidate_client(&mut self, client_id: &str) {
        let Some(keys) = self.by_client.remove(client_id) else {
            return;
        };
        for key in keys {
            if let Some(entry) = self.threads.remove(&key) {
                let _ = entry.validity_tx.send(false);
            }
        }
    }

    /// 单 thread 主动退场（2026-10-17 复审 C4：unfollow 最后一引用退出时回收——
    /// lease 移除 + validity false，后续该 owner 的帧不再有授权可套）。
    pub fn release(&mut self, key: &ThreadKey) {
        if let Some(old) = self.threads.remove(key) {
            let _ = old.validity_tx.send(false);
            if let Some(set) = self.by_client.get_mut(&old.client_id) {
                set.remove(key);
                if set.is_empty() {
                    self.by_client.remove(&old.client_id);
                }
            }
        }
    }

    /// 重连/Desktop 重启：清全部 lease（epoch 由 supervisor 递增），generation
    /// 记忆保留（跨 reset 仍单调）。
    pub fn reset(&mut self, new_epoch: u64) {
        self.epoch = new_epoch;
        for (_, entry) in self.threads.drain() {
            let _ = entry.validity_tx.send(false);
        }
        self.by_client.clear();
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

/// 跟随集合（有界 64；adapter 消费——看板/详情/有待发消息的 Thread 才跟随）。
#[derive(Default)]
pub struct FollowingSet {
    keys: Vec<ThreadKey>,
}

impl FollowingSet {
    pub fn is_following(&self, key: &ThreadKey) -> bool {
        self.keys.contains(key)
    }

    /// 满员返回 false（不挤掉既有跟随）。
    pub fn follow(&mut self, key: ThreadKey) -> bool {
        if self.keys.contains(&key) {
            return true;
        }
        if self.keys.len() >= MAX_FOLLOWINGS {
            return false;
        }
        self.keys.push(key);
        true
    }

    /// 退出跟随（释放名额）。
    pub fn unfollow(&mut self, key: &ThreadKey) -> bool {
        let before = self.keys.len();
        self.keys.retain(|k| k != key);
        self.keys.len() != before
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
pub async fn wait_first_snapshot(
    conn: &super::connection::ConnectionHandle,
    key: &ThreadKey,
) -> Result<(), ProtocolError> {
    let mut rx = conn.subscribe();
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
    declare_following(conn, key).await?;
    // 死 lease 复查：bind 即死的 lease（旧 epoch 迟到回执）与 declare 窗口内失活的
    // lease 都在此拦下，杜绝 Ok(死 lease)——调用方拿到它做写授权会全军覆没。
    if !owners.is_current(&lease) {
        return Err(ProtocolError::StaleContext(
            "lease lost during follow (old epoch late receipt?)".into(),
        ));
    }
    wait_first_snapshot(conn, key).await?;
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
