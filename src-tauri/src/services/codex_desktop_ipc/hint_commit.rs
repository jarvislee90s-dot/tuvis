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
}
#[derive(Clone)]
pub struct HintCommitPermit {
    authority: Arc<HintLeaseAuthority>,
    scope: u64,
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
            gate: Mutex::new(AuthorityState {
                owner_valid: true,
                current_mirror_scope: None,
                next_scope: 0,
            }),
        })
    }
    pub(crate) fn rotate_scope(self: &Arc<Self>) -> Option<HintCommitPermit> {
        let mut state = self.gate.lock().ok()?;
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
        })
    }
    pub(crate) fn revoke_owner(&self) {
        let mut state = self.gate.lock().unwrap_or_else(|e| e.into_inner());
        state.owner_valid = false;
        state.current_mirror_scope = None;
    }
    #[cfg(test)]
    fn set_next_scope_for_test(&self, value: u64) {
        self.gate.lock().unwrap().next_scope = value;
    }
}
impl HintCommitPermit {
    pub(crate) fn revoke_scope(&self) {
        if let Ok(mut state) = self.authority.gate.lock() {
            if state.current_mirror_scope == Some(self.scope) {
                state.current_mirror_scope = None;
            }
        }
    }
    pub(crate) fn lock(&self) -> Result<MutexGuard<'_, AuthorityState>, Fence> {
        let state = self.authority.gate.lock().map_err(|_| Fence::Owner)?;
        if !state.owner_valid {
            return Err(Fence::Owner);
        }
        if state.current_mirror_scope != Some(self.scope) {
            return Err(Fence::Scope);
        }
        Ok(state)
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
