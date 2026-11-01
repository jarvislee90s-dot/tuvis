use super::super::types::{TargetKey, ZResult};
use super::{
    host::QueueHostPort,
    store::SqlLocalQueueStore,
    types::{EnqueueCommand, EnqueueReceipt},
};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use tokio::sync::Notify;

pub(crate) trait QueueRuntimeGate: Send + Sync {
    fn remote_enabled(&self) -> bool;
    fn device_valid(&self, device_id: &str) -> bool;
    fn shutting_down(&self) -> bool;
}
enum FlushFailure {
    Preflight(super::super::types::ZcodeError),
    Other(super::super::types::ZcodeError),
}
impl From<super::super::types::ZcodeError> for FlushFailure {
    fn from(error: super::super::types::ZcodeError) -> Self {
        Self::Other(error)
    }
}
impl FlushFailure {
    fn into_error(self) -> super::super::types::ZcodeError {
        match self {
            Self::Preflight(e) | Self::Other(e) => e,
        }
    }
}
fn preflight_failure(error: super::super::types::ZcodeError) -> FlushFailure {
    if error.is(super::super::types::codes::NOT_READY)
        || error.is(super::super::types::codes::OFFLINE)
    {
        FlushFailure::Preflight(error)
    } else {
        FlushFailure::Other(error)
    }
}
pub(crate) struct ZcodeQueueService {
    store: SqlLocalQueueStore,
    host: Arc<dyn QueueHostPort>,
    gate: Arc<dyn QueueRuntimeGate>,
    started: AtomicBool,
    cleanup_started: AtomicBool,
    stopped: AtomicBool,
    wake: Notify,
    cleanup_wake: Notify,
    shutdown_wake: Notify,
    activation_wake: Notify,
    activation_lock: tokio::sync::Mutex<()>,
    activation_requests: std::sync::atomic::AtomicU64,
    activation_processed: std::sync::atomic::AtomicU64,
    activation_force_requests: std::sync::atomic::AtomicU64,
    #[cfg(test)]
    activation_invocations: std::sync::atomic::AtomicUsize,
    observations: std::sync::Mutex<
        std::collections::BTreeMap<String, Vec<super::types::ExecutionObservation>>,
    >,
    recovered: tokio::sync::Mutex<bool>,
}
impl ZcodeQueueService {
    pub(crate) fn new(
        store: SqlLocalQueueStore,
        host: Arc<dyn QueueHostPort>,
        gate: Arc<dyn QueueRuntimeGate>,
    ) -> Self {
        Self {
            store,
            host,
            gate,
            started: AtomicBool::new(false),
            cleanup_started: AtomicBool::new(false),
            stopped: AtomicBool::new(false),
            wake: Notify::new(),
            cleanup_wake: Notify::new(),
            shutdown_wake: Notify::new(),
            activation_wake: Notify::new(),
            activation_lock: tokio::sync::Mutex::new(()),
            activation_requests: std::sync::atomic::AtomicU64::new(0),
            activation_processed: std::sync::atomic::AtomicU64::new(0),
            activation_force_requests: std::sync::atomic::AtomicU64::new(0),
            #[cfg(test)]
            activation_invocations: std::sync::atomic::AtomicUsize::new(0),
            observations: std::sync::Mutex::new(std::collections::BTreeMap::new()),
            recovered: tokio::sync::Mutex::new(false),
        }
    }
    pub(crate) fn store(&self) -> SqlLocalQueueStore {
        self.store.clone()
    }
    pub(crate) async fn recovery_ready(&self) -> bool {
        *self.recovered.lock().await
    }
    pub(crate) async fn recover(&self) -> ZResult<()> {
        let mut recovered = self.recovered.lock().await;
        if *recovered {
            return Ok(());
        }
        let claims = self.store.recover(chrono::Utc::now().timestamp_millis())?;
        for recovery in claims {
            let claim = recovery.dispatch;
            if let Some(receipt) = self
                .host
                .receipt(&claim.ctx.action_id, &claim.ctx.device_id)
                .await?
            {
                if matches!(
                    receipt.state,
                    super::super::types::ActionState::Accepted
                        | super::super::types::ActionState::Rejected
                        | super::super::types::ActionState::Noop
                        | super::super::types::ActionState::SendFailed
                        | super::super::types::ActionState::Unknown
                ) {
                    self.store.settle_dispatch(
                        &claim,
                        &receipt,
                        chrono::Utc::now().timestamp_millis(),
                    )?;
                }
            }
        }
        self.store
            .mark_restart_unconfirmed(chrono::Utc::now().timestamp_millis())?;
        *recovered = true;
        Ok(())
    }
    pub(crate) fn start_cleanup(self: &Arc<Self>) {
        if self.cleanup_started.swap(true, Ordering::SeqCst) || self.stopped.load(Ordering::SeqCst)
        {
            return;
        }
        let cleaner = self.clone();
        tokio::spawn(async move {
            loop {
                if cleaner.stopped.load(Ordering::SeqCst) {
                    break;
                }
                let now = chrono::Utc::now().timestamp_millis();
                let deadline = match cleaner
                    .store
                    .cleanup_expired_edits(now)
                    .and_then(|_| cleaner.store.next_edit_expiry())
                {
                    Ok(deadline) => deadline,
                    Err(_) => {
                        log::warn!("queue edit snapshot cleanup failed");
                        Some(now.saturating_add(1000))
                    }
                };
                let delay = deadline.map(|at| {
                    std::time::Duration::from_millis(
                        at.saturating_sub(chrono::Utc::now().timestamp_millis())
                            .max(0) as u64,
                    )
                });
                tokio::select! {
                    _ = cleaner.cleanup_wake.notified() => {},
                    _ = cleaner.store.expiry_changed().notified() => {},
                    _ = async { match delay { Some(d) => tokio::time::sleep(d).await, None => std::future::pending::<()>().await } } => {},
                }
            }
        });
    }
    pub(crate) fn start(self: &Arc<Self>) {
        if self.started.swap(true, Ordering::SeqCst) || self.stopped.load(Ordering::SeqCst) {
            return;
        }
        self.start_cleanup();
        let service = self.clone();
        tokio::spawn(async move {
            let mut delay = std::time::Duration::from_millis(100);
            loop {
                if service.stopped.load(Ordering::SeqCst) {
                    break;
                }
                let mut retry = false;
                match service.store.pending_targets() {
                    Ok(keys) => {
                        for key in keys {
                            if service.stopped.load(Ordering::SeqCst) {
                                break;
                            }
                            match service.flush_one_inner(&key).await {
                                Ok(()) => {}
                                Err(FlushFailure::Preflight(_)) => retry = true,
                                Err(FlushFailure::Other(_)) => {
                                    log::warn!("queue flush failed; no automatic retry")
                                }
                            }
                        }
                    }
                    Err(_) => {
                        log::warn!("queue pending target read failed");
                        retry = true;
                    }
                }
                if retry
                    && !service.stopped.load(Ordering::SeqCst)
                    && service.gate.remote_enabled()
                    && !service.gate.shutting_down()
                {
                    tokio::select! { _ = service.wake.notified() => {}, _ = tokio::time::sleep(delay) => {} }
                    delay = delay
                        .saturating_mul(2)
                        .min(std::time::Duration::from_secs(5));
                } else {
                    delay = std::time::Duration::from_millis(100);
                    service.wake.notified().await;
                }
            }
        });
    }
    #[cfg(test)]
    pub(crate) fn activation_call_count(&self) -> usize {
        self.activation_invocations.load(Ordering::SeqCst)
    }
    #[cfg(test)]
    pub(crate) fn is_stopped(&self) -> bool {
        self.stopped.load(Ordering::SeqCst)
    }
    pub(crate) fn shutdown(&self) {
        self.stopped.store(true, Ordering::SeqCst);
        self.wake.notify_one();
        self.cleanup_wake.notify_one();
        self.shutdown_wake.notify_one();
        self.activation_wake.notify_one();
    }
    pub(crate) async fn wait_shutdown(&self) {
        if !self.stopped.load(Ordering::SeqCst) {
            self.shutdown_wake.notified().await;
        }
    }
    pub(crate) fn wake_all(&self) {
        self.wake.notify_one();
        self.cleanup_wake.notify_one();
        self.activation_wake.notify_one();
    }
    pub(crate) fn notify(&self, _key: &TargetKey) {
        self.wake.notify_one();
        self.cleanup_wake.notify_one();
    }
    pub(crate) async fn enqueue(&self, cmd: EnqueueCommand) -> ZResult<EnqueueReceipt> {
        let key = TargetKey {
            session_id: cmd.session_id.clone(),
        };
        let lock = self.host.session_lock(&key);
        let _guard = lock.lock().await;
        let result = self
            .store
            .enqueue(&cmd, chrono::Utc::now().timestamp_millis());
        if result.is_ok() {
            self.notify(&key);
        }
        result
    }
    fn session_observations(
        view: &super::super::types::SessionView,
    ) -> Vec<super::types::ExecutionObservation> {
        use super::types::ExecutionObservation;
        let Some(revision) = view.revision.filter(|_| view.ready) else {
            return Vec::new();
        };
        let mut observations = Vec::new();
        if let Some(control) = &view.control {
            for work in &control.active_works {
                if let Some(input_id) = &work.source_command_id {
                    observations.push(ExecutionObservation {
                        input_id: input_id.clone(),
                        execution_id: work.foreground_execution_id.clone(),
                        terminal: false,
                        revision,
                        generation: view.generation,
                    });
                }
            }
        }
        if let Some(terminal) = &view.execution_terminal {
            if terminal.revision == revision {
                observations.push(ExecutionObservation {
                    input_id: terminal.input_id.clone(),
                    execution_id: None,
                    terminal: true,
                    revision,
                    generation: view.generation,
                });
            }
        }
        observations
    }
    pub(crate) fn request_connection_ready(self: &Arc<Self>) {
        self.activation_force_requests
            .fetch_add(1, Ordering::SeqCst);
        self.activation_wake.notify_one();
        let service = self.clone();
        tokio::spawn(async move {
            service.on_connected().await;
        });
    }
    pub(crate) async fn on_connected(&self) {
        #[cfg(test)]
        self.activation_invocations.fetch_add(1, Ordering::SeqCst);
        let request = self.activation_requests.fetch_add(1, Ordering::SeqCst) + 1;
        self.activation_wake.notify_one();
        let Ok(mut guard) = self.activation_lock.try_lock() else {
            return;
        };
        if self.activation_processed.load(Ordering::SeqCst) >= request {
            return;
        }
        let mut activated = std::collections::BTreeSet::new();
        let mut epoch = self.host.activation_epoch();
        let mut seen_force = self.activation_force_requests.load(Ordering::SeqCst);
        let mut seen_request = request;
        let mut delay = std::time::Duration::from_millis(100);
        loop {
            if self.stopped.load(Ordering::SeqCst)
                || self.gate.shutting_down()
                || !self.gate.remote_enabled()
            {
                return;
            }
            let current_request = self.activation_requests.load(Ordering::SeqCst);
            let current_epoch = self.host.activation_epoch();
            let current_force = self.activation_force_requests.load(Ordering::SeqCst);
            if current_epoch != epoch
                || current_force != seen_force
                || (current_epoch.is_none() && current_request != seen_request)
            {
                activated.clear();
                epoch = current_epoch;
                delay = std::time::Duration::from_millis(100);
            }
            seen_force = current_force;
            seen_request = current_request;
            let mut retry = false;
            match self.store.pending_targets() {
                Ok(keys) => {
                    for key in keys {
                        if self.stopped.load(Ordering::SeqCst)
                            || self.gate.shutting_down()
                            || !self.gate.remote_enabled()
                        {
                            return;
                        }
                        if activated.contains(&key.session_id) {
                            continue;
                        }
                        let before = self.host.activation_epoch();
                        let result = self
                            .host
                            .activate(&key, std::time::Duration::from_secs(2))
                            .await;
                        if self.host.activation_epoch() != before {
                            activated.clear();
                            epoch = self.host.activation_epoch();
                            retry = true;
                            break;
                        }
                        match result {
                            Ok(()) => {
                                activated.insert(key.session_id);
                            }
                            Err(error)
                                if error.is(super::super::types::codes::NO_SESSION)
                                    || error.is(super::super::types::codes::TARGET_AMBIGUOUS) =>
                            {
                                let _ = self.store.pause_target_mismatch(
                                    &key,
                                    chrono::Utc::now().timestamp_millis(),
                                );
                                activated.insert(key.session_id);
                            }
                            Err(_) => retry = true,
                        }
                    }
                }
                Err(_) => retry = true,
            }
            self.wake.notify_one();
            self.cleanup_wake.notify_one();
            if !retry {
                self.activation_processed
                    .store(current_request, Ordering::SeqCst);
                if self.activation_requests.load(Ordering::SeqCst) != current_request {
                    continue;
                }
                drop(guard);
                if self.activation_requests.load(Ordering::SeqCst) != current_request {
                    match self.activation_lock.try_lock() {
                        Ok(next) => {
                            guard = next;
                            continue;
                        }
                        Err(_) => return,
                    }
                }
                return;
            }
            tokio::select! {
                _ = self.activation_wake.notified() => {},
                _ = tokio::time::sleep(delay) => {},
            }
            delay = delay
                .saturating_mul(2)
                .min(std::time::Duration::from_secs(5));
        }
    }
    pub(crate) fn on_session(&self, view: &super::super::types::SessionView) {
        let observations = Self::session_observations(view);
        if !observations.is_empty() {
            let mut pending = self.observations.lock().unwrap_or_else(|e| e.into_inner());
            if pending.len() < 256 || pending.contains_key(&view.session_id) {
                let items = pending.entry(view.session_id.clone()).or_default();
                for observation in observations {
                    if let Some(existing) = items.iter_mut().find(|item| {
                        item.generation == observation.generation
                            && item.input_id == observation.input_id
                    }) {
                        if observation.revision > existing.revision
                            || (observation.revision == existing.revision
                                && existing.terminal
                                && !observation.terminal)
                        {
                            *existing = observation;
                        }
                    } else if items.len() < 256 {
                        items.push(observation);
                    }
                }
            }
        }
        self.notify(&TargetKey {
            session_id: view.session_id.clone(),
        });
    }
    fn observe_bound(
        &self,
        key: &TargetKey,
        bound: &super::host::BoundQueueSession,
    ) -> ZResult<()> {
        if !bound.view.ready
            || bound.target.session_id != key.session_id
            || bound.view.session_id != key.session_id
            || bound.view.generation != bound.target.generation
        {
            return Ok(());
        }
        let Some(revision) = bound.view.revision else {
            return Ok(());
        };
        if !self.store.matches_execution_target(key, &bound.target)? {
            self.store.mark_changed_execution_target(
                key,
                &bound.target,
                chrono::Utc::now().timestamp_millis(),
            )?;
            self.observations
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&key.session_id);
            return Ok(());
        }
        let mut observations = {
            let mut pending = self.observations.lock().unwrap_or_else(|e| e.into_inner());
            let items = pending.entry(key.session_id.clone()).or_default();
            let mut eligible = Vec::new();
            items.retain(|item| {
                if item.generation != bound.target.generation {
                    return false;
                }
                if item.revision <= revision {
                    eligible.push(item.clone());
                    false
                } else {
                    true
                }
            });
            if items.is_empty() {
                pending.remove(&key.session_id);
            }
            eligible
        };
        observations.extend(Self::session_observations(&bound.view));
        let mut latest =
            std::collections::BTreeMap::<String, super::types::ExecutionObservation>::new();
        for observation in observations {
            match latest.entry(observation.input_id.clone()) {
                std::collections::btree_map::Entry::Vacant(entry) => {
                    entry.insert(observation);
                }
                std::collections::btree_map::Entry::Occupied(mut entry) => {
                    let existing = entry.get();
                    if observation.revision > existing.revision
                        || (observation.revision == existing.revision
                            && existing.terminal
                            && !observation.terminal)
                    {
                        entry.insert(observation);
                    }
                }
            }
        }
        for observation in latest.values() {
            if self.store.observe_execution(key, observation)? {
                self.wake.notify_one();
            }
        }
        Ok(())
    }
    pub(crate) async fn flush_one(&self, key: &TargetKey) -> ZResult<()> {
        self.flush_one_inner(key)
            .await
            .map_err(FlushFailure::into_error)
    }
    async fn flush_one_inner(&self, key: &TargetKey) -> Result<(), FlushFailure> {
        let lock = self.host.session_lock(key);
        let _guard = lock.lock().await;
        if self.stopped.load(Ordering::SeqCst)
            || !self.gate.remote_enabled()
            || self.gate.shutting_down()
        {
            return Ok(());
        }
        let Some(device) = self.store.head_device(key)? else {
            return Ok(());
        };
        if !self.gate.device_valid(&device) {
            return Ok(());
        }
        let bound = self
            .host
            .read(key, std::time::Duration::from_secs(2))
            .await
            .map_err(preflight_failure)?;
        let v = &bound.view;
        if !v.ready {
            return Err(FlushFailure::Preflight(
                super::super::types::ZcodeError::of(super::super::types::codes::NOT_READY),
            ));
        }
        if v.session_id != key.session_id
            || v.generation != bound.target.generation
            || !v.ready
            || !v.capabilities.send.enabled
            || !v.pending.is_empty()
            || v.control
                .as_ref()
                .is_none_or(|c| !c.active_works.is_empty())
            || !v.queue.known
            || v.queue.paused != Some(false)
            || !v.queue.items.is_empty()
        {
            return Ok(());
        }
        let fresh_target = match self
            .host
            .refresh_target(key, std::time::Duration::from_secs(2))
            .await
        {
            Ok(target) => target,
            Err(error)
                if error.is(super::super::types::codes::NO_SESSION)
                    || error.is(super::super::types::codes::TARGET_AMBIGUOUS) =>
            {
                self.store
                    .mark_unprovable_execution_target(key, chrono::Utc::now().timestamp_millis())?;
                self.store
                    .pause_target_mismatch(key, chrono::Utc::now().timestamp_millis())?;
                self.observations
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(&key.session_id);
                return Ok(());
            }
            Err(error) => return Err(preflight_failure(error)),
        };
        if fresh_target != bound.target {
            self.store.mark_changed_execution_target(
                key,
                &fresh_target,
                chrono::Utc::now().timestamp_millis(),
            )?;
            self.store
                .pause_target_mismatch(key, chrono::Utc::now().timestamp_millis())?;
            self.observations
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&key.session_id);
            return Ok(());
        }
        self.observe_bound(key, &bound)?;
        if !self.store.head_matches_target(key, &bound.target)? {
            self.store
                .pause_target_mismatch(key, chrono::Utc::now().timestamp_millis())?;
            return Ok(());
        }
        let Some(claim) =
            self.store
                .claim_next(key, &bound.target, chrono::Utc::now().timestamp_millis())?
        else {
            return Ok(());
        };
        let stopping = self.stopped.load(Ordering::SeqCst) || self.gate.shutting_down();
        let remote_enabled = self.gate.remote_enabled();
        let device_valid =
            claim.ctx.device_id == device && self.gate.device_valid(&claim.ctx.device_id);
        let receipt = if !stopping && remote_enabled && device_valid {
            match self
                .host
                .send_prepared(&claim.ctx, &claim.target, &claim.prepared, None)
                .await
            {
                Ok(receipt) => receipt,
                Err(error) => {
                    let (state, reason) = match error {
                        super::super::command::PreparedDispatchError::BeforeWrite(error) => {
                            (super::super::types::ActionState::SendFailed, error.code)
                        }
                        super::super::command::PreparedDispatchError::MayHaveWritten(error) => {
                            (super::super::types::ActionState::Unknown, error.code)
                        }
                    };
                    super::super::types::ActionReceipt {
                        action_id: claim.ctx.action_id.clone(),
                        session_id: claim.target.session_id.clone(),
                        generation: claim.target.generation,
                        state,
                        reason_code: Some(reason),
                        send_admission: None,
                    }
                }
            }
        } else {
            super::super::types::ActionReceipt {
                action_id: claim.ctx.action_id.clone(),
                session_id: claim.target.session_id.clone(),
                generation: claim.target.generation,
                state: super::super::types::ActionState::SendFailed,
                reason_code: Some(
                    if stopping || !remote_enabled {
                        super::super::types::codes::NOT_READY
                    } else {
                        super::super::types::codes::NO_SESSION
                    }
                    .to_owned(),
                ),
                send_admission: None,
            }
        };
        self.store
            .settle_dispatch(&claim, &receipt, chrono::Utc::now().timestamp_millis())?;
        if receipt.state == super::super::types::ActionState::Accepted {
            self.wake.notify_one();
        }
        Ok(())
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote::pairing::DeviceStore;
    use crate::services::zcode_control::{
        command::PreparedSend,
        local_queue::{
            host::BoundQueueSession,
            types::{QueueActor, QueueDraft},
        },
        types::*,
    };
    use futures::future::BoxFuture;
    use std::{sync::Mutex, time::Duration};
    struct Fake {
        sent: Mutex<Vec<String>>,
        busy: Mutex<bool>,
        lock: Arc<tokio::sync::Mutex<()>>,
        receipts: Mutex<std::collections::BTreeMap<String, ActionReceipt>>,
        terminal_after_send: std::sync::atomic::AtomicBool,
        delivery: SendDelivery,
        changed_path_after_send: std::sync::atomic::AtomicBool,
        revision: std::sync::atomic::AtomicU64,
        bound: AtomicBool,
        refresh_calls: std::sync::atomic::AtomicUsize,
        activate_calls: std::sync::atomic::AtomicUsize,
        activate_failures: std::sync::atomic::AtomicUsize,
        identity: Option<String>,
        blocked_session: AtomicBool,
        read_calls: std::sync::atomic::AtomicUsize,
        read_failures: std::sync::atomic::AtomicUsize,
        refresh_failures: std::sync::atomic::AtomicUsize,
        send_failure: Mutex<Option<(bool, &'static str)>>,
    }
    impl QueueHostPort for Fake {
        fn session_lock(&self, _: &TargetKey) -> Arc<tokio::sync::Mutex<()>> {
            self.lock.clone()
        }
        fn read<'a>(
            &'a self,
            key: &'a TargetKey,
            _: Duration,
        ) -> BoxFuture<'a, ZResult<BoundQueueSession>> {
            self.read_calls.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                if self
                    .read_failures
                    .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
                    .is_ok()
                {
                    return Err(ZcodeError::of(codes::NOT_READY));
                }
                if self.blocked_session.load(Ordering::SeqCst) && key.session_id == "blocked" {
                    return Err(ZcodeError::of(codes::NOT_READY));
                }
                if !self.bound.load(Ordering::SeqCst) {
                    return Err(ZcodeError::of(codes::NOT_READY));
                }
                let mut capabilities = CapabilityView::default();
                capabilities.send.enabled = true;
                let works = if *self.busy.lock().unwrap() {
                    vec![ActiveWork {
                        source_command_id: None,
                        kind: "primaryTurn".into(),
                        foreground_execution_id: Some("A".into()),
                    }]
                } else {
                    vec![]
                };
                let sent = !self.sent.lock().unwrap().is_empty();
                let terminal = self.terminal_after_send.load(Ordering::SeqCst) && sent;
                let path = if self.changed_path_after_send.load(Ordering::SeqCst) && sent {
                    "/other"
                } else {
                    "/synthetic"
                };
                Ok(BoundQueueSession {
                    target: BoundTarget {
                        session_id: key.session_id.clone(),
                        workspace_path: path.into(),
                        workspace_identity: self.identity.clone(),
                        workspace_key: self.identity.clone().unwrap_or_else(|| path.into()),
                        generation: 1,
                        bridge_generation: None,
                    },
                    view: SessionView {
                        session_id: key.session_id.clone(),
                        generation: 1,
                        revision: Some(if terminal {
                            2
                        } else {
                            self.revision.load(Ordering::SeqCst)
                        }),
                        ready: true,
                        pending: vec![],
                        control: Some(ControlView {
                            active_works: works,
                        }),
                        selection: None,
                        capabilities,
                        queue: QueueView {
                            known: true,
                            paused: Some(false),
                            ..Default::default()
                        },
                        execution_terminal: if terminal {
                            Some(ExecutionTerminal {
                                input_id: "B-input".into(),
                                result_type: "success".into(),
                                revision: 2,
                            })
                        } else {
                            None
                        },
                        input_routing: None,
                    },
                })
            })
        }
        fn refresh_target<'a>(
            &'a self,
            key: &'a TargetKey,
            timeout: Duration,
        ) -> BoxFuture<'a, ZResult<BoundTarget>> {
            self.refresh_calls.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                if self
                    .refresh_failures
                    .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
                    .is_ok()
                {
                    return Err(ZcodeError::of(codes::NOT_READY));
                }
                Ok(self.read(key, timeout).await?.target)
            })
        }
        fn activate<'a>(
            &'a self,
            key: &'a TargetKey,
            _timeout: Duration,
        ) -> BoxFuture<'a, ZResult<()>> {
            self.activate_calls.fetch_add(1, Ordering::SeqCst);
            if self.blocked_session.load(Ordering::SeqCst) && key.session_id == "blocked" {
                return Box::pin(async { Err(ZcodeError::of(codes::NOT_READY)) });
            }
            let fail = self
                .activate_failures
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
                .is_ok();
            if !fail {
                self.bound.store(true, Ordering::SeqCst);
            }
            Box::pin(async move {
                if fail {
                    Err(ZcodeError::of(codes::NOT_READY))
                } else {
                    Ok(())
                }
            })
        }
        fn send_prepared<'a>(
            &'a self,
            ctx: &'a ActionContext,
            target: &'a BoundTarget,
            body: &'a PreparedSend,
            _: Option<&'a HeldQueueChoice>,
        ) -> BoxFuture<'a, super::super::super::command::PreparedResult<ActionReceipt>> {
            Box::pin(async move {
                if let Some((written, code)) = self.send_failure.lock().unwrap().take() {
                    if written {
                        self.sent.lock().unwrap().push(body.wire_text().into());
                    }
                    return Err(if written {
                        super::super::super::command::PreparedDispatchError::MayHaveWritten(
                            ZcodeError::of(code),
                        )
                    } else {
                        super::super::super::command::PreparedDispatchError::BeforeWrite(
                            ZcodeError::of(code),
                        )
                    });
                }
                self.sent.lock().unwrap().push(body.wire_text().into());
                Ok(ActionReceipt {
                    action_id: ctx.action_id.clone(),
                    session_id: target.session_id.clone(),
                    generation: target.generation,
                    state: ActionState::Accepted,
                    reason_code: None,
                    send_admission: Some(SendAdmission {
                        delivery: self.delivery,
                        input_id: if body.wire_text() == "A2" {
                            "A2-input".into()
                        } else {
                            "B-input".into()
                        },
                    }),
                })
            })
        }
        fn stop<'a>(
            &'a self,
            _: &'a ActionContext,
            _: &'a BoundTarget,
            _: &'a str,
        ) -> BoxFuture<'a, ZResult<ActionReceipt>> {
            Box::pin(async { unreachable!() })
        }
        fn receipt<'a>(
            &'a self,
            action: &'a str,
            _: &'a str,
        ) -> BoxFuture<'a, ZResult<Option<ActionReceipt>>> {
            Box::pin(async move { Ok(self.receipts.lock().unwrap().get(action).cloned()) })
        }
    }
    struct Gate;
    impl QueueRuntimeGate for Gate {
        fn remote_enabled(&self) -> bool {
            true
        }
        fn device_valid(&self, _: &str) -> bool {
            true
        }
        fn shutting_down(&self) -> bool {
            false
        }
    }
    struct DisabledGate;
    impl QueueRuntimeGate for DisabledGate {
        fn remote_enabled(&self) -> bool {
            false
        }
        fn device_valid(&self, _: &str) -> bool {
            true
        }
        fn shutting_down(&self) -> bool {
            false
        }
    }
    struct RevokedGate;
    impl QueueRuntimeGate for RevokedGate {
        fn remote_enabled(&self) -> bool {
            true
        }
        fn device_valid(&self, _: &str) -> bool {
            false
        }
        fn shutting_down(&self) -> bool {
            false
        }
    }
    async fn denied_gate_sends_nothing(gate: Arc<dyn QueueRuntimeGate>) {
        let fake = Arc::new(Fake {
            sent: Mutex::new(vec![]),
            busy: Mutex::new(false),
            lock: Arc::new(tokio::sync::Mutex::new(())),
            receipts: Mutex::new(std::collections::BTreeMap::new()),
            terminal_after_send: AtomicBool::new(false),
            delivery: SendDelivery::StartNow,
            changed_path_after_send: AtomicBool::new(false),
            revision: std::sync::atomic::AtomicU64::new(1),
            bound: AtomicBool::new(true),
            refresh_calls: std::sync::atomic::AtomicUsize::new(0),
            activate_calls: std::sync::atomic::AtomicUsize::new(0),
            activate_failures: std::sync::atomic::AtomicUsize::new(0),
            identity: None,
            blocked_session: AtomicBool::new(false),
            read_calls: std::sync::atomic::AtomicUsize::new(0),
            read_failures: std::sync::atomic::AtomicUsize::new(0),
            refresh_failures: std::sync::atomic::AtomicUsize::new(0),
            send_failure: Mutex::new(None),
        });
        let svc = ZcodeQueueService::new(
            SqlLocalQueueStore::new(DeviceStore::memory()),
            fake.clone(),
            gate,
        );
        let key = TargetKey {
            session_id: "session".into(),
        };
        svc.enqueue(EnqueueCommand {
            request_id: "B".into(),
            actor: QueueActor {
                device_id: "d".into(),
                device_name: "phone".into(),
            },
            session_id: key.session_id.clone(),
            workspace_path: "/synthetic".into(),
            workspace_identity: None,
            draft: QueueDraft {
                text: "B".into(),
                attachments: vec![],
            },
            prepared: PreparedSend::from_stored("B".into()).unwrap(),
            edited_from: None,
        })
        .await
        .unwrap();
        svc.flush_one(&key).await.unwrap();
        assert!(fake.sent.lock().unwrap().is_empty());
    }
    struct ToggleGate(AtomicBool);
    impl QueueRuntimeGate for ToggleGate {
        fn remote_enabled(&self) -> bool {
            self.0.load(Ordering::SeqCst)
        }
        fn device_valid(&self, _: &str) -> bool {
            true
        }
        fn shutting_down(&self) -> bool {
            false
        }
    }
    #[tokio::test]
    async fn offline_first_enqueue_activates_on_connection_and_dispatches() {
        let fake = Arc::new(Fake {
            sent: Mutex::new(vec![]),
            busy: Mutex::new(false),
            lock: Arc::new(tokio::sync::Mutex::new(())),
            receipts: Mutex::new(std::collections::BTreeMap::new()),
            terminal_after_send: AtomicBool::new(false),
            delivery: SendDelivery::StartNow,
            changed_path_after_send: AtomicBool::new(false),
            revision: std::sync::atomic::AtomicU64::new(1),
            bound: AtomicBool::new(false),
            refresh_calls: std::sync::atomic::AtomicUsize::new(0),
            activate_calls: std::sync::atomic::AtomicUsize::new(0),
            activate_failures: std::sync::atomic::AtomicUsize::new(1),
            identity: None,
            blocked_session: AtomicBool::new(false),
            read_calls: std::sync::atomic::AtomicUsize::new(0),
            read_failures: std::sync::atomic::AtomicUsize::new(0),
            refresh_failures: std::sync::atomic::AtomicUsize::new(0),
            send_failure: Mutex::new(None),
        });
        let gate = Arc::new(ToggleGate(AtomicBool::new(false)));
        let svc = Arc::new(ZcodeQueueService::new(
            SqlLocalQueueStore::new(DeviceStore::memory()),
            fake.clone(),
            gate.clone(),
        ));
        svc.enqueue(EnqueueCommand {
            request_id: "B".into(),
            actor: QueueActor {
                device_id: "d".into(),
                device_name: "phone".into(),
            },
            session_id: "session".into(),
            workspace_path: "/synthetic".into(),
            workspace_identity: None,
            draft: QueueDraft {
                text: "B".into(),
                attachments: vec![],
            },
            prepared: PreparedSend::from_stored("B".into()).unwrap(),
            edited_from: None,
        })
        .await
        .unwrap();
        svc.start();
        assert!(fake.sent.lock().unwrap().is_empty());
        assert_eq!(fake.refresh_calls.load(Ordering::SeqCst), 0);
        gate.0.store(true, Ordering::SeqCst);
        svc.on_connected().await;
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if !fake.sent.lock().unwrap().is_empty() {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("paired activation must permit dispatch");
        assert_eq!(fake.activate_calls.load(Ordering::SeqCst), 2);
        assert_eq!(*fake.sent.lock().unwrap(), vec!["B"]);
        svc.shutdown();
    }
    #[tokio::test]
    async fn activation_retry_stops_on_remote_close_or_shutdown() {
        for shutdown in [false, true] {
            let fake = Arc::new(Fake {
                sent: Mutex::new(vec![]),
                busy: Mutex::new(false),
                lock: Arc::new(tokio::sync::Mutex::new(())),
                receipts: Mutex::new(std::collections::BTreeMap::new()),
                terminal_after_send: AtomicBool::new(false),
                delivery: SendDelivery::StartNow,
                changed_path_after_send: AtomicBool::new(false),
                revision: std::sync::atomic::AtomicU64::new(1),
                bound: AtomicBool::new(false),
                refresh_calls: std::sync::atomic::AtomicUsize::new(0),
                activate_calls: std::sync::atomic::AtomicUsize::new(0),
                activate_failures: std::sync::atomic::AtomicUsize::new(100),
                identity: None,
                blocked_session: AtomicBool::new(false),
                read_calls: std::sync::atomic::AtomicUsize::new(0),
                read_failures: std::sync::atomic::AtomicUsize::new(0),
                refresh_failures: std::sync::atomic::AtomicUsize::new(0),
                send_failure: Mutex::new(None),
            });
            let gate = Arc::new(ToggleGate(AtomicBool::new(true)));
            let svc = Arc::new(ZcodeQueueService::new(
                SqlLocalQueueStore::new(DeviceStore::memory()),
                fake.clone(),
                gate.clone(),
            ));
            svc.enqueue(EnqueueCommand {
                request_id: "B".into(),
                actor: QueueActor {
                    device_id: "d".into(),
                    device_name: "phone".into(),
                },
                session_id: "session".into(),
                workspace_path: "/synthetic".into(),
                workspace_identity: None,
                draft: QueueDraft {
                    text: "B".into(),
                    attachments: vec![],
                },
                prepared: PreparedSend::from_stored("B".into()).unwrap(),
                edited_from: None,
            })
            .await
            .unwrap();
            let worker = svc.clone();
            let task = tokio::spawn(async move { worker.on_connected().await });
            tokio::time::timeout(Duration::from_secs(1), async {
                while fake.activate_calls.load(Ordering::SeqCst) == 0 {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            tokio::task::yield_now().await;
            assert!(
                !task.is_finished(),
                "temporary subscription failure must be retried"
            );
            if shutdown {
                svc.shutdown()
            } else {
                gate.0.store(false, Ordering::SeqCst);
                svc.wake_all();
            }
            tokio::time::timeout(Duration::from_secs(1), task)
                .await
                .unwrap()
                .unwrap();
            assert!(fake.sent.lock().unwrap().is_empty());
        }
    }
    #[tokio::test]
    async fn remote_disabled_never_flushes() {
        denied_gate_sends_nothing(Arc::new(DisabledGate)).await;
    }
    #[tokio::test]
    async fn revoked_device_zero_dispatch() {
        denied_gate_sends_nothing(Arc::new(RevokedGate)).await;
    }
    #[tokio::test]
    async fn enqueue_waits_for_same_session_write_lock() {
        let fake = Arc::new(Fake {
            sent: Mutex::new(vec![]),
            busy: Mutex::new(false),
            lock: Arc::new(tokio::sync::Mutex::new(())),
            receipts: Mutex::new(std::collections::BTreeMap::new()),
            terminal_after_send: AtomicBool::new(false),
            delivery: SendDelivery::StartNow,
            changed_path_after_send: AtomicBool::new(false),
            revision: std::sync::atomic::AtomicU64::new(1),
            bound: AtomicBool::new(true),
            refresh_calls: std::sync::atomic::AtomicUsize::new(0),
            activate_calls: std::sync::atomic::AtomicUsize::new(0),
            activate_failures: std::sync::atomic::AtomicUsize::new(0),
            identity: None,
            blocked_session: AtomicBool::new(false),
            read_calls: std::sync::atomic::AtomicUsize::new(0),
            read_failures: std::sync::atomic::AtomicUsize::new(0),
            refresh_failures: std::sync::atomic::AtomicUsize::new(0),
            send_failure: Mutex::new(None),
        });
        let svc = Arc::new(ZcodeQueueService::new(
            SqlLocalQueueStore::new(DeviceStore::memory()),
            fake.clone(),
            Arc::new(Gate),
        ));
        let lock = fake.lock.clone();
        let guard = lock.lock().await;
        let entered = Arc::new(tokio::sync::Notify::new());
        let ready = entered.clone();
        let copy = svc.clone();
        let mut task = tokio::spawn(async move {
            ready.notify_one();
            copy.enqueue(EnqueueCommand {
                request_id: "B".into(),
                actor: QueueActor {
                    device_id: "d".into(),
                    device_name: "phone".into(),
                },
                session_id: "session".into(),
                workspace_path: "/synthetic".into(),
                workspace_identity: None,
                draft: QueueDraft {
                    text: "B".into(),
                    attachments: vec![],
                },
                prepared: PreparedSend::from_stored("B".into()).unwrap(),
                edited_from: None,
            })
            .await
        });
        entered.notified().await;
        assert!(
            tokio::time::timeout(Duration::from_millis(50), &mut task)
                .await
                .is_err(),
            "enqueue must serialize with an existing writer"
        );
        drop(guard);
        task.await.unwrap().unwrap();
    }
    #[tokio::test]
    async fn startup_and_new_snapshot_deadline_are_cleaned() {
        use super::super::types::ItemCommand;
        let fake = Arc::new(Fake {
            sent: Mutex::new(vec![]),
            busy: Mutex::new(true),
            lock: Arc::new(tokio::sync::Mutex::new(())),
            receipts: Mutex::new(std::collections::BTreeMap::new()),
            terminal_after_send: AtomicBool::new(false),
            delivery: SendDelivery::StartNow,
            changed_path_after_send: AtomicBool::new(false),
            revision: std::sync::atomic::AtomicU64::new(1),
            bound: AtomicBool::new(true),
            refresh_calls: std::sync::atomic::AtomicUsize::new(0),
            activate_calls: std::sync::atomic::AtomicUsize::new(0),
            activate_failures: std::sync::atomic::AtomicUsize::new(0),
            identity: None,
            blocked_session: AtomicBool::new(false),
            read_calls: std::sync::atomic::AtomicUsize::new(0),
            read_failures: std::sync::atomic::AtomicUsize::new(0),
            refresh_failures: std::sync::atomic::AtomicUsize::new(0),
            send_failure: Mutex::new(None),
        });
        let store = SqlLocalQueueStore::new(DeviceStore::memory());
        let actor = QueueActor {
            device_id: "d".into(),
            device_name: "phone".into(),
        };
        let now = chrono::Utc::now().timestamp_millis();
        let create = |id: &str, time: i64| {
            let receipt = store
                .enqueue(
                    &EnqueueCommand {
                        request_id: id.into(),
                        actor: actor.clone(),
                        session_id: "session".into(),
                        workspace_path: "/synthetic".into(),
                        workspace_identity: None,
                        draft: QueueDraft {
                            text: id.into(),
                            attachments: vec![],
                        },
                        prepared: PreparedSend::from_stored(id.into()).unwrap(),
                        edited_from: None,
                    },
                    time,
                )
                .unwrap();
            store
                .edit(
                    &actor,
                    &ItemCommand {
                        request_id: format!("edit-{id}"),
                        item_id: receipt.item_id,
                        expected_version: 1,
                    },
                    time,
                )
                .unwrap();
        };
        create("old", now - 86_400_001);
        let svc = Arc::new(ZcodeQueueService::new(store.clone(), fake, Arc::new(Gate)));
        svc.start_cleanup();
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if store.next_edit_expiry().unwrap().is_none() {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("startup must clear expired snapshot");
        create(
            "new",
            chrono::Utc::now().timestamp_millis() - 86_400_000 + 800,
        );
        assert!(store.next_edit_expiry().unwrap().is_some());
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if store.next_edit_expiry().unwrap().is_none() {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("new deadline must wake cleaner");
        svc.shutdown();
    }
    #[tokio::test]
    async fn preclaim_retry_stops_on_remote_close_and_shutdown() {
        for shutdown in [false, true] {
            let fake = Arc::new(Fake {
                sent: Mutex::new(vec![]),
                busy: Mutex::new(false),
                lock: Arc::new(tokio::sync::Mutex::new(())),
                receipts: Mutex::new(std::collections::BTreeMap::new()),
                terminal_after_send: AtomicBool::new(false),
                delivery: SendDelivery::StartNow,
                changed_path_after_send: AtomicBool::new(false),
                revision: std::sync::atomic::AtomicU64::new(1),
                bound: AtomicBool::new(true),
                refresh_calls: std::sync::atomic::AtomicUsize::new(0),
                activate_calls: std::sync::atomic::AtomicUsize::new(0),
                activate_failures: std::sync::atomic::AtomicUsize::new(0),
                identity: None,
                blocked_session: AtomicBool::new(false),
                read_calls: std::sync::atomic::AtomicUsize::new(0),
                read_failures: std::sync::atomic::AtomicUsize::new(100),
                refresh_failures: std::sync::atomic::AtomicUsize::new(0),
                send_failure: Mutex::new(None),
            });
            let gate = Arc::new(ToggleGate(AtomicBool::new(true)));
            let svc = Arc::new(ZcodeQueueService::new(
                SqlLocalQueueStore::new(DeviceStore::memory()),
                fake.clone(),
                gate.clone(),
            ));
            svc.enqueue(EnqueueCommand {
                request_id: "B".into(),
                actor: QueueActor {
                    device_id: "d".into(),
                    device_name: "phone".into(),
                },
                session_id: "s".into(),
                workspace_path: "/synthetic".into(),
                workspace_identity: None,
                draft: QueueDraft {
                    text: "B".into(),
                    attachments: vec![],
                },
                prepared: PreparedSend::from_stored("B".into()).unwrap(),
                edited_from: None,
            })
            .await
            .unwrap();
            svc.wake.notified().await;
            svc.start();
            tokio::time::timeout(Duration::from_secs(1), async {
                while fake.read_calls.load(Ordering::SeqCst) == 0 {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            if shutdown {
                svc.shutdown()
            } else {
                gate.0.store(false, Ordering::SeqCst);
                svc.wake_all();
            }
            let before = fake.read_calls.load(Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(250)).await;
            assert_eq!(fake.read_calls.load(Ordering::SeqCst), before);
            assert!(fake.sent.lock().unwrap().is_empty());
            svc.shutdown();
        }
    }
    struct ChangingGate {
        revoke: bool,
        remote_calls: std::sync::atomic::AtomicUsize,
        device_calls: std::sync::atomic::AtomicUsize,
    }
    impl QueueRuntimeGate for ChangingGate {
        fn remote_enabled(&self) -> bool {
            !(!self.revoke && self.remote_calls.fetch_add(1, Ordering::SeqCst) > 0)
        }
        fn device_valid(&self, _: &str) -> bool {
            !(self.revoke && self.device_calls.fetch_add(1, Ordering::SeqCst) > 0)
        }
        fn shutting_down(&self) -> bool {
            false
        }
    }
    #[tokio::test]
    async fn postclaim_gate_change_retains_zero_write_reason() {
        for revoke in [false, true] {
            let fake = Arc::new(Fake {
                sent: Mutex::new(vec![]),
                busy: Mutex::new(false),
                lock: Arc::new(tokio::sync::Mutex::new(())),
                receipts: Mutex::new(std::collections::BTreeMap::new()),
                terminal_after_send: AtomicBool::new(false),
                delivery: SendDelivery::StartNow,
                changed_path_after_send: AtomicBool::new(false),
                revision: std::sync::atomic::AtomicU64::new(1),
                bound: AtomicBool::new(true),
                refresh_calls: std::sync::atomic::AtomicUsize::new(0),
                activate_calls: std::sync::atomic::AtomicUsize::new(0),
                activate_failures: std::sync::atomic::AtomicUsize::new(0),
                identity: None,
                blocked_session: AtomicBool::new(false),
                read_calls: std::sync::atomic::AtomicUsize::new(0),
                read_failures: std::sync::atomic::AtomicUsize::new(0),
                refresh_failures: std::sync::atomic::AtomicUsize::new(0),
                send_failure: Mutex::new(None),
            });
            let gate = Arc::new(ChangingGate {
                revoke,
                remote_calls: std::sync::atomic::AtomicUsize::new(0),
                device_calls: std::sync::atomic::AtomicUsize::new(0),
            });
            let store = SqlLocalQueueStore::new(DeviceStore::memory());
            let svc = ZcodeQueueService::new(store.clone(), fake.clone(), gate);
            let key = TargetKey {
                session_id: "s".into(),
            };
            let actor = QueueActor {
                device_id: "d".into(),
                device_name: "phone".into(),
            };
            svc.enqueue(EnqueueCommand {
                request_id: "B".into(),
                actor: actor.clone(),
                session_id: key.session_id.clone(),
                workspace_path: "/synthetic".into(),
                workspace_identity: None,
                draft: QueueDraft {
                    text: "B".into(),
                    attachments: vec![],
                },
                prepared: PreparedSend::from_stored("B".into()).unwrap(),
                edited_from: None,
            })
            .await
            .unwrap();
            svc.flush_one(&key).await.unwrap();
            let view = store.view(&actor, &key).unwrap();
            assert_eq!(
                view.items[0].state,
                super::super::types::QueueItemState::Queued
            );
            assert!(!view.auto_drain);
            assert_eq!(
                view.pause_reason.as_deref(),
                Some(if revoke {
                    codes::NO_SESSION
                } else {
                    codes::NOT_READY
                })
            );
            assert!(fake.sent.lock().unwrap().is_empty());
        }
    }
    #[tokio::test]
    async fn prewrite_rejection_retains_reason_but_postwrite_error_is_unknown() {
        for (written, code) in [
            (false, codes::NOT_READY),
            (false, codes::NEEDS_EVIDENCE),
            (false, codes::NO_SESSION),
            (false, codes::STALE_GENERATION),
            (true, codes::STORAGE_FAILED),
        ] {
            let fake = Arc::new(Fake {
                sent: Mutex::new(vec![]),
                busy: Mutex::new(false),
                lock: Arc::new(tokio::sync::Mutex::new(())),
                receipts: Mutex::new(std::collections::BTreeMap::new()),
                terminal_after_send: AtomicBool::new(false),
                delivery: SendDelivery::StartNow,
                changed_path_after_send: AtomicBool::new(false),
                revision: std::sync::atomic::AtomicU64::new(1),
                bound: AtomicBool::new(true),
                refresh_calls: std::sync::atomic::AtomicUsize::new(0),
                activate_calls: std::sync::atomic::AtomicUsize::new(0),
                activate_failures: std::sync::atomic::AtomicUsize::new(0),
                identity: None,
                blocked_session: AtomicBool::new(false),
                read_calls: std::sync::atomic::AtomicUsize::new(0),
                read_failures: std::sync::atomic::AtomicUsize::new(0),
                refresh_failures: std::sync::atomic::AtomicUsize::new(0),
                send_failure: Mutex::new(Some((written, code))),
            });
            let store = SqlLocalQueueStore::new(DeviceStore::memory());
            let svc = ZcodeQueueService::new(store.clone(), fake.clone(), Arc::new(Gate));
            let key = TargetKey {
                session_id: "s".into(),
            };
            let actor = QueueActor {
                device_id: "d".into(),
                device_name: "phone".into(),
            };
            svc.enqueue(EnqueueCommand {
                request_id: "B".into(),
                actor: actor.clone(),
                session_id: key.session_id.clone(),
                workspace_path: "/synthetic".into(),
                workspace_identity: None,
                draft: QueueDraft {
                    text: "B".into(),
                    attachments: vec![],
                },
                prepared: PreparedSend::from_stored("B".into()).unwrap(),
                edited_from: None,
            })
            .await
            .unwrap();
            svc.flush_one(&key).await.unwrap();
            let view = store.view(&actor, &key).unwrap();
            assert_eq!(
                view.items[0].state,
                if written {
                    super::super::types::QueueItemState::Unknown
                } else {
                    super::super::types::QueueItemState::Queued
                }
            );
            assert!(!view.auto_drain);
            assert_eq!(
                view.pause_reason.as_deref(),
                Some(if written { "unknown" } else { code })
            );
            svc.flush_one(&key).await.unwrap();
            assert_eq!(fake.sent.lock().unwrap().len(), usize::from(written));
        }
    }
    #[tokio::test]
    async fn transient_preclaim_read_or_refresh_retries_without_external_events() {
        for fail_refresh in [false, true] {
            let fake = Arc::new(Fake {
                sent: Mutex::new(vec![]),
                busy: Mutex::new(false),
                lock: Arc::new(tokio::sync::Mutex::new(())),
                receipts: Mutex::new(std::collections::BTreeMap::new()),
                terminal_after_send: AtomicBool::new(false),
                delivery: SendDelivery::StartNow,
                changed_path_after_send: AtomicBool::new(false),
                revision: std::sync::atomic::AtomicU64::new(1),
                bound: AtomicBool::new(true),
                refresh_calls: std::sync::atomic::AtomicUsize::new(0),
                activate_calls: std::sync::atomic::AtomicUsize::new(0),
                activate_failures: std::sync::atomic::AtomicUsize::new(0),
                identity: None,
                blocked_session: AtomicBool::new(false),
                read_calls: std::sync::atomic::AtomicUsize::new(0),
                read_failures: std::sync::atomic::AtomicUsize::new(usize::from(!fail_refresh)),
                refresh_failures: std::sync::atomic::AtomicUsize::new(usize::from(fail_refresh)),
                send_failure: Mutex::new(None),
            });
            let svc = Arc::new(ZcodeQueueService::new(
                SqlLocalQueueStore::new(DeviceStore::memory()),
                fake.clone(),
                Arc::new(Gate),
            ));
            svc.enqueue(EnqueueCommand {
                request_id: "B".into(),
                actor: QueueActor {
                    device_id: "d".into(),
                    device_name: "phone".into(),
                },
                session_id: "s".into(),
                workspace_path: "/synthetic".into(),
                workspace_identity: None,
                draft: QueueDraft {
                    text: "B".into(),
                    attachments: vec![],
                },
                prepared: PreparedSend::from_stored("B".into()).unwrap(),
                edited_from: None,
            })
            .await
            .unwrap();
            svc.wake.notified().await;
            svc.start();
            tokio::time::timeout(Duration::from_secs(2), async {
                loop {
                    if !fake.sent.lock().unwrap().is_empty() {
                        break;
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("temporary preclaim error must retry without an event");
            assert_eq!(*fake.sent.lock().unwrap(), vec!["B"]);
            svc.shutdown();
        }
    }
    #[tokio::test]
    async fn completion_revalidates_success_observed_before_invalidate_is_applied() {
        struct SplitBridge {
            reserved: std::sync::atomic::AtomicU64,
            applied: std::sync::atomic::AtomicU64,
            a_bound: std::sync::atomic::AtomicU64,
            a_activations: std::sync::atomic::AtomicUsize,
            b_attempts: std::sync::atomic::AtomicUsize,
            first_entered: AtomicBool,
            first_release: Notify,
            b_held: AtomicBool,
            b_release: Notify,
            active: std::sync::atomic::AtomicUsize,
            max_active: std::sync::atomic::AtomicUsize,
            sent: Mutex<Vec<String>>,
            lock: Arc<tokio::sync::Mutex<()>>,
        }
        impl QueueHostPort for SplitBridge {
            fn activation_epoch(&self) -> Option<u64> {
                Some(self.reserved.load(Ordering::SeqCst))
            }
            fn session_lock(&self, _: &TargetKey) -> Arc<tokio::sync::Mutex<()>> {
                self.lock.clone()
            }
            fn read<'a>(
                &'a self,
                key: &'a TargetKey,
                _: Duration,
            ) -> BoxFuture<'a, ZResult<BoundQueueSession>> {
                Box::pin(async move {
                    let applied = self.applied.load(Ordering::SeqCst);
                    if key.session_id == "b" || self.a_bound.load(Ordering::SeqCst) != applied {
                        return Err(ZcodeError::of(codes::NOT_READY));
                    }
                    let mut capabilities = CapabilityView::default();
                    capabilities.send.enabled = true;
                    let active_works = if applied == 1 {
                        vec![ActiveWork {
                            kind: "primaryTurn".into(),
                            foreground_execution_id: Some("old".into()),
                            source_command_id: None,
                        }]
                    } else {
                        vec![]
                    };
                    Ok(BoundQueueSession {
                        target: BoundTarget {
                            session_id: key.session_id.clone(),
                            workspace_path: "/synthetic".into(),
                            workspace_identity: None,
                            workspace_key: "/synthetic".into(),
                            generation: 1,
                            bridge_generation: Some(applied),
                        },
                        view: SessionView {
                            session_id: key.session_id.clone(),
                            generation: 1,
                            revision: Some(1),
                            ready: true,
                            pending: vec![],
                            control: Some(ControlView { active_works }),
                            selection: None,
                            capabilities,
                            queue: QueueView {
                                known: true,
                                paused: Some(false),
                                ..Default::default()
                            },
                            execution_terminal: None,
                            input_routing: None,
                        },
                    })
                })
            }
            fn refresh_target<'a>(
                &'a self,
                key: &'a TargetKey,
                timeout: Duration,
            ) -> BoxFuture<'a, ZResult<BoundTarget>> {
                Box::pin(async move { Ok(self.read(key, timeout).await?.target) })
            }
            fn activate<'a>(
                &'a self,
                key: &'a TargetKey,
                _: Duration,
            ) -> BoxFuture<'a, ZResult<()>> {
                Box::pin(async move {
                    let n = self.active.fetch_add(1, Ordering::SeqCst) + 1;
                    self.max_active.fetch_max(n, Ordering::SeqCst);
                    if key.session_id == "a" {
                        self.a_activations.fetch_add(1, Ordering::SeqCst);
                        self.a_bound
                            .store(self.applied.load(Ordering::SeqCst), Ordering::SeqCst);
                    } else {
                        let attempt = self.b_attempts.fetch_add(1, Ordering::SeqCst);
                        if attempt == 0 {
                            self.first_entered.store(true, Ordering::SeqCst);
                            self.first_release.notified().await;
                            self.active.fetch_sub(1, Ordering::SeqCst);
                            return Err(ZcodeError::of(codes::NOT_READY));
                        }
                        if !self.b_held.swap(true, Ordering::SeqCst) {
                            self.b_release.notified().await;
                        }
                    }
                    self.active.fetch_sub(1, Ordering::SeqCst);
                    Ok(())
                })
            }
            fn send_prepared<'a>(
                &'a self,
                ctx: &'a ActionContext,
                target: &'a BoundTarget,
                body: &'a PreparedSend,
                _: Option<&'a HeldQueueChoice>,
            ) -> BoxFuture<'a, super::super::super::command::PreparedResult<ActionReceipt>>
            {
                Box::pin(async move {
                    self.sent.lock().unwrap().push(body.wire_text().into());
                    Ok(ActionReceipt {
                        action_id: ctx.action_id.clone(),
                        session_id: target.session_id.clone(),
                        generation: target.generation,
                        state: ActionState::Accepted,
                        reason_code: None,
                        send_admission: Some(SendAdmission {
                            delivery: SendDelivery::StartNow,
                            input_id: "input".into(),
                        }),
                    })
                })
            }
            fn stop<'a>(
                &'a self,
                _: &'a ActionContext,
                _: &'a BoundTarget,
                _: &'a str,
            ) -> BoxFuture<'a, ZResult<ActionReceipt>> {
                Box::pin(async { unreachable!() })
            }
            fn receipt<'a>(
                &'a self,
                _: &'a str,
                _: &'a str,
            ) -> BoxFuture<'a, ZResult<Option<ActionReceipt>>> {
                Box::pin(async { Ok(None) })
            }
        }
        let fake = Arc::new(SplitBridge {
            reserved: std::sync::atomic::AtomicU64::new(1),
            applied: std::sync::atomic::AtomicU64::new(1),
            a_bound: std::sync::atomic::AtomicU64::new(0),
            a_activations: std::sync::atomic::AtomicUsize::new(0),
            b_attempts: std::sync::atomic::AtomicUsize::new(0),
            first_entered: AtomicBool::new(false),
            first_release: Notify::new(),
            b_held: AtomicBool::new(false),
            b_release: Notify::new(),
            active: std::sync::atomic::AtomicUsize::new(0),
            max_active: std::sync::atomic::AtomicUsize::new(0),
            sent: Mutex::new(vec![]),
            lock: Arc::new(tokio::sync::Mutex::new(())),
        });
        let svc = Arc::new(ZcodeQueueService::new(
            SqlLocalQueueStore::new(DeviceStore::memory()),
            fake.clone(),
            Arc::new(Gate),
        ));
        for session in ["a", "b"] {
            svc.enqueue(EnqueueCommand {
                request_id: session.into(),
                actor: QueueActor {
                    device_id: "d".into(),
                    device_name: "phone".into(),
                },
                session_id: session.into(),
                workspace_path: "/synthetic".into(),
                workspace_identity: None,
                draft: QueueDraft {
                    text: session.into(),
                    attachments: vec![],
                },
                prepared: PreparedSend::from_stored(session.into()).unwrap(),
                edited_from: None,
            })
            .await
            .unwrap();
        }
        svc.start();
        let (_tx, rx) = tokio::sync::broadcast::channel(64);
        let listener = tokio::spawn(crate::remote::run_queue_listener(svc.clone(), rx, true));
        tokio::time::timeout(Duration::from_secs(2), async {
            while !fake.first_entered.load(Ordering::SeqCst) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(fake.a_activations.load(Ordering::SeqCst), 1);
        fake.reserved.store(2, Ordering::SeqCst);
        fake.first_release.notify_one();
        tokio::time::timeout(Duration::from_secs(2), async {
            while !fake.b_held.load(Ordering::SeqCst) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(fake.a_activations.load(Ordering::SeqCst), 2);
        assert_eq!(fake.a_bound.load(Ordering::SeqCst), 1);
        fake.applied.store(2, Ordering::SeqCst);
        fake.a_bound.store(0, Ordering::SeqCst);
        svc.request_connection_ready();
        tokio::time::timeout(Duration::from_secs(2), async {
            while svc.activation_invocations.load(Ordering::SeqCst) < 2 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(svc.activation_lock.try_lock().is_err());
        fake.b_release.notify_one();
        tokio::time::timeout(Duration::from_secs(2), async {
            while fake.sent.lock().unwrap().is_empty() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("completion must revalidate A after invalidate without another paired event");
        assert_eq!(*fake.sent.lock().unwrap(), vec!["a"]);
        assert_eq!(fake.a_activations.load(Ordering::SeqCst), 3);
        assert_eq!(fake.max_active.load(Ordering::SeqCst), 1);
        svc.shutdown();
        tokio::time::timeout(Duration::from_secs(2), listener)
            .await
            .unwrap()
            .unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            while fake.active.load(Ordering::SeqCst) != 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let a_count = fake.a_activations.load(Ordering::SeqCst);
        let b_count = fake.b_attempts.load(Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(fake.a_activations.load(Ordering::SeqCst), a_count);
        assert_eq!(fake.b_attempts.load(Ordering::SeqCst), b_count);
    }
    #[tokio::test]
    async fn paired_during_activation_rebinds_old_success_in_new_bridge_epoch() {
        struct RaceHost {
            epoch: std::sync::atomic::AtomicU64,
            a_bound: std::sync::atomic::AtomicU64,
            a_activations: std::sync::atomic::AtomicUsize,
            b_entered: AtomicBool,
            b_attempts: std::sync::atomic::AtomicUsize,
            release: Notify,
            active: std::sync::atomic::AtomicUsize,
            max_active: std::sync::atomic::AtomicUsize,
            sent: Mutex<Vec<String>>,
            lock: Arc<tokio::sync::Mutex<()>>,
        }
        impl QueueHostPort for RaceHost {
            fn activation_epoch(&self) -> Option<u64> {
                Some(self.epoch.load(Ordering::SeqCst))
            }
            fn session_lock(&self, _: &TargetKey) -> Arc<tokio::sync::Mutex<()>> {
                self.lock.clone()
            }
            fn read<'a>(
                &'a self,
                key: &'a TargetKey,
                _: Duration,
            ) -> BoxFuture<'a, ZResult<BoundQueueSession>> {
                Box::pin(async move {
                    let epoch = self.epoch.load(Ordering::SeqCst);
                    if key.session_id == "b" || self.a_bound.load(Ordering::SeqCst) != epoch {
                        return Err(ZcodeError::of(codes::NOT_READY));
                    }
                    let mut capabilities = CapabilityView::default();
                    capabilities.send.enabled = true;
                    let active_works = if epoch == 1 {
                        vec![ActiveWork {
                            kind: "primaryTurn".into(),
                            foreground_execution_id: Some("old".into()),
                            source_command_id: None,
                        }]
                    } else {
                        vec![]
                    };
                    Ok(BoundQueueSession {
                        target: BoundTarget {
                            session_id: key.session_id.clone(),
                            workspace_path: "/synthetic".into(),
                            workspace_identity: None,
                            workspace_key: "/synthetic".into(),
                            generation: epoch,
                            bridge_generation: Some(epoch),
                        },
                        view: SessionView {
                            session_id: key.session_id.clone(),
                            generation: epoch,
                            revision: Some(1),
                            ready: true,
                            pending: vec![],
                            control: Some(ControlView { active_works }),
                            selection: None,
                            capabilities,
                            queue: QueueView {
                                known: true,
                                paused: Some(false),
                                ..Default::default()
                            },
                            execution_terminal: None,
                            input_routing: None,
                        },
                    })
                })
            }
            fn refresh_target<'a>(
                &'a self,
                key: &'a TargetKey,
                timeout: Duration,
            ) -> BoxFuture<'a, ZResult<BoundTarget>> {
                Box::pin(async move { Ok(self.read(key, timeout).await?.target) })
            }
            fn activate<'a>(
                &'a self,
                key: &'a TargetKey,
                _: Duration,
            ) -> BoxFuture<'a, ZResult<()>> {
                Box::pin(async move {
                    let n = self.active.fetch_add(1, Ordering::SeqCst) + 1;
                    self.max_active.fetch_max(n, Ordering::SeqCst);
                    if key.session_id == "a" {
                        self.a_activations.fetch_add(1, Ordering::SeqCst);
                        self.a_bound
                            .store(self.epoch.load(Ordering::SeqCst), Ordering::SeqCst);
                    } else {
                        if self.b_attempts.fetch_add(1, Ordering::SeqCst) == 0 {
                            self.active.fetch_sub(1, Ordering::SeqCst);
                            return Err(ZcodeError::of(codes::NOT_READY));
                        }
                        if !self.b_entered.swap(true, Ordering::SeqCst) {
                            self.release.notified().await;
                        }
                    }
                    self.active.fetch_sub(1, Ordering::SeqCst);
                    Ok(())
                })
            }
            fn send_prepared<'a>(
                &'a self,
                ctx: &'a ActionContext,
                target: &'a BoundTarget,
                body: &'a PreparedSend,
                _: Option<&'a HeldQueueChoice>,
            ) -> BoxFuture<'a, super::super::super::command::PreparedResult<ActionReceipt>>
            {
                Box::pin(async move {
                    self.sent.lock().unwrap().push(body.wire_text().into());
                    Ok(ActionReceipt {
                        action_id: ctx.action_id.clone(),
                        session_id: target.session_id.clone(),
                        generation: target.generation,
                        state: ActionState::Accepted,
                        reason_code: None,
                        send_admission: Some(SendAdmission {
                            delivery: SendDelivery::StartNow,
                            input_id: "input".into(),
                        }),
                    })
                })
            }
            fn stop<'a>(
                &'a self,
                _: &'a ActionContext,
                _: &'a BoundTarget,
                _: &'a str,
            ) -> BoxFuture<'a, ZResult<ActionReceipt>> {
                Box::pin(async { unreachable!() })
            }
            fn receipt<'a>(
                &'a self,
                _: &'a str,
                _: &'a str,
            ) -> BoxFuture<'a, ZResult<Option<ActionReceipt>>> {
                Box::pin(async { Ok(None) })
            }
        }
        let fake = Arc::new(RaceHost {
            epoch: std::sync::atomic::AtomicU64::new(1),
            a_bound: std::sync::atomic::AtomicU64::new(0),
            a_activations: std::sync::atomic::AtomicUsize::new(0),
            b_entered: AtomicBool::new(false),
            b_attempts: std::sync::atomic::AtomicUsize::new(0),
            release: Notify::new(),
            active: std::sync::atomic::AtomicUsize::new(0),
            max_active: std::sync::atomic::AtomicUsize::new(0),
            sent: Mutex::new(vec![]),
            lock: Arc::new(tokio::sync::Mutex::new(())),
        });
        let svc = Arc::new(ZcodeQueueService::new(
            SqlLocalQueueStore::new(DeviceStore::memory()),
            fake.clone(),
            Arc::new(Gate),
        ));
        for session in ["a", "b"] {
            svc.enqueue(EnqueueCommand {
                request_id: session.into(),
                actor: QueueActor {
                    device_id: "d".into(),
                    device_name: "phone".into(),
                },
                session_id: session.into(),
                workspace_path: "/synthetic".into(),
                workspace_identity: None,
                draft: QueueDraft {
                    text: session.into(),
                    attachments: vec![],
                },
                prepared: PreparedSend::from_stored(session.into()).unwrap(),
                edited_from: None,
            })
            .await
            .unwrap();
        }
        svc.start();
        let (tx, rx) = tokio::sync::broadcast::channel(64);
        let listener = tokio::spawn(crate::remote::run_queue_listener(svc.clone(), rx, true));
        tokio::time::timeout(Duration::from_secs(2), async {
            while !fake.b_entered.load(Ordering::SeqCst) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(fake.a_activations.load(Ordering::SeqCst), 1);
        assert!(fake.sent.lock().unwrap().is_empty());
        fake.epoch.store(2, Ordering::SeqCst);
        fake.a_bound.store(0, Ordering::SeqCst);
        tx.send(ControlEvent {
            seq: 1,
            generation: 1,
            session_id: None,
            kind: ControlEventKind::Connection,
            data: serde_json::json!({"state":"paired"}),
            device_id: None,
        })
        .unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            while svc.activation_invocations.load(Ordering::SeqCst) < 2 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(
            svc.activation_lock.try_lock().is_err(),
            "old activation still owns the single-flight lock"
        );
        fake.release.notify_one();
        tokio::time::timeout(Duration::from_secs(2), async {
            while fake.sent.lock().unwrap().is_empty() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("A must reactivate after reconnect without another event");
        assert_eq!(*fake.sent.lock().unwrap(), vec!["a"]);
        assert_eq!(fake.a_activations.load(Ordering::SeqCst), 2);
        assert_eq!(fake.max_active.load(Ordering::SeqCst), 1);
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if svc.activation_lock.try_lock().is_ok() {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        fake.b_entered.store(false, Ordering::SeqCst);
        let owner_svc = svc.clone();
        let owner = tokio::spawn(async move { owner_svc.on_connected().await });
        tokio::time::timeout(Duration::from_secs(2), async {
            while !fake.b_entered.load(Ordering::SeqCst) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let mut callers = Vec::new();
        for _ in 0..8 {
            let service = svc.clone();
            callers.push(tokio::spawn(async move { service.on_connected().await }));
        }
        for caller in callers {
            tokio::time::timeout(Duration::from_millis(250), caller)
                .await
                .expect("coalesced callers must not pile up behind the active task")
                .unwrap();
        }
        fake.release.notify_one();
        tokio::time::timeout(Duration::from_secs(2), owner)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(fake.max_active.load(Ordering::SeqCst), 1);
        svc.shutdown();
        tokio::time::timeout(Duration::from_secs(2), listener)
            .await
            .unwrap()
            .unwrap();
        let count = fake.a_activations.load(Ordering::SeqCst);
        let b_count = fake.b_attempts.load(Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(fake.a_activations.load(Ordering::SeqCst), count);
        assert_eq!(fake.b_attempts.load(Ordering::SeqCst), b_count);
        assert_eq!(fake.active.load(Ordering::SeqCst), 0);
    }
    #[tokio::test]
    async fn listener_keeps_other_session_terminal_during_activation_retry() {
        let fake = Arc::new(Fake {
            sent: Mutex::new(vec![]),
            busy: Mutex::new(false),
            lock: Arc::new(tokio::sync::Mutex::new(())),
            receipts: Mutex::new(std::collections::BTreeMap::new()),
            terminal_after_send: AtomicBool::new(false),
            delivery: SendDelivery::StartNow,
            changed_path_after_send: AtomicBool::new(false),
            revision: std::sync::atomic::AtomicU64::new(100),
            bound: AtomicBool::new(true),
            refresh_calls: std::sync::atomic::AtomicUsize::new(0),
            activate_calls: std::sync::atomic::AtomicUsize::new(0),
            activate_failures: std::sync::atomic::AtomicUsize::new(0),
            identity: None,
            blocked_session: AtomicBool::new(true),
            read_calls: std::sync::atomic::AtomicUsize::new(0),
            read_failures: std::sync::atomic::AtomicUsize::new(0),
            refresh_failures: std::sync::atomic::AtomicUsize::new(0),
            send_failure: Mutex::new(None),
        });
        let svc = Arc::new(ZcodeQueueService::new(
            SqlLocalQueueStore::new(DeviceStore::memory()),
            fake.clone(),
            Arc::new(Gate),
        ));
        for (session, text) in [("a", "A1"), ("a", "A2"), ("a", "A3"), ("blocked", "B1")] {
            svc.enqueue(EnqueueCommand {
                request_id: text.into(),
                actor: QueueActor {
                    device_id: "d".into(),
                    device_name: "phone".into(),
                },
                session_id: session.into(),
                workspace_path: "/synthetic".into(),
                workspace_identity: None,
                draft: QueueDraft {
                    text: text.into(),
                    attachments: vec![],
                },
                prepared: PreparedSend::from_stored(text.into()).unwrap(),
                edited_from: None,
            })
            .await
            .unwrap();
        }
        svc.start();
        let (tx, rx) = tokio::sync::broadcast::channel(64);
        let listener = tokio::spawn(crate::remote::run_queue_listener(svc.clone(), rx, true));
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if fake.activate_calls.load(Ordering::SeqCst) >= 2
                    && fake.sent.lock().unwrap().contains(&"A1".to_string())
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let key = TargetKey {
            session_id: "a".into(),
        };
        let mut view = fake.read(&key, Duration::ZERO).await.unwrap().view;
        view.revision = Some(2);
        view.execution_terminal = Some(ExecutionTerminal {
            input_id: "B-input".into(),
            result_type: "success".into(),
            revision: 2,
        });
        let event = |seq, data| ControlEvent {
            seq,
            generation: 1,
            session_id: Some("a".into()),
            kind: ControlEventKind::Session,
            data,
            device_id: None,
        };
        tx.send(event(1, serde_json::to_value(view).unwrap()))
            .unwrap();
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if svc.observations.lock().unwrap().contains_key("a")
                    || fake.sent.lock().unwrap().contains(&"A2".to_string())
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("terminal event must be consumed while B retries");
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if fake.sent.lock().unwrap().contains(&"A2".to_string()) {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("A2 must follow first terminal");
        for seq in 2..=72 {
            let view = fake.read(&key, Duration::ZERO).await.unwrap().view;
            tx.send(event(seq, serde_json::to_value(view).unwrap()))
                .unwrap();
            tokio::task::yield_now().await;
        }
        fake.revision.store(101, Ordering::SeqCst);
        let mut final_view = fake.read(&key, Duration::ZERO).await.unwrap().view;
        final_view.execution_terminal = Some(ExecutionTerminal {
            input_id: "A2-input".into(),
            result_type: "success".into(),
            revision: 101,
        });
        tx.send(event(73, serde_json::to_value(final_view).unwrap()))
            .unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if fake.sent.lock().unwrap().contains(&"A3".to_string()) {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("A must continue after more than 64 frames");
        svc.shutdown();
        tokio::time::timeout(Duration::from_secs(2), listener)
            .await
            .unwrap()
            .unwrap();
    }
    #[tokio::test]
    async fn recovery_readiness_is_false_until_success() {
        let fake = Arc::new(Fake {
            sent: Mutex::new(vec![]),
            busy: Mutex::new(false),
            lock: Arc::new(tokio::sync::Mutex::new(())),
            receipts: Mutex::new(std::collections::BTreeMap::new()),
            terminal_after_send: AtomicBool::new(false),
            delivery: SendDelivery::StartNow,
            changed_path_after_send: AtomicBool::new(false),
            revision: std::sync::atomic::AtomicU64::new(1),
            bound: AtomicBool::new(true),
            refresh_calls: std::sync::atomic::AtomicUsize::new(0),
            activate_calls: std::sync::atomic::AtomicUsize::new(0),
            activate_failures: std::sync::atomic::AtomicUsize::new(0),
            identity: None,
            blocked_session: AtomicBool::new(false),
            read_calls: std::sync::atomic::AtomicUsize::new(0),
            read_failures: std::sync::atomic::AtomicUsize::new(0),
            refresh_failures: std::sync::atomic::AtomicUsize::new(0),
            send_failure: Mutex::new(None),
        });
        let svc = ZcodeQueueService::new(
            SqlLocalQueueStore::new(DeviceStore::memory()),
            fake,
            Arc::new(Gate),
        );
        assert!(!svc.recovery_ready().await);
        svc.recover().await.unwrap();
        assert!(svc.recovery_ready().await);
    }
    #[tokio::test]
    async fn recovery_is_once_and_never_marks_live_dispatch_unknown() {
        let fake = Arc::new(Fake {
            sent: Mutex::new(vec![]),
            busy: Mutex::new(false),
            lock: Arc::new(tokio::sync::Mutex::new(())),
            receipts: Mutex::new(std::collections::BTreeMap::new()),
            terminal_after_send: AtomicBool::new(false),
            delivery: SendDelivery::StartNow,
            changed_path_after_send: AtomicBool::new(false),
            revision: std::sync::atomic::AtomicU64::new(1),
            bound: AtomicBool::new(true),
            refresh_calls: std::sync::atomic::AtomicUsize::new(0),
            activate_calls: std::sync::atomic::AtomicUsize::new(0),
            activate_failures: std::sync::atomic::AtomicUsize::new(0),
            identity: None,
            blocked_session: AtomicBool::new(false),
            read_calls: std::sync::atomic::AtomicUsize::new(0),
            read_failures: std::sync::atomic::AtomicUsize::new(0),
            refresh_failures: std::sync::atomic::AtomicUsize::new(0),
            send_failure: Mutex::new(None),
        });
        let store = SqlLocalQueueStore::new(DeviceStore::memory());
        let svc = ZcodeQueueService::new(store.clone(), fake.clone(), Arc::new(Gate));
        svc.recover().await.unwrap();
        let actor = QueueActor {
            device_id: "d".into(),
            device_name: "phone".into(),
        };
        let key = TargetKey {
            session_id: "session".into(),
        };
        store
            .enqueue(
                &EnqueueCommand {
                    request_id: "B".into(),
                    actor: actor.clone(),
                    session_id: key.session_id.clone(),
                    workspace_path: "/synthetic".into(),
                    workspace_identity: None,
                    draft: QueueDraft {
                        text: "B".into(),
                        attachments: vec![],
                    },
                    prepared: PreparedSend::from_stored("B".into()).unwrap(),
                    edited_from: None,
                },
                1,
            )
            .unwrap();
        let target = fake.read(&key, Duration::ZERO).await.unwrap().target;
        store.claim_next(&key, &target, 2).unwrap().unwrap();
        svc.recover().await.unwrap();
        assert_eq!(
            store.view(&actor, &key).unwrap().items[0].state,
            super::super::types::QueueItemState::Dispatching
        );
    }
    #[tokio::test]
    async fn restart_uses_original_dispatch_receipt() {
        let fake = Arc::new(Fake {
            sent: Mutex::new(vec![]),
            busy: Mutex::new(false),
            lock: Arc::new(tokio::sync::Mutex::new(())),
            receipts: Mutex::new(std::collections::BTreeMap::new()),
            terminal_after_send: AtomicBool::new(false),
            delivery: SendDelivery::StartNow,
            changed_path_after_send: AtomicBool::new(false),
            revision: std::sync::atomic::AtomicU64::new(1),
            bound: AtomicBool::new(true),
            refresh_calls: std::sync::atomic::AtomicUsize::new(0),
            activate_calls: std::sync::atomic::AtomicUsize::new(0),
            activate_failures: std::sync::atomic::AtomicUsize::new(0),
            identity: None,
            blocked_session: AtomicBool::new(false),
            read_calls: std::sync::atomic::AtomicUsize::new(0),
            read_failures: std::sync::atomic::AtomicUsize::new(0),
            refresh_failures: std::sync::atomic::AtomicUsize::new(0),
            send_failure: Mutex::new(None),
        });
        let store = SqlLocalQueueStore::new(DeviceStore::memory());
        let actor = QueueActor {
            device_id: "d".into(),
            device_name: "phone".into(),
        };
        let key = TargetKey {
            session_id: "session".into(),
        };
        store
            .enqueue(
                &EnqueueCommand {
                    request_id: "B".into(),
                    actor: actor.clone(),
                    session_id: key.session_id.clone(),
                    workspace_path: "/synthetic".into(),
                    workspace_identity: None,
                    draft: QueueDraft {
                        text: "B".into(),
                        attachments: vec![],
                    },
                    prepared: PreparedSend::from_stored("B".into()).unwrap(),
                    edited_from: None,
                },
                1,
            )
            .unwrap();
        let target = fake.read(&key, Duration::ZERO).await.unwrap().target;
        let claim = store.claim_next(&key, &target, 2).unwrap().unwrap();
        fake.receipts.lock().unwrap().insert(
            claim.ctx.action_id.clone(),
            ActionReceipt {
                action_id: claim.ctx.action_id.clone(),
                session_id: key.session_id.clone(),
                generation: 1,
                state: ActionState::Accepted,
                reason_code: None,
                send_admission: Some(SendAdmission {
                    delivery: SendDelivery::StartNow,
                    input_id: "B-input".into(),
                }),
            },
        );
        let svc = ZcodeQueueService::new(store.clone(), fake.clone(), Arc::new(Gate));
        svc.recover().await.unwrap();
        let view = store.view(&actor, &key).unwrap();
        assert!(view.items.is_empty());
        assert!(!view.auto_drain);
        assert!(!store
            .observe_execution(
                &key,
                &super::super::types::ExecutionObservation {
                    input_id: "B-input".into(),
                    execution_id: None,
                    terminal: true,
                    revision: 100,
                    generation: 1
                }
            )
            .unwrap());
        assert!(fake.sent.lock().unwrap().is_empty());
    }
    #[tokio::test]
    async fn older_terminal_cannot_override_newer_running_observation() {
        let fake = Arc::new(Fake {
            sent: Mutex::new(vec![]),
            busy: Mutex::new(false),
            lock: Arc::new(tokio::sync::Mutex::new(())),
            receipts: Mutex::new(std::collections::BTreeMap::new()),
            terminal_after_send: AtomicBool::new(false),
            delivery: SendDelivery::StartNow,
            changed_path_after_send: AtomicBool::new(false),
            revision: std::sync::atomic::AtomicU64::new(1),
            bound: AtomicBool::new(true),
            refresh_calls: std::sync::atomic::AtomicUsize::new(0),
            activate_calls: std::sync::atomic::AtomicUsize::new(0),
            activate_failures: std::sync::atomic::AtomicUsize::new(0),
            identity: None,
            blocked_session: AtomicBool::new(false),
            read_calls: std::sync::atomic::AtomicUsize::new(0),
            read_failures: std::sync::atomic::AtomicUsize::new(0),
            refresh_failures: std::sync::atomic::AtomicUsize::new(0),
            send_failure: Mutex::new(None),
        });
        let store = SqlLocalQueueStore::new(DeviceStore::memory());
        let svc = ZcodeQueueService::new(store, fake.clone(), Arc::new(Gate));
        let key = TargetKey {
            session_id: "session".into(),
        };
        for text in ["B", "C"] {
            svc.enqueue(EnqueueCommand {
                request_id: text.into(),
                actor: QueueActor {
                    device_id: "d".into(),
                    device_name: "phone".into(),
                },
                session_id: key.session_id.clone(),
                workspace_path: "/synthetic".into(),
                workspace_identity: None,
                draft: QueueDraft {
                    text: text.into(),
                    attachments: vec![],
                },
                prepared: PreparedSend::from_stored(text.into()).unwrap(),
                edited_from: None,
            })
            .await
            .unwrap();
        }
        svc.flush_one(&key).await.unwrap();
        let mut older = fake.read(&key, Duration::ZERO).await.unwrap().view;
        older.revision = Some(2);
        older.execution_terminal = Some(ExecutionTerminal {
            input_id: "B-input".into(),
            result_type: "success".into(),
            revision: 2,
        });
        svc.on_session(&older);
        let mut newer = older.clone();
        newer.revision = Some(3);
        newer.execution_terminal = None;
        newer
            .control
            .as_mut()
            .unwrap()
            .active_works
            .push(ActiveWork {
                source_command_id: Some("B-input".into()),
                kind: "primaryTurn".into(),
                foreground_execution_id: Some("E-new".into()),
            });
        svc.on_session(&newer);
        fake.revision.store(3, Ordering::SeqCst);
        svc.flush_one(&key).await.unwrap();
        assert_eq!(
            *fake.sent.lock().unwrap(),
            vec!["B"],
            "old terminal cannot beat newer running fact"
        );
    }
    #[tokio::test]
    async fn offline_path_match_pins_first_fresh_identity() {
        let fake = Arc::new(Fake {
            sent: Mutex::new(vec![]),
            busy: Mutex::new(false),
            lock: Arc::new(tokio::sync::Mutex::new(())),
            receipts: Mutex::new(std::collections::BTreeMap::new()),
            terminal_after_send: AtomicBool::new(false),
            delivery: SendDelivery::StartNow,
            changed_path_after_send: AtomicBool::new(false),
            revision: std::sync::atomic::AtomicU64::new(1),
            bound: AtomicBool::new(true),
            refresh_calls: std::sync::atomic::AtomicUsize::new(0),
            activate_calls: std::sync::atomic::AtomicUsize::new(0),
            activate_failures: std::sync::atomic::AtomicUsize::new(0),
            identity: Some("fresh-identity".into()),
            blocked_session: AtomicBool::new(false),
            read_calls: std::sync::atomic::AtomicUsize::new(0),
            read_failures: std::sync::atomic::AtomicUsize::new(0),
            refresh_failures: std::sync::atomic::AtomicUsize::new(0),
            send_failure: Mutex::new(None),
        });
        let db = DeviceStore::memory();
        let store = SqlLocalQueueStore::new(db.clone());
        let svc = ZcodeQueueService::new(store, fake.clone(), Arc::new(Gate));
        let key = TargetKey {
            session_id: "session".into(),
        };
        svc.enqueue(EnqueueCommand {
            request_id: "B".into(),
            actor: QueueActor {
                device_id: "d".into(),
                device_name: "phone".into(),
            },
            session_id: key.session_id.clone(),
            workspace_path: "/synthetic".into(),
            workspace_identity: None,
            draft: QueueDraft {
                text: "B".into(),
                attachments: vec![],
            },
            prepared: PreparedSend::from_stored("B".into()).unwrap(),
            edited_from: None,
        })
        .await
        .unwrap();
        svc.flush_one(&key).await.unwrap();
        assert_eq!(*fake.sent.lock().unwrap(), vec!["B"]);
        let pinned: Option<String> = db.with(|c| {
            c.query_row(
                "SELECT workspace_identity FROM zcode_send_queue WHERE request_id='B'",
                [],
                |r| r.get(0),
            )
            .unwrap()
        });
        assert_eq!(pinned.as_deref(), Some("fresh-identity"));
    }
    #[tokio::test]
    async fn queued_item_changed_workspace_pauses_without_dispatch() {
        let fake = Arc::new(Fake {
            sent: Mutex::new(vec![]),
            busy: Mutex::new(false),
            lock: Arc::new(tokio::sync::Mutex::new(())),
            receipts: Mutex::new(std::collections::BTreeMap::new()),
            terminal_after_send: AtomicBool::new(false),
            delivery: SendDelivery::StartNow,
            changed_path_after_send: AtomicBool::new(false),
            revision: std::sync::atomic::AtomicU64::new(1),
            bound: AtomicBool::new(true),
            refresh_calls: std::sync::atomic::AtomicUsize::new(0),
            activate_calls: std::sync::atomic::AtomicUsize::new(0),
            activate_failures: std::sync::atomic::AtomicUsize::new(0),
            identity: None,
            blocked_session: AtomicBool::new(false),
            read_calls: std::sync::atomic::AtomicUsize::new(0),
            read_failures: std::sync::atomic::AtomicUsize::new(0),
            refresh_failures: std::sync::atomic::AtomicUsize::new(0),
            send_failure: Mutex::new(None),
        });
        let store = SqlLocalQueueStore::new(DeviceStore::memory());
        let svc = ZcodeQueueService::new(store.clone(), fake.clone(), Arc::new(Gate));
        let actor = QueueActor {
            device_id: "d".into(),
            device_name: "phone".into(),
        };
        let key = TargetKey {
            session_id: "session".into(),
        };
        svc.enqueue(EnqueueCommand {
            request_id: "B".into(),
            actor: actor.clone(),
            session_id: key.session_id.clone(),
            workspace_path: "/synthetic".into(),
            workspace_identity: Some("old".into()),
            draft: QueueDraft {
                text: "B".into(),
                attachments: vec![],
            },
            prepared: PreparedSend::from_stored("B".into()).unwrap(),
            edited_from: None,
        })
        .await
        .unwrap();
        let _ = svc.flush_one(&key).await;
        assert!(fake.sent.lock().unwrap().is_empty());
        assert!(
            !store.view(&actor, &key).unwrap().auto_drain,
            "changed target must pause"
        );
    }
    #[tokio::test]
    async fn changed_workspace_cannot_confirm_previous_execution() {
        let fake = Arc::new(Fake {
            sent: Mutex::new(vec![]),
            busy: Mutex::new(false),
            lock: Arc::new(tokio::sync::Mutex::new(())),
            receipts: Mutex::new(std::collections::BTreeMap::new()),
            terminal_after_send: AtomicBool::new(true),
            delivery: SendDelivery::StartNow,
            changed_path_after_send: AtomicBool::new(true),
            revision: std::sync::atomic::AtomicU64::new(1),
            bound: AtomicBool::new(true),
            refresh_calls: std::sync::atomic::AtomicUsize::new(0),
            activate_calls: std::sync::atomic::AtomicUsize::new(0),
            activate_failures: std::sync::atomic::AtomicUsize::new(0),
            identity: None,
            blocked_session: AtomicBool::new(false),
            read_calls: std::sync::atomic::AtomicUsize::new(0),
            read_failures: std::sync::atomic::AtomicUsize::new(0),
            refresh_failures: std::sync::atomic::AtomicUsize::new(0),
            send_failure: Mutex::new(None),
        });
        let store = SqlLocalQueueStore::new(DeviceStore::memory());
        let svc = ZcodeQueueService::new(store.clone(), fake.clone(), Arc::new(Gate));
        let key = TargetKey {
            session_id: "session".into(),
        };
        let target = fake.read(&key, Duration::ZERO).await.unwrap().target;
        for text in ["B", "C"] {
            svc.enqueue(EnqueueCommand {
                request_id: text.into(),
                actor: QueueActor {
                    device_id: "d".into(),
                    device_name: "phone".into(),
                },
                session_id: key.session_id.clone(),
                workspace_path: "/synthetic".into(),
                workspace_identity: None,
                draft: QueueDraft {
                    text: text.into(),
                    attachments: vec![],
                },
                prepared: PreparedSend::from_stored(text.into()).unwrap(),
                edited_from: None,
            })
            .await
            .unwrap();
        }
        svc.flush_one(&key).await.unwrap();
        assert_eq!(*fake.sent.lock().unwrap(), vec!["B"]);
        let changed_event = fake.read(&key, Duration::ZERO).await.unwrap();
        svc.on_session(&changed_event.view);
        svc.flush_one(&key).await.unwrap();
        assert!(
            !store
                .view(
                    &QueueActor {
                        device_id: "d".into(),
                        device_name: "phone".into()
                    },
                    &key
                )
                .unwrap()
                .auto_drain,
            "changed workspace must pause queue"
        );
        assert!(
            store
                .claim_next(&key, &target, chrono::Utc::now().timestamp_millis())
                .unwrap()
                .is_none(),
            "terminal from changed workspace cannot release barrier"
        );
        let changed = fake.read(&key, Duration::ZERO).await.unwrap();
        svc.on_session(&changed.view);
        let _ = svc.flush_one(&key).await;
        fake.changed_path_after_send.store(false, Ordering::SeqCst);
        fake.terminal_after_send.store(false, Ordering::SeqCst);
        fake.revision.store(3, Ordering::SeqCst);
        svc.flush_one(&key).await.unwrap();
        assert_eq!(
            *fake.sent.lock().unwrap(),
            vec!["B"],
            "old observation must not revive after workspace returns"
        );
    }
    #[tokio::test]
    async fn unexpected_official_queue_admission_never_resends() {
        let fake = Arc::new(Fake {
            sent: Mutex::new(vec![]),
            busy: Mutex::new(false),
            lock: Arc::new(tokio::sync::Mutex::new(())),
            receipts: Mutex::new(std::collections::BTreeMap::new()),
            terminal_after_send: AtomicBool::new(false),
            delivery: SendDelivery::Queue,
            changed_path_after_send: AtomicBool::new(false),
            revision: std::sync::atomic::AtomicU64::new(1),
            bound: AtomicBool::new(true),
            refresh_calls: std::sync::atomic::AtomicUsize::new(0),
            activate_calls: std::sync::atomic::AtomicUsize::new(0),
            activate_failures: std::sync::atomic::AtomicUsize::new(0),
            identity: None,
            blocked_session: AtomicBool::new(false),
            read_calls: std::sync::atomic::AtomicUsize::new(0),
            read_failures: std::sync::atomic::AtomicUsize::new(0),
            refresh_failures: std::sync::atomic::AtomicUsize::new(0),
            send_failure: Mutex::new(None),
        });
        let store = SqlLocalQueueStore::new(DeviceStore::memory());
        let svc = ZcodeQueueService::new(store.clone(), fake.clone(), Arc::new(Gate));
        let actor = QueueActor {
            device_id: "d".into(),
            device_name: "phone".into(),
        };
        let key = TargetKey {
            session_id: "session".into(),
        };
        for text in ["B", "C"] {
            svc.enqueue(EnqueueCommand {
                request_id: text.into(),
                actor: actor.clone(),
                session_id: key.session_id.clone(),
                workspace_path: "/synthetic".into(),
                workspace_identity: None,
                draft: QueueDraft {
                    text: text.into(),
                    attachments: vec![],
                },
                prepared: PreparedSend::from_stored(text.into()).unwrap(),
                edited_from: None,
            })
            .await
            .unwrap();
        }
        svc.flush_one(&key).await.unwrap();
        svc.flush_one(&key).await.unwrap();
        assert_eq!(*fake.sent.lock().unwrap(), vec!["B"]);
        let paused = store.view(&actor, &key).unwrap();
        assert!(!paused.auto_drain);
        store
            .resume(&actor, &key, "resume", paused.version)
            .unwrap();
        svc.flush_one(&key).await.unwrap();
        assert_eq!(*fake.sent.lock().unwrap(), vec!["B", "C"]);
    }
    #[tokio::test]
    async fn terminal_seen_during_ack_wakes_next_fifo_item() {
        let fake = Arc::new(Fake {
            sent: Mutex::new(vec![]),
            busy: Mutex::new(false),
            lock: Arc::new(tokio::sync::Mutex::new(())),
            receipts: Mutex::new(std::collections::BTreeMap::new()),
            terminal_after_send: AtomicBool::new(true),
            delivery: SendDelivery::StartNow,
            changed_path_after_send: AtomicBool::new(false),
            revision: std::sync::atomic::AtomicU64::new(1),
            bound: AtomicBool::new(true),
            refresh_calls: std::sync::atomic::AtomicUsize::new(0),
            activate_calls: std::sync::atomic::AtomicUsize::new(0),
            activate_failures: std::sync::atomic::AtomicUsize::new(0),
            identity: None,
            blocked_session: AtomicBool::new(false),
            read_calls: std::sync::atomic::AtomicUsize::new(0),
            read_failures: std::sync::atomic::AtomicUsize::new(0),
            refresh_failures: std::sync::atomic::AtomicUsize::new(0),
            send_failure: Mutex::new(None),
        });
        let svc = Arc::new(ZcodeQueueService::new(
            SqlLocalQueueStore::new(DeviceStore::memory()),
            fake.clone(),
            Arc::new(Gate),
        ));
        for text in ["B", "C"] {
            svc.enqueue(EnqueueCommand {
                request_id: text.into(),
                actor: QueueActor {
                    device_id: "d".into(),
                    device_name: "phone".into(),
                },
                session_id: "session".into(),
                workspace_path: "/synthetic".into(),
                workspace_identity: None,
                draft: QueueDraft {
                    text: text.into(),
                    attachments: vec![],
                },
                prepared: PreparedSend::from_stored(text.into()).unwrap(),
                edited_from: None,
            })
            .await
            .unwrap();
        }
        svc.wake.notified().await;
        svc.start();
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if fake.sent.lock().unwrap().len() == 2 {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("post-ACK terminal must wake C");
        assert_eq!(*fake.sent.lock().unwrap(), vec!["B", "C"]);
        svc.shutdown();
    }
    #[tokio::test]
    async fn worker_start_drains_existing_idle_queue_once() {
        let fake = Arc::new(Fake {
            sent: Mutex::new(vec![]),
            busy: Mutex::new(false),
            lock: Arc::new(tokio::sync::Mutex::new(())),
            receipts: Mutex::new(std::collections::BTreeMap::new()),
            terminal_after_send: AtomicBool::new(false),
            delivery: SendDelivery::StartNow,
            changed_path_after_send: AtomicBool::new(false),
            revision: std::sync::atomic::AtomicU64::new(1),
            bound: AtomicBool::new(true),
            refresh_calls: std::sync::atomic::AtomicUsize::new(0),
            activate_calls: std::sync::atomic::AtomicUsize::new(0),
            activate_failures: std::sync::atomic::AtomicUsize::new(0),
            identity: None,
            blocked_session: AtomicBool::new(false),
            read_calls: std::sync::atomic::AtomicUsize::new(0),
            read_failures: std::sync::atomic::AtomicUsize::new(0),
            refresh_failures: std::sync::atomic::AtomicUsize::new(0),
            send_failure: Mutex::new(None),
        });
        let svc = Arc::new(ZcodeQueueService::new(
            SqlLocalQueueStore::new(DeviceStore::memory()),
            fake.clone(),
            Arc::new(Gate),
        ));
        svc.enqueue(EnqueueCommand {
            request_id: "B".into(),
            actor: QueueActor {
                device_id: "d".into(),
                device_name: "phone".into(),
            },
            session_id: "session".into(),
            workspace_path: "/synthetic".into(),
            workspace_identity: None,
            draft: QueueDraft {
                text: "B".into(),
                attachments: vec![],
            },
            prepared: PreparedSend::from_stored("B".into()).unwrap(),
            edited_from: None,
        })
        .await
        .unwrap();
        svc.start();
        svc.start();
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if !fake.sent.lock().unwrap().is_empty() {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("worker must dispatch");
        assert_eq!(*fake.sent.lock().unwrap(), vec!["B"]);
        svc.shutdown();
    }
    #[tokio::test]
    async fn busy_enqueue_emits_zero_upstream_frames_and_idle_fifo_waits() {
        let fake = Arc::new(Fake {
            sent: Mutex::new(vec![]),
            busy: Mutex::new(true),
            lock: Arc::new(tokio::sync::Mutex::new(())),
            receipts: Mutex::new(std::collections::BTreeMap::new()),
            terminal_after_send: AtomicBool::new(false),
            delivery: SendDelivery::StartNow,
            changed_path_after_send: AtomicBool::new(false),
            revision: std::sync::atomic::AtomicU64::new(1),
            bound: AtomicBool::new(true),
            refresh_calls: std::sync::atomic::AtomicUsize::new(0),
            activate_calls: std::sync::atomic::AtomicUsize::new(0),
            activate_failures: std::sync::atomic::AtomicUsize::new(0),
            identity: None,
            blocked_session: AtomicBool::new(false),
            read_calls: std::sync::atomic::AtomicUsize::new(0),
            read_failures: std::sync::atomic::AtomicUsize::new(0),
            refresh_failures: std::sync::atomic::AtomicUsize::new(0),
            send_failure: Mutex::new(None),
        });
        let svc = ZcodeQueueService::new(
            SqlLocalQueueStore::new(DeviceStore::memory()),
            fake.clone(),
            Arc::new(Gate),
        );
        let key = TargetKey {
            session_id: "session".into(),
        };
        for text in ["B", "C"] {
            svc.enqueue(EnqueueCommand {
                request_id: text.into(),
                actor: QueueActor {
                    device_id: "d".into(),
                    device_name: "phone".into(),
                },
                session_id: "session".into(),
                workspace_path: "/synthetic".into(),
                workspace_identity: None,
                draft: QueueDraft {
                    text: text.into(),
                    attachments: vec![],
                },
                prepared: PreparedSend::from_stored(text.into()).unwrap(),
                edited_from: None,
            })
            .await
            .unwrap();
        }
        svc.flush_one(&key).await.unwrap();
        assert!(fake.sent.lock().unwrap().is_empty());
        assert_eq!(fake.refresh_calls.load(Ordering::SeqCst), 0);
        *fake.busy.lock().unwrap() = false;
        svc.flush_one(&key).await.unwrap();
        svc.flush_one(&key).await.unwrap();
        assert_eq!(*fake.sent.lock().unwrap(), vec!["B"]);
        assert!(
            fake.refresh_calls.load(Ordering::SeqCst) > 0,
            "idle dispatch must refresh target"
        );
        let mut view = fake.read(&key, Duration::ZERO).await.unwrap().view;
        view.revision = Some(2);
        view.control
            .as_mut()
            .unwrap()
            .active_works
            .push(ActiveWork {
                source_command_id: Some("B-input".into()),
                kind: "primaryTurn".into(),
                foreground_execution_id: Some("B-execution".into()),
            });
        svc.on_session(&view);
        fake.revision.store(2, Ordering::SeqCst);
        let mut missing = view.clone();
        missing.control.as_mut().unwrap().active_works[0].source_command_id = None;
        missing.revision = Some(3);
        svc.on_session(&missing);
        fake.revision.store(3, Ordering::SeqCst);
        svc.flush_one(&key).await.unwrap();
        assert_eq!(*fake.sent.lock().unwrap(), vec!["B"]);
        view.control.as_mut().unwrap().active_works.clear();
        view.revision = Some(3);
        view.execution_terminal = Some(ExecutionTerminal {
            input_id: "another-input".into(),
            result_type: "success".into(),
            revision: 3,
        });
        svc.on_session(&view);
        svc.flush_one(&key).await.unwrap();
        assert_eq!(*fake.sent.lock().unwrap(), vec!["B"]);
        view.execution_terminal = Some(ExecutionTerminal {
            input_id: "B-input".into(),
            result_type: "success".into(),
            revision: 4,
        });
        view.revision = Some(4);
        fake.revision.store(4, Ordering::SeqCst);
        let mut old_generation = view.clone();
        old_generation.generation = 0;
        svc.on_session(&old_generation);
        svc.flush_one(&key).await.unwrap();
        assert_eq!(*fake.sent.lock().unwrap(), vec!["B"]);
        let mut other_session = view.clone();
        other_session.session_id = "other".into();
        svc.on_session(&other_session);
        svc.flush_one(&key).await.unwrap();
        assert_eq!(*fake.sent.lock().unwrap(), vec!["B"]);
        svc.on_session(&view);
        svc.flush_one(&key).await.unwrap();
        assert_eq!(*fake.sent.lock().unwrap(), vec!["B", "C"]);
    }
}
