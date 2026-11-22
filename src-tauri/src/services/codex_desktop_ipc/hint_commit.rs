use super::types::OwnerLease;
use std::sync::{Arc, Mutex, MutexGuard};

pub(crate) struct AuthorityState {
    owner_valid: bool,
    current_mirror_scope: Option<u64>,
    next_scope: u64,
}
pub(crate) struct HintLeaseAuthority {
    lease: OwnerLease,
    gate: Mutex<AuthorityState>,
    #[cfg(test)]
    revoke_contended: std::sync::atomic::AtomicBool,
}
#[derive(Clone)]
pub struct HintCommitPermit {
    authority: Arc<HintLeaseAuthority>,
    scope: u64,
    /// 终审 Blocker 2：scope 撤销的**即时**栅栏——`revoke_scope` 只置位本原子
    /// （不取 gate、不阻塞），`lock()` 每次提交前读取；已持 guard 的在飞事务
    /// 按 ticket 收尾，未开始的提交立即被 fence（不再依赖 try_lock 成功）。
    revoked: Arc<std::sync::atomic::AtomicBool>,
}
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Fence {
    Owner,
    Scope,
}
impl HintLeaseAuthority {
    pub(crate) fn new(lease: OwnerLease) -> Arc<Self> {
        Arc::new(Self {
            lease,
            #[cfg(test)]
            revoke_contended: std::sync::atomic::AtomicBool::new(false),
            gate: Mutex::new(AuthorityState {
                owner_valid: true,
                current_mirror_scope: None,
                next_scope: 0,
            }),
        })
    }
    #[cfg(test)]
    pub(crate) fn rotate_scope(self: &Arc<Self>) -> Option<HintCommitPermit> {
        let mut state = self.gate.lock().ok()?;
        Self::rotate_locked(self, &mut state)
    }

    /// 终审 Blocker 2：**非阻塞**换代（镜像/转发器持锁路径用）——gate 正被在飞
    /// 提交持有时返回 None（调用方保留「无 permit」保守态，下一张状态帧重试），
    /// 绝不在转发器/镜像临界区内等待慢 COMMIT。
    pub(crate) fn try_rotate_scope(self: &Arc<Self>) -> Option<HintCommitPermit> {
        let mut state = self.gate.try_lock().ok()?;
        Self::rotate_locked(self, &mut state)
    }

    fn rotate_locked(
        self: &Arc<Self>,
        state: &mut std::sync::MutexGuard<'_, AuthorityState>,
    ) -> Option<HintCommitPermit> {
        if !state.owner_valid {
            return None;
        }
        state.current_mirror_scope = None;
        let scope = state.next_scope.checked_add(1)?;
        state.next_scope = scope;
        state.current_mirror_scope = Some(scope);
        Some(HintCommitPermit {
            authority: self.clone(),
            scope,
            revoked: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        })
    }
    pub(crate) fn revoke_owner(&self) {
        #[cfg(test)]
        if matches!(
            self.gate.try_lock(),
            Err(std::sync::TryLockError::WouldBlock)
        ) {
            self.revoke_contended
                .store(true, std::sync::atomic::Ordering::Release);
        }
        let mut state = self.gate.lock().unwrap_or_else(|e| e.into_inner());
        state.owner_valid = false;
        state.current_mirror_scope = None;
    }
    #[cfg(test)]
    pub(crate) fn test_revoke_contended(&self) -> bool {
        self.revoke_contended
            .load(std::sync::atomic::Ordering::Acquire)
    }
    #[cfg(test)]
    fn set_next_scope_for_test(&self, value: u64) {
        self.gate.lock().unwrap().next_scope = value;
    }
}
impl HintCommitPermit {
    #[cfg(test)]
    pub(crate) fn test_gate_held(&self) -> bool {
        matches!(
            self.authority.gate.try_lock(),
            Err(std::sync::TryLockError::WouldBlock)
        )
    }
    #[cfg(test)]
    pub(crate) fn test_owner_fenced_nonblocking(&self) -> Result<bool, &'static str> {
        match self.authority.gate.try_lock() {
            Ok(state) => Ok(!state.owner_valid),
            Err(std::sync::TryLockError::WouldBlock) => Err("gate_would_block"),
            Err(std::sync::TryLockError::Poisoned(_)) => Err("gate_poisoned"),
        }
    }
    pub(crate) fn revoke_scope(&self) {
        // A4/T10 终审 Blocker 2：**即时**栅栏——只置本 permit 的原子标志（不取
        // gate、绝不阻塞转发器/镜像临界区）。已持 guard 的在飞事务按 ticket 收尾，
        // 未开始的提交在 `lock()` 首个检查点被 Fence::Scope 拒绝。
        self.revoked
            .store(true, std::sync::atomic::Ordering::Release);
        // 尽力同步登记表（gate 空闲时）；占用中不影响栅栏正确性——上面已 fence。
        if let Ok(mut state) = self.authority.gate.try_lock() {
            if state.current_mirror_scope == Some(self.scope) {
                state.current_mirror_scope = None;
            }
        }
    }
    pub(crate) fn lock(&self) -> Result<MutexGuard<'_, AuthorityState>, Fence> {
        // 终审 Blocker 2：撤销栅栏先于 gate 等待判定——被撤销的 permit 即使
        // gate 稍后空闲也不得再取得提交资格。
        if self.revoked.load(std::sync::atomic::Ordering::Acquire) {
            return Err(Fence::Scope);
        }
        let state = self.authority.gate.lock().map_err(|_| Fence::Owner)?;
        if self.revoked.load(std::sync::atomic::Ordering::Acquire) {
            return Err(Fence::Scope);
        }
        if !state.owner_valid {
            return Err(Fence::Owner);
        }
        if state.current_mirror_scope != Some(self.scope) {
            return Err(Fence::Scope);
        }
        Ok(state)
    }
    /// 终审 Blocker 2：**非阻塞**栅栏探测——镜像锁内/转发器路径用（不取 gate）。
    /// 撤销原子、owner 失效与 scope 变更任一成立即视为已 fence。
    pub(crate) fn is_fenced_nonblocking(&self) -> bool {
        if self.revoked.load(std::sync::atomic::Ordering::Acquire) {
            return true;
        }
        match self.authority.gate.try_lock() {
            Ok(state) => !state.owner_valid || state.current_mirror_scope != Some(self.scope),
            Err(std::sync::TryLockError::WouldBlock) => {
                // gate 忙 = 在飞提交；本检查只是资格淘汰，保守按「未 fence」放行
                //（真正提交仍会在 lock() 处被权威复核）。
                false
            }
            Err(std::sync::TryLockError::Poisoned(_)) => true,
        }
    }

    pub(crate) fn scope_id(&self) -> u64 {
        self.scope
    }
    pub(crate) fn lease(&self) -> &OwnerLease {
        &self.authority.lease
    }
}

#[cfg(test)]
mod tests {
    use super::super::types::ThreadKey;
    use super::*;
    fn authority() -> Arc<HintLeaseAuthority> {
        HintLeaseAuthority::new(OwnerLease {
            key: ThreadKey {
                host_id: "h".into(),
                thread_id: "t".into(),
            },
            client_id: "c".into(),
            epoch: 1,
            generation: 1,
        })
    }
    #[test]
    fn b1_old_scope_cleanup_does_not_revoke_new_scope() {
        let a = authority();
        let old = a.rotate_scope().unwrap();
        let new = a.rotate_scope().unwrap();
        old.revoke_scope();
        assert_eq!(old.lock().err(), Some(Fence::Scope));
        assert!(new.lock().is_ok());
    }
    #[test]
    fn b1_owner_revocation_blocks_commit() {
        let a = authority();
        let p = a.rotate_scope().unwrap();
        a.revoke_owner();
        assert_eq!(p.lock().err(), Some(Fence::Owner));
    }
    #[test]
    fn b1_scope_never_reuses_and_overflow_fences() {
        let a = authority();
        let old = a.rotate_scope().unwrap();
        let next = a.rotate_scope().unwrap();
        assert_ne!(old.scope, next.scope);
        a.set_next_scope_for_test(u64::MAX);
        assert!(a.rotate_scope().is_none());
        assert!(next.lock().is_err());
    }
    #[test]
    fn b1_commit_before_revocation_is_ordered() {
        use std::sync::mpsc;
        let a = authority();
        let p = a.rotate_scope().unwrap();
        let guard = p.lock().unwrap();
        let (started_tx, started_rx) = mpsc::channel();
        let (done_tx, done_rx) = mpsc::channel();
        let other = a.clone();
        let thread = std::thread::spawn(move || {
            started_tx.send(()).unwrap();
            other.revoke_owner();
            done_tx.send(()).unwrap();
        });
        started_rx.recv().unwrap();
        assert!(done_rx.try_recv().is_err());
        drop(guard);
        done_rx.recv().unwrap();
        thread.join().unwrap();
        assert_eq!(p.lock().err(), Some(Fence::Owner));
    }
}

#[cfg(test)]
#[path = "hint_commit_sqlite_tests.rs"]
mod sqlite_tests;
