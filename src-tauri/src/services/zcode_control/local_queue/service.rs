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
#[allow(dead_code)]
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
    #[cfg(test)]
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
#[cfg(test)]
type BeforePriorityHook = Arc<dyn Fn(&str) + Send + Sync>;

pub(crate) struct ZcodeQueueService {
    store: SqlLocalQueueStore,
    host: Arc<dyn QueueHostPort>,
    gate: Arc<dyn QueueRuntimeGate>,
    started: AtomicBool,
    cleanup_started: AtomicBool,
    stopped: AtomicBool,
    wake: Notify,
    #[cfg(test)]
    queue_events: tokio::sync::broadcast::Sender<String>,
    cleanup_wake: Notify,
    shutdown_wake: Notify,
    activation_wake: Notify,
    activation_lock: tokio::sync::Mutex<()>,
    activation_requests: std::sync::atomic::AtomicU64,
    activation_processed: std::sync::atomic::AtomicU64,
    activation_force_requests: std::sync::atomic::AtomicU64,
    #[cfg(test)]
    activation_invocations: std::sync::atomic::AtomicUsize,
    #[cfg(test)]
    before_priority_port: std::sync::Mutex<Option<BeforePriorityHook>>,
    observations: std::sync::Mutex<
        std::collections::BTreeMap<String, Vec<super::types::ExecutionObservation>>,
    >,
    manual_claim_failures:
        std::sync::Mutex<std::collections::BTreeMap<String, super::types::StopFailure>>,
    recovered: tokio::sync::Mutex<bool>,
}

struct QueueEventGuard {
    #[cfg(test)]
    sender: tokio::sync::broadcast::Sender<String>,
    host: Arc<dyn QueueHostPort>,
    session_id: String,
}
impl Drop for QueueEventGuard {
    fn drop(&mut self) {
        #[cfg(test)]
        let _ = self.sender.send(self.session_id.clone());
        self.host.publish_queue_change(&self.session_id);
    }
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
            #[cfg(test)]
            queue_events: tokio::sync::broadcast::channel(64).0,
            cleanup_wake: Notify::new(),
            shutdown_wake: Notify::new(),
            activation_wake: Notify::new(),
            activation_lock: tokio::sync::Mutex::new(()),
            activation_requests: std::sync::atomic::AtomicU64::new(0),
            activation_processed: std::sync::atomic::AtomicU64::new(0),
            activation_force_requests: std::sync::atomic::AtomicU64::new(0),
            #[cfg(test)]
            activation_invocations: std::sync::atomic::AtomicUsize::new(0),
            #[cfg(test)]
            before_priority_port: std::sync::Mutex::new(None),
            observations: std::sync::Mutex::new(std::collections::BTreeMap::new()),
            manual_claim_failures: std::sync::Mutex::new(std::collections::BTreeMap::new()),
            recovered: tokio::sync::Mutex::new(false),
        }
    }
    fn remember_manual_claim_failure(&self, request_id: &str, failure: super::types::StopFailure) {
        const MAX_PENDING_FAILURES: usize = 128;
        let mut pending = self
            .manual_claim_failures
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if !pending.contains_key(request_id) && pending.len() >= MAX_PENDING_FAILURES {
            return;
        }
        pending.insert(request_id.to_owned(), failure);
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
        // 只读原停止动作回执；恢复永不使用旧目标重新发帧，也不凭回执自动解除人工核对。
        for pending in self.store.pending_priority_recovery()? {
            let _ = self
                .host
                .receipt(&pending.stop_action_id, &pending.device_id)
                .await?;
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
                let deadline = match cleaner.store.cleanup_expired_edits(now).and_then(|count| {
                    if count > 0 {
                        #[cfg(test)]
                        let _ = cleaner.queue_events.send(String::new());
                        cleaner.host.publish_queue_change("");
                    }
                    cleaner.store.next_edit_expiry()
                }) {
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
                            let result = service.flush_one_inner(&key).await;
                            #[cfg(test)]
                            let _ = service.queue_events.send(key.session_id.clone());
                            service.host.publish_queue_change(&key.session_id);
                            match result {
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
    #[cfg(test)]
    pub(crate) fn subscribe_queue(&self) -> tokio::sync::broadcast::Receiver<String> {
        self.queue_events.subscribe()
    }
    pub(crate) fn notify(&self, key: &TargetKey) {
        #[cfg(test)]
        let _ = self.queue_events.send(key.session_id.clone());
        self.host.publish_queue_change(&key.session_id);
        self.wake.notify_one();
        self.cleanup_wake.notify_one();
    }
    pub(crate) async fn prioritize(
        self: &Arc<Self>,
        actor: &super::types::QueueActor,
        cmd: super::types::PriorityCommand,
    ) -> ZResult<super::types::QueueOperationView> {
        let actor = actor.clone();
        let request_id = cmd.request_id.clone();
        let worker = self.clone();
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(60);
        let task_actor = actor.clone();
        let mut task =
            tokio::spawn(async move { worker.prioritize_inner(&task_actor, cmd, deadline).await });
        match tokio::time::timeout_at(deadline, &mut task).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(super::super::types::ZcodeError::of(
                super::super::types::codes::NOT_READY,
            )),
            Err(_) => self.store.operation(&actor, &request_id)?.ok_or_else(|| {
                super::super::types::ZcodeError::of(super::super::types::codes::NOT_READY)
            }),
        }
    }
    async fn prioritize_inner(
        &self,
        actor: &super::types::QueueActor,
        cmd: super::types::PriorityCommand,
        deadline: tokio::time::Instant,
    ) -> ZResult<super::types::QueueOperationView> {
        use super::super::types::{codes, ActionContext, ActionReceipt, ActionState, ZcodeError};
        use super::types::{ItemCommand, PriorityEnd};
        let key = self.store.item_key(actor, &cmd.item_id)?;
        let lock = self.host.session_lock(&key);
        let _guard = lock.lock().await;
        if tokio::time::Instant::now() >= deadline {
            return Err(ZcodeError::of(codes::NOT_READY));
        }
        let now = || chrono::Utc::now().timestamp_millis();
        let owner = self.store.begin_priority(actor, &cmd, now())?;
        let _event_guard = QueueEventGuard {
            #[cfg(test)]
            sender: self.queue_events.clone(),
            host: self.host.clone(),
            session_id: key.session_id.clone(),
        };
        #[cfg(test)]
        let _ = self.queue_events.send(key.session_id.clone());
        self.host.publish_queue_change(&key.session_id);
        if !owner.is_owner {
            if owner.operation.state == super::types::QueueOperationState::InFlight {
                match self.store.finish_priority_safe(
                    actor,
                    &cmd.request_id,
                    PriorityEnd::BeforeStop,
                    now(),
                ) {
                    Ok(()) => return Ok(self.store.operation(actor, &cmd.request_id)?.unwrap()),
                    Err(error) if error.is(codes::INVALID_ACTION) => {}
                    Err(error) => return Err(error),
                }
            }
            return Ok(owner.operation);
        }
        let before = async {
            if self.stopped.load(Ordering::SeqCst)
                || self.gate.shutting_down()
                || !self.gate.remote_enabled()
                || !self.gate.device_valid(&actor.device_id)
            {
                return Err(ZcodeError::of(codes::NOT_READY));
            }
            let bound = self
                .host
                .read(&key, std::time::Duration::from_secs(2))
                .await?;
            let fresh = self
                .host
                .refresh_target(&key, std::time::Duration::from_secs(2))
                .await?;
            if fresh != bound.target
                || !bound.view.ready
                || !bound.view.pending.is_empty()
                || bound.view.control.is_none()
            {
                return Err(ZcodeError::of(codes::NOT_READY));
            }
            Ok(bound)
        }
        .await;
        let bound = match before {
            Ok(v) => v,
            Err(e) => {
                self.store.finish_priority_safe(
                    actor,
                    &cmd.request_id,
                    PriorityEnd::BeforeStopFailure {
                        code: e.code.clone(),
                    },
                    now(),
                )?;
                return Err(e);
            }
        };
        let matches = match self
            .store
            .priority_item_matches_target(actor, &cmd, &bound.target)
        {
            Ok(matches) => matches,
            Err(error) => {
                let _ = self.store.finish_priority_safe(
                    actor,
                    &cmd.request_id,
                    PriorityEnd::BeforeStopFailure {
                        code: error.code.clone(),
                    },
                    now(),
                );
                return Err(error);
            }
        };
        if !matches {
            self.store.finish_priority_safe(
                actor,
                &cmd.request_id,
                PriorityEnd::BeforeStopFailure {
                    code: codes::NO_SESSION.into(),
                },
                now(),
            )?;
            return Err(ZcodeError::of(codes::NO_SESSION));
        }
        match self
            .store
            .attachments_valid(&key, Some(&cmd.item_id), &bound.target.workspace_path)
        {
            Ok(true) => {}
            result => {
                let error = match result {
                    Ok(false) => ZcodeError::of(codes::ATTACHMENT_MISSING),
                    Err(error) => error,
                    Ok(true) => unreachable!(),
                };
                let _ = self.store.finish_priority_safe(
                    actor,
                    &cmd.request_id,
                    PriorityEnd::BeforeStopFailure {
                        code: error.code.clone(),
                    },
                    now(),
                );
                return Err(error);
            }
        }
        self.observe_bound(&key, &bound)?;
        let works = &bound.view.control.as_ref().unwrap().active_works;
        // 同帧新鲜终态已在 observe_bound 收口；仍有的未核对屏障一律拒绝（零 stop/send）。
        if self.store.has_review_barrier(&key)? {
            self.store.finish_priority_safe(
                actor,
                &cmd.request_id,
                PriorityEnd::BeforeStop,
                now(),
            )?;
            return Err(ZcodeError::of(codes::QUEUE_REVIEW_REQUIRED));
        }
        if works.is_empty() && self.store.has_delivery_barrier(&key)? {
            self.store.finish_priority_safe(
                actor,
                &cmd.request_id,
                PriorityEnd::BeforeStop,
                now(),
            )?;
            return Ok(self.store.operation(actor, &cmd.request_id)?.unwrap());
        }
        if !works.is_empty() {
            let expected = match (&cmd.expected_execution_id, cmd.confirm_stop) {
                (Some(id), true)
                    if works.len() == 1
                        && works[0].foreground_execution_id.as_ref() == Some(id) =>
                {
                    id
                }
                _ => {
                    self.store.finish_priority_safe(
                        actor,
                        &cmd.request_id,
                        PriorityEnd::BeforeStop,
                        now(),
                    )?;
                    return Err(ZcodeError::of(codes::INVALID_ACTION));
                }
            };
            if tokio::time::Instant::now() >= deadline {
                self.store.finish_priority_safe(
                    actor,
                    &cmd.request_id,
                    PriorityEnd::BeforeStop,
                    now(),
                )?;
                return Ok(self.store.operation(actor, &cmd.request_id)?.unwrap());
            }
            match self.store.mark_stop_claimed(
                actor,
                &cmd.request_id,
                bound.target.generation,
                expected,
            ) {
                Ok(true) => {}
                Ok(false) => return Ok(owner.operation),
                Err(error) => {
                    // 未调用停止端口；仅当事务确实未认领时才能安全释放。
                    let _ = self.store.finish_priority_safe(
                        actor,
                        &cmd.request_id,
                        PriorityEnd::BeforeStopFailure {
                            code: error.code.clone(),
                        },
                        now(),
                    );
                    return Err(error);
                }
            }
            let ctx = ActionContext {
                action_id: owner.stop_action_id,
                device_id: actor.device_id.clone(),
                device_name: actor.device_name.clone(),
            };
            #[cfg(test)]
            if let Some(hook) = self.before_priority_port.lock().unwrap().as_ref() {
                hook("stop");
            }
            let attachment_error = match self.store.attachments_valid(
                &key,
                Some(&cmd.item_id),
                &bound.target.workspace_path,
            ) {
                Ok(true) => None,
                Ok(false) => Some(codes::ATTACHMENT_MISSING.to_owned()),
                Err(error) => Some(error.code),
            };
            if let Some(code) = attachment_error {
                self.store.finish_priority_safe(
                    actor,
                    &cmd.request_id,
                    PriorityEnd::StopBeforeWriteFailure {
                        action_id: ctx.action_id.clone(),
                        generation: bound.target.generation,
                        code,
                    },
                    now(),
                )?;
                return Ok(self.store.operation(actor, &cmd.request_id)?.unwrap());
            }
            if tokio::time::Instant::now() >= deadline {
                self.store.finish_priority_safe(
                    actor,
                    &cmd.request_id,
                    PriorityEnd::StopBeforeWrite {
                        action_id: ctx.action_id.clone(),
                        generation: bound.target.generation,
                    },
                    now(),
                )?;
                return Ok(self.store.operation(actor, &cmd.request_id)?.unwrap());
            }
            let receipt = match self.host.stop(&ctx, &bound.target, expected).await {
                Ok(r) => r,
                Err(super::super::command::PreparedDispatchError::BeforeWrite(error)) => {
                    self.store.finish_priority_safe(
                        actor,
                        &cmd.request_id,
                        PriorityEnd::StopBeforeWriteFailure {
                            action_id: ctx.action_id.clone(),
                            generation: bound.target.generation,
                            code: error.code,
                        },
                        now(),
                    )?;
                    return Ok(self.store.operation(actor, &cmd.request_id)?.unwrap());
                }
                Err(super::super::command::PreparedDispatchError::MayHaveWritten(error)) => {
                    self.store.mark_priority_unknown_with_failure(
                        actor,
                        &cmd.request_id,
                        Some(super::types::StopFailure {
                            phase: "may_have_written".into(),
                            code: error.code,
                        }),
                        now(),
                    )?;
                    return Ok(self.store.operation(actor, &cmd.request_id)?.unwrap());
                }
            };
            if matches!(
                receipt.state,
                ActionState::Rejected | ActionState::Noop | ActionState::SendFailed
            ) {
                self.store.finish_priority_safe(
                    actor,
                    &cmd.request_id,
                    PriorityEnd::StopRejected(receipt),
                    now(),
                )?;
                return Ok(self.store.operation(actor, &cmd.request_id)?.unwrap());
            }
            if receipt.state != ActionState::Confirmed
                || receipt.action_id != ctx.action_id
                || receipt.session_id != key.session_id
                || receipt.generation != bound.target.generation
            {
                self.store
                    .mark_priority_unknown(actor, &cmd.request_id, now())?;
                return Ok(self.store.operation(actor, &cmd.request_id)?.unwrap());
            }
        }
        let after = self
            .host
            .read(&key, std::time::Duration::from_secs(2))
            .await;
        let fresh = self
            .host
            .refresh_target(&key, std::time::Duration::from_secs(2))
            .await;
        let valid = match (after, fresh) {
            (Ok(v), Ok(t)) => {
                let valid = v.target == bound.target
                    && t == bound.target
                    && v.view.ready
                    && v.view.pending.is_empty()
                    && v.view.capabilities.send.enabled
                    && v.view
                        .control
                        .as_ref()
                        .is_some_and(|c| c.active_works.is_empty());
                if valid {
                    self.observe_bound(&key, &v)?;
                }
                valid
            }
            _ => false,
        };
        let valid = valid && !self.store.has_delivery_barrier(&key)?;
        if !valid
            || tokio::time::Instant::now() >= deadline
            || self.stopped.load(Ordering::SeqCst)
            || self.gate.shutting_down()
            || !self.gate.remote_enabled()
            || !self.gate.device_valid(&actor.device_id)
        {
            if works.is_empty() {
                self.store.finish_priority_safe(
                    actor,
                    &cmd.request_id,
                    PriorityEnd::BeforeStop,
                    now(),
                )?;
            } else {
                self.store
                    .mark_priority_unknown(actor, &cmd.request_id, now())?;
            }
            return Ok(self.store.operation(actor, &cmd.request_id)?.unwrap());
        }
        let item = ItemCommand {
            request_id: cmd.request_id.clone(),
            item_id: cmd.item_id.clone(),
            expected_version: cmd.expected_version,
        };
        let claim = match self
            .store
            .claim_item(actor, &item, &bound.target, &cmd.request_id, now())
        {
            Ok(claim) => claim,
            Err(error) => {
                if works.is_empty() {
                    let _ = self.store.finish_priority_safe(
                        actor,
                        &cmd.request_id,
                        PriorityEnd::BeforeStop,
                        now(),
                    );
                } else {
                    let _ = self
                        .store
                        .mark_priority_unknown(actor, &cmd.request_id, now());
                }
                return Err(error);
            }
        };
        #[cfg(test)]
        let _ = self.queue_events.send(key.session_id.clone());
        self.host.publish_queue_change(&key.session_id);
        #[cfg(test)]
        if let Some(hook) = self.before_priority_port.lock().unwrap().as_ref() {
            hook("send");
        }
        let attachment_error = match self.store.attachments_valid(
            &key,
            Some(&claim.item_id),
            &claim.target.workspace_path,
        ) {
            Ok(true) => None,
            Ok(false) => Some("attachment_missing".to_owned()),
            Err(error) => Some(error.code),
        };
        let receipt = if attachment_error.is_some()
            || tokio::time::Instant::now() >= deadline
            || self.stopped.load(Ordering::SeqCst)
            || self.gate.shutting_down()
            || !self.gate.remote_enabled()
            || !self.gate.device_valid(&actor.device_id)
        {
            ActionReceipt {
                action_id: claim.ctx.action_id.clone(),
                session_id: claim.target.session_id.clone(),
                generation: claim.target.generation,
                state: ActionState::SendFailed,
                reason_code: Some(attachment_error.unwrap_or_else(|| codes::NOT_READY.to_owned())),
                send_admission: None,
            }
        } else {
            match self
                .host
                .send_prepared(
                    &claim.ctx,
                    &claim.target,
                    &claim.prepared,
                    cmd.held_queue.as_ref(),
                )
                .await
            {
                Ok(r) => r,
                Err(error) => {
                    let (state, reason) = match error {
                        super::super::command::PreparedDispatchError::BeforeWrite(e) => {
                            (ActionState::SendFailed, e.code)
                        }
                        super::super::command::PreparedDispatchError::MayHaveWritten(e) => {
                            (ActionState::Unknown, e.code)
                        }
                    };
                    ActionReceipt {
                        action_id: claim.ctx.action_id.clone(),
                        session_id: claim.target.session_id.clone(),
                        generation: claim.target.generation,
                        state,
                        reason_code: Some(reason),
                        send_admission: None,
                    }
                }
            }
        };
        self.store.settle_dispatch(&claim, &receipt, now())?;
        Ok(self.store.operation(actor, &cmd.request_id)?.unwrap())
    }
    pub(crate) async fn stop_current(
        self: &Arc<Self>,
        actor: &super::types::QueueActor,
        key: &TargetKey,
        request_id: &str,
        execution: &str,
    ) -> ZResult<super::types::QueueOperationView> {
        let worker = self.clone();
        let actor = actor.clone();
        let key = key.clone();
        let request = request_id.to_owned();
        let execution = execution.to_owned();
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(60);
        let actor_for_task = actor.clone();
        let request_for_task = request.clone();
        let mut task = tokio::spawn(async move {
            worker
                .stop_current_inner(
                    &actor_for_task,
                    &key,
                    &request_for_task,
                    &execution,
                    deadline,
                )
                .await
        });
        match tokio::time::timeout_at(deadline, &mut task).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(super::super::types::ZcodeError::of(
                super::super::types::codes::NOT_READY,
            )),
            Err(_) => self.store.operation(&actor, &request)?.ok_or_else(|| {
                super::super::types::ZcodeError::of(super::super::types::codes::NOT_READY)
            }),
        }
    }
    async fn stop_current_inner(
        &self,
        actor: &super::types::QueueActor,
        key: &TargetKey,
        request_id: &str,
        execution: &str,
        deadline: tokio::time::Instant,
    ) -> ZResult<super::types::QueueOperationView> {
        use super::super::types::{codes, ActionContext, ActionState, ZcodeError};
        use super::types::QueueOperationState;
        let lock = self.host.session_lock(key);
        let _guard = lock.lock().await;
        if tokio::time::Instant::now() >= deadline {
            return Err(ZcodeError::of(codes::NOT_READY));
        }
        let now = chrono::Utc::now().timestamp_millis();
        let (operation, owner, action_id) = self
            .store
            .begin_stop(actor, key, request_id, execution, now)?;
        let _event_guard = QueueEventGuard {
            #[cfg(test)]
            sender: self.queue_events.clone(),
            host: self.host.clone(),
            session_id: key.session_id.clone(),
        };
        #[cfg(test)]
        let _ = self.queue_events.send(key.session_id.clone());
        self.host.publish_queue_change(&key.session_id);
        if !owner {
            if operation.state == QueueOperationState::InFlight {
                let failure = self
                    .manual_claim_failures
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .get(request_id)
                    .cloned();
                if self
                    .store
                    .finish_unclaimed_manual_stop(actor, request_id, failure.as_ref())?
                {
                    self.manual_claim_failures
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .remove(request_id);
                    return Ok(self.store.operation(actor, request_id)?.unwrap());
                }
            }
            return Ok(operation);
        }
        let preflight = async {
            if self.stopped.load(Ordering::SeqCst)
                || self.gate.shutting_down()
                || !self.gate.remote_enabled()
                || !self.gate.device_valid(&actor.device_id)
            {
                return Err(ZcodeError::of(codes::NOT_READY));
            }
            let bound = self
                .host
                .read(key, std::time::Duration::from_secs(2))
                .await?;
            let fresh = self
                .host
                .refresh_target(key, std::time::Duration::from_secs(2))
                .await?;
            if fresh != bound.target
                || !bound.view.ready
                || bound.view.control.as_ref().is_none_or(|c| {
                    !c.active_works.iter().any(|w| {
                        w.kind == "primaryTurn"
                            && w.foreground_execution_id.as_deref() == Some(execution)
                    })
                })
            {
                return Err(ZcodeError::of(codes::INVALID_ACTION));
            }
            Ok(bound)
        }
        .await;
        let bound = match preflight {
            Ok(v) => v,
            Err(e) => {
                self.store.finish_manual_stop_with_failure(
                    actor,
                    request_id,
                    QueueOperationState::Failed,
                    None,
                    Some(super::types::StopFailure {
                        phase: "before_write".into(),
                        code: e.code.clone(),
                    }),
                )?;
                return Err(e);
            }
        };
        if tokio::time::Instant::now() >= deadline {
            self.store
                .finish_manual_stop(actor, request_id, QueueOperationState::Failed, None)?;
            return Ok(self.store.operation(actor, request_id)?.unwrap());
        }
        if let Err(error) = self
            .store
            .claim_manual_stop(actor, request_id, bound.target.generation)
        {
            let failure = super::types::StopFailure {
                phase: "before_write".into(),
                code: error.code.clone(),
            };
            self.remember_manual_claim_failure(request_id, failure.clone());
            if self
                .store
                .finish_unclaimed_manual_stop(actor, request_id, Some(&failure))
                .unwrap_or(false)
            {
                self.manual_claim_failures
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(request_id);
            }
            return Err(error);
        }
        if tokio::time::Instant::now() >= deadline {
            self.store
                .finish_manual_stop(actor, request_id, QueueOperationState::Failed, None)?;
            return Ok(self.store.operation(actor, request_id)?.unwrap());
        }
        let ctx = ActionContext {
            action_id,
            device_id: actor.device_id.clone(),
            device_name: actor.device_name.clone(),
        };
        let result = self.host.stop(&ctx, &bound.target, execution).await;
        let (state, receipt, failure) = match result {
            Err(super::super::command::PreparedDispatchError::BeforeWrite(error)) => (
                QueueOperationState::Failed,
                None,
                Some(super::types::StopFailure {
                    phase: "before_write".into(),
                    code: error.code,
                }),
            ),
            Err(super::super::command::PreparedDispatchError::MayHaveWritten(error)) => (
                QueueOperationState::Unknown,
                None,
                Some(super::types::StopFailure {
                    phase: "may_have_written".into(),
                    code: error.code,
                }),
            ),
            Ok(r)
                if r.action_id != ctx.action_id
                    || r.session_id != key.session_id
                    || r.generation != bound.target.generation =>
            {
                (QueueOperationState::Unknown, None, None)
            }
            Ok(r) => {
                let state = match r.state {
                    ActionState::Confirmed => QueueOperationState::Completed,
                    ActionState::Rejected | ActionState::Noop | ActionState::SendFailed => {
                        QueueOperationState::Failed
                    }
                    _ => QueueOperationState::Unknown,
                };
                (state, Some(r), None)
            }
        };
        self.store.finish_manual_stop_with_failure(
            actor,
            request_id,
            state,
            receipt.as_ref(),
            failure,
        )?;
        Ok(self.store.operation(actor, request_id)?.unwrap())
    }
    pub(crate) fn pending_priority_recovery(
        &self,
        actor: &super::types::QueueActor,
        key: &TargetKey,
    ) -> ZResult<Vec<super::types::PriorityRecovery>> {
        Ok(self
            .store
            .pending_priority_recovery()?
            .into_iter()
            .filter(|r| r.device_id == actor.device_id && r.session_id == key.session_id)
            .collect())
    }
    pub(crate) async fn acknowledge_priority_unknown(
        &self,
        actor: &super::types::QueueActor,
        key: &TargetKey,
        request_id: &str,
    ) -> ZResult<()> {
        let lock = self.host.session_lock(key);
        let _guard = lock.lock().await;
        if self.store.priority_key(actor, request_id)?.session_id != key.session_id {
            return Err(super::super::types::ZcodeError::of(
                super::super::types::codes::NO_SESSION,
            ));
        }
        self.store.acknowledge_priority_unknown(
            actor,
            request_id,
            chrono::Utc::now().timestamp_millis(),
        )
    }
    pub(crate) async fn acknowledge_dispatched_unknown(
        &self,
        actor: &super::types::QueueActor,
        key: &TargetKey,
        action_id: &str,
        request_id: &str,
    ) -> ZResult<()> {
        let lock = self.host.session_lock(key);
        let _guard = lock.lock().await;
        self.store.acknowledge_dispatched_unknown(
            actor,
            key,
            action_id,
            request_id,
            chrono::Utc::now().timestamp_millis(),
        )
    }
    pub(crate) async fn acknowledge_manual_stop_unknown(
        &self,
        actor: &super::types::QueueActor,
        key: &TargetKey,
        stop_request: &str,
        review_request: &str,
    ) -> ZResult<()> {
        let lock = self.host.session_lock(key);
        let _guard = lock.lock().await;
        self.store.acknowledge_manual_stop_unknown(
            actor,
            key,
            stop_request,
            review_request,
            chrono::Utc::now().timestamp_millis(),
        )
    }
    pub(crate) async fn acknowledge_unconfirmed(
        &self,
        actor: &super::types::QueueActor,
        key: &TargetKey,
        action_id: &str,
        request_id: &str,
    ) -> ZResult<()> {
        let lock = self.host.session_lock(key);
        let _guard = lock.lock().await;
        self.store.acknowledge_unconfirmed(
            actor,
            key,
            action_id,
            request_id,
            chrono::Utc::now().timestamp_millis(),
        )
    }
    pub(crate) async fn resume(
        &self,
        actor: &super::types::QueueActor,
        key: &TargetKey,
        request_id: &str,
        version: u64,
    ) -> ZResult<super::types::LocalQueueView> {
        let lock = self.host.session_lock(key);
        let _guard = lock.lock().await;
        let view = self.store.resume(actor, key, request_id, version)?;
        self.notify(key);
        Ok(view)
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
    #[cfg(test)]
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
        if !self
            .store
            .attachments_valid(key, None, &bound.target.workspace_path)?
        {
            self.store
                .pause_attachment_missing(key, chrono::Utc::now().timestamp_millis())?;
            return Ok(());
        }
        let Some(claim) =
            self.store
                .claim_next(key, &bound.target, chrono::Utc::now().timestamp_millis())?
        else {
            return Ok(());
        };
        #[cfg(test)]
        let _ = self.queue_events.send(key.session_id.clone());
        self.host.publish_queue_change(&key.session_id);
        let stopping = self.stopped.load(Ordering::SeqCst) || self.gate.shutting_down();
        let remote_enabled = self.gate.remote_enabled();
        let device_valid =
            claim.ctx.device_id == device && self.gate.device_valid(&claim.ctx.device_id);
        let attachment_error = match self.store.attachments_valid(
            key,
            Some(&claim.item_id),
            &claim.target.workspace_path,
        ) {
            Ok(true) => None,
            Ok(false) => Some("attachment_missing".to_owned()),
            Err(error) => Some(error.code),
        };
        let receipt = if !stopping && remote_enabled && device_valid && attachment_error.is_none() {
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
                reason_code: Some(attachment_error.unwrap_or_else(|| {
                    if stopping || !remote_enabled {
                        super::super::types::codes::NOT_READY
                    } else {
                        super::super::types::codes::NO_SESSION
                    }
                    .to_owned()
                })),
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
            ctx: &'a ActionContext,
            target: &'a BoundTarget,
            execution: &'a str,
        ) -> BoxFuture<'a, super::super::super::command::PreparedResult<ActionReceipt>> {
            Box::pin(async move {
                assert_eq!(execution, "A");
                assert!(*self.busy.lock().unwrap());
                *self.busy.lock().unwrap() = false;
                let receipt = ActionReceipt {
                    action_id: ctx.action_id.clone(),
                    session_id: target.session_id.clone(),
                    generation: target.generation,
                    state: ActionState::Confirmed,
                    reason_code: None,
                    send_admission: None,
                };
                self.receipts
                    .lock()
                    .unwrap()
                    .insert("stopped:A".into(), receipt.clone());
                Ok(receipt)
            })
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
            ) -> BoxFuture<'a, super::super::super::command::PreparedResult<ActionReceipt>>
            {
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
            ) -> BoxFuture<'a, super::super::super::command::PreparedResult<ActionReceipt>>
            {
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
    async fn resume_and_prioritize_before_unconfirmed_review_are_rejected_without_side_effects() {
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
        let svc = Arc::new(ZcodeQueueService::new(
            store.clone(),
            fake.clone(),
            Arc::new(Gate),
        ));
        let actor = QueueActor {
            device_id: "d".into(),
            device_name: "phone".into(),
        };
        let key = TargetKey {
            session_id: "session".into(),
        };
        for text in ["A", "B"] {
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
        // 重启恢复：A accepted/startNow 无终态，B queued。
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
                    input_id: "A-input".into(),
                }),
            },
        );
        svc.recover().await.unwrap();
        let before = store.view(&actor, &key).unwrap();
        assert!(!before.auto_drain);
        assert_eq!(before.pause_reason.as_deref(), Some("unconfirmed"));

        let error = svc
            .resume(&actor, &key, "resume-before-review", before.version)
            .await
            .err()
            .unwrap();
        assert_eq!(error.code, codes::QUEUE_REVIEW_REQUIRED);
        let after = store.view(&actor, &key).unwrap();
        assert_eq!(after.version, before.version);
        assert!(!after.auto_drain);

        let b = after
            .items
            .iter()
            .find(|item| item.text_preview == "B")
            .unwrap();
        let error = svc
            .prioritize(
                &actor,
                super::super::types::PriorityCommand {
                    request_id: "priority-before-review".into(),
                    item_id: b.item_id.clone(),
                    expected_version: b.item_version,
                    confirm_stop: false,
                    expected_execution_id: None,
                    held_queue: None,
                },
            )
            .await
            .err()
            .unwrap();
        assert_eq!(error.code, codes::QUEUE_REVIEW_REQUIRED);
        assert!(fake.sent.lock().unwrap().is_empty());
        assert!(!fake.receipts.lock().unwrap().contains_key("stopped:A"));
        // 新 priority 操作已落账但被明确判失败；关键是零 stop / 零 send，绝不静默成功。
        assert_eq!(
            store
                .operation(&actor, "priority-before-review")
                .unwrap()
                .unwrap()
                .state,
            super::super::types::QueueOperationState::Failed
        );

        // 核对只清屏障、不重发；确认并继续只发 B 一次，重复请求幂等。
        svc.acknowledge_unconfirmed(&actor, &key, &claim.ctx.action_id, "review-ack")
            .await
            .unwrap();
        let reviewed = store.view(&actor, &key).unwrap();
        assert!(!reviewed.auto_drain);
        let resumed = svc
            .resume(&actor, &key, "continue", reviewed.version)
            .await
            .unwrap();
        assert!(resumed.auto_drain);
        assert!(
            svc.resume(&actor, &key, "continue", reviewed.version)
                .await
                .unwrap()
                .auto_drain
        );
        svc.flush_one(&key).await.unwrap();
        svc.flush_one(&key).await.unwrap();
        assert_eq!(*fake.sent.lock().unwrap(), vec!["B"]);
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
    struct FailedPriorityStop {
        inner: Arc<Fake>,
        written: bool,
        code: &'static str,
        calls: std::sync::atomic::AtomicUsize,
    }
    impl QueueHostPort for FailedPriorityStop {
        fn session_lock(&self, key: &TargetKey) -> Arc<tokio::sync::Mutex<()>> {
            self.inner.session_lock(key)
        }
        fn read<'a>(
            &'a self,
            key: &'a TargetKey,
            timeout: Duration,
        ) -> BoxFuture<'a, ZResult<BoundQueueSession>> {
            self.inner.read(key, timeout)
        }
        fn refresh_target<'a>(
            &'a self,
            key: &'a TargetKey,
            timeout: Duration,
        ) -> BoxFuture<'a, ZResult<BoundTarget>> {
            self.inner.refresh_target(key, timeout)
        }
        fn send_prepared<'a>(
            &'a self,
            ctx: &'a ActionContext,
            target: &'a BoundTarget,
            body: &'a PreparedSend,
            held: Option<&'a HeldQueueChoice>,
        ) -> BoxFuture<'a, super::super::super::command::PreparedResult<ActionReceipt>> {
            self.inner.send_prepared(ctx, target, body, held)
        }
        fn stop<'a>(
            &'a self,
            _ctx: &'a ActionContext,
            _target: &'a BoundTarget,
            _execution: &'a str,
        ) -> BoxFuture<'a, super::super::super::command::PreparedResult<ActionReceipt>> {
            Box::pin(async move {
                self.calls.fetch_add(1, Ordering::SeqCst);
                let e = ZcodeError::of(self.code);
                Err(if self.written {
                    super::super::super::command::PreparedDispatchError::MayHaveWritten(e)
                } else {
                    super::super::super::command::PreparedDispatchError::BeforeWrite(e)
                })
            })
        }
        fn receipt<'a>(
            &'a self,
            action: &'a str,
            device: &'a str,
        ) -> BoxFuture<'a, ZResult<Option<ActionReceipt>>> {
            self.inner.receipt(action, device)
        }
    }
    struct BlockingPriorityStop {
        inner: Arc<Fake>,
        entered: Notify,
        release: Notify,
        calls: std::sync::atomic::AtomicUsize,
    }
    impl QueueHostPort for BlockingPriorityStop {
        fn session_lock(&self, key: &TargetKey) -> Arc<tokio::sync::Mutex<()>> {
            self.inner.session_lock(key)
        }
        fn read<'a>(
            &'a self,
            key: &'a TargetKey,
            timeout: Duration,
        ) -> BoxFuture<'a, ZResult<BoundQueueSession>> {
            self.inner.read(key, timeout)
        }
        fn refresh_target<'a>(
            &'a self,
            key: &'a TargetKey,
            timeout: Duration,
        ) -> BoxFuture<'a, ZResult<BoundTarget>> {
            self.inner.refresh_target(key, timeout)
        }
        fn send_prepared<'a>(
            &'a self,
            ctx: &'a ActionContext,
            target: &'a BoundTarget,
            body: &'a PreparedSend,
            held: Option<&'a HeldQueueChoice>,
        ) -> BoxFuture<'a, super::super::super::command::PreparedResult<ActionReceipt>> {
            self.inner.send_prepared(ctx, target, body, held)
        }
        fn stop<'a>(
            &'a self,
            ctx: &'a ActionContext,
            target: &'a BoundTarget,
            execution: &'a str,
        ) -> BoxFuture<'a, super::super::super::command::PreparedResult<ActionReceipt>> {
            Box::pin(async move {
                self.calls.fetch_add(1, Ordering::SeqCst);
                self.entered.notify_one();
                self.release.notified().await;
                self.inner.stop(ctx, target, execution).await
            })
        }
        fn receipt<'a>(
            &'a self,
            action: &'a str,
            device: &'a str,
        ) -> BoxFuture<'a, ZResult<Option<ActionReceipt>>> {
            self.inner.receipt(action, device)
        }
    }
    #[tokio::test]
    async fn cancelled_prioritize_does_not_repeat_stop() {
        let fake = Arc::new(Fake {
            sent: Mutex::new(vec![]),
            busy: Mutex::new(true),
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
        let host = Arc::new(BlockingPriorityStop {
            inner: fake.clone(),
            entered: Notify::new(),
            release: Notify::new(),
            calls: std::sync::atomic::AtomicUsize::new(0),
        });
        let store = SqlLocalQueueStore::new(DeviceStore::memory());
        let svc = Arc::new(ZcodeQueueService::new(
            store.clone(),
            host.clone(),
            Arc::new(Gate),
        ));
        let actor = QueueActor {
            device_id: "d".into(),
            device_name: "phone".into(),
        };
        let key = TargetKey {
            session_id: "session".into(),
        };
        let item = svc
            .enqueue(EnqueueCommand {
                request_id: "C".into(),
                actor: actor.clone(),
                session_id: key.session_id,
                workspace_path: "/synthetic".into(),
                workspace_identity: None,
                draft: QueueDraft {
                    text: "C".into(),
                    attachments: vec![],
                },
                prepared: PreparedSend::from_stored("C".into()).unwrap(),
                edited_from: None,
            })
            .await
            .unwrap();
        let cmd = super::super::types::PriorityCommand {
            request_id: "p".into(),
            item_id: item.item_id,
            expected_version: item.item_version,
            confirm_stop: true,
            expected_execution_id: Some("A".into()),
            held_queue: None,
        };
        let worker = svc.clone();
        let a = actor.clone();
        let c = cmd.clone();
        let task = tokio::spawn(async move { worker.prioritize(&a, c).await });
        host.entered.notified().await;
        task.abort();
        host.release.notify_one();
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if !fake.sent.lock().unwrap().is_empty() {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("后台编排应完成");
        svc.prioritize(&actor, cmd).await.unwrap();
        assert_eq!(host.calls.load(Ordering::SeqCst), 1);
        assert_eq!(*fake.sent.lock().unwrap(), vec!["C"]);
    }
    #[tokio::test]
    async fn stop_before_write_releases_queued_item_without_send() {
        let fake = Arc::new(Fake {
            sent: Mutex::new(vec![]),
            busy: Mutex::new(true),
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
        let host = Arc::new(FailedPriorityStop {
            inner: fake.clone(),
            written: false,
            code: codes::NO_SESSION,
            calls: std::sync::atomic::AtomicUsize::new(0),
        });
        let store = SqlLocalQueueStore::new(DeviceStore::memory());
        let svc = Arc::new(ZcodeQueueService::new(
            store.clone(),
            host.clone(),
            Arc::new(Gate),
        ));
        let actor = QueueActor {
            device_id: "d".into(),
            device_name: "phone".into(),
        };
        let key = TargetKey {
            session_id: "session".into(),
        };
        let item = svc
            .enqueue(EnqueueCommand {
                request_id: "C".into(),
                actor: actor.clone(),
                session_id: key.session_id.clone(),
                workspace_path: "/synthetic".into(),
                workspace_identity: None,
                draft: QueueDraft {
                    text: "C".into(),
                    attachments: vec![],
                },
                prepared: PreparedSend::from_stored("C".into()).unwrap(),
                edited_from: None,
            })
            .await
            .unwrap();
        let result = svc
            .prioritize(
                &actor,
                super::super::types::PriorityCommand {
                    request_id: "p".into(),
                    item_id: item.item_id,
                    expected_version: item.item_version,
                    confirm_stop: true,
                    expected_execution_id: Some("A".into()),
                    held_queue: None,
                },
            )
            .await
            .unwrap();
        assert_eq!(
            result.state,
            super::super::types::QueueOperationState::Failed
        );
        let view = store.view(&actor, &key).unwrap();
        assert!(!view.auto_drain);
        assert_eq!(
            view.items[0].state,
            super::super::types::QueueItemState::Queued
        );
        assert!(fake.sent.lock().unwrap().is_empty());
        assert_eq!(host.calls.load(Ordering::SeqCst), 1);
    }
    #[tokio::test]
    async fn stop_unknown_is_zero_send_even_after_restart() {
        let fake = Arc::new(Fake {
            sent: Mutex::new(vec![]),
            busy: Mutex::new(true),
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
        let host = Arc::new(FailedPriorityStop {
            inner: fake.clone(),
            written: true,
            code: codes::NO_SESSION,
            calls: std::sync::atomic::AtomicUsize::new(0),
        });
        let store = SqlLocalQueueStore::new(DeviceStore::memory());
        let svc = Arc::new(ZcodeQueueService::new(
            store.clone(),
            host.clone(),
            Arc::new(Gate),
        ));
        let actor = QueueActor {
            device_id: "d".into(),
            device_name: "phone".into(),
        };
        let key = TargetKey {
            session_id: "session".into(),
        };
        let item = svc
            .enqueue(EnqueueCommand {
                request_id: "C".into(),
                actor: actor.clone(),
                session_id: key.session_id.clone(),
                workspace_path: "/synthetic".into(),
                workspace_identity: None,
                draft: QueueDraft {
                    text: "C".into(),
                    attachments: vec![],
                },
                prepared: PreparedSend::from_stored("C".into()).unwrap(),
                edited_from: None,
            })
            .await
            .unwrap();
        let result = svc
            .prioritize(
                &actor,
                super::super::types::PriorityCommand {
                    request_id: "p".into(),
                    item_id: item.item_id.clone(),
                    expected_version: item.item_version,
                    confirm_stop: true,
                    expected_execution_id: Some("A".into()),
                    held_queue: None,
                },
            )
            .await
            .unwrap();
        assert_eq!(
            result.state,
            super::super::types::QueueOperationState::Unknown
        );
        store
            .recover(chrono::Utc::now().timestamp_millis())
            .unwrap();
        let recovery = store.pending_priority_recovery().unwrap();
        assert_eq!(recovery.len(), 1);
        assert_eq!(recovery[0].execution_id, "A");
        assert!(svc
            .prioritize(
                &actor,
                super::super::types::PriorityCommand {
                    request_id: "new".into(),
                    item_id: item.item_id.clone(),
                    expected_version: item.item_version,
                    confirm_stop: true,
                    expected_execution_id: Some("A".into()),
                    held_queue: None
                }
            )
            .await
            .is_err());
        let view = store.view(&actor, &key).unwrap();
        assert!(!view.auto_drain);
        assert_eq!(
            view.items[0].state,
            super::super::types::QueueItemState::Queued
        );
        assert!(fake.sent.lock().unwrap().is_empty());
        assert_eq!(host.calls.load(Ordering::SeqCst), 1);
        let other_key = TargetKey {
            session_id: "other".into(),
        };
        assert!(svc
            .acknowledge_priority_unknown(&actor, &other_key, "p")
            .await
            .is_err());
        assert_eq!(
            svc.pending_priority_recovery(&actor, &key).unwrap().len(),
            1
        );
        svc.acknowledge_priority_unknown(&actor, &key, "p")
            .await
            .unwrap();
        let reviewed = store.view(&actor, &key).unwrap();
        assert!(!reviewed.auto_drain);
        *fake.busy.lock().unwrap() = false;
        svc.flush_one(&key).await.unwrap();
        assert!(fake.sent.lock().unwrap().is_empty());
        svc.resume(&actor, &key, "resume-after-review", reviewed.version)
            .await
            .unwrap();
        svc.flush_one(&key).await.unwrap();
        assert_eq!(*fake.sent.lock().unwrap(), vec!["C"]);
        assert_eq!(host.calls.load(Ordering::SeqCst), 1);
    }
    struct NewExecutionStop {
        inner: Arc<Fake>,
        switched: AtomicBool,
        calls: std::sync::atomic::AtomicUsize,
    }
    impl QueueHostPort for NewExecutionStop {
        fn session_lock(&self, key: &TargetKey) -> Arc<tokio::sync::Mutex<()>> {
            self.inner.session_lock(key)
        }
        fn read<'a>(
            &'a self,
            key: &'a TargetKey,
            timeout: Duration,
        ) -> BoxFuture<'a, ZResult<BoundQueueSession>> {
            Box::pin(async move {
                let mut b = self.inner.read(key, timeout).await?;
                if self.switched.load(Ordering::SeqCst) {
                    b.view.control.as_mut().unwrap().active_works[0].foreground_execution_id =
                        Some("new-execution".into());
                }
                Ok(b)
            })
        }
        fn refresh_target<'a>(
            &'a self,
            key: &'a TargetKey,
            timeout: Duration,
        ) -> BoxFuture<'a, ZResult<BoundTarget>> {
            self.inner.refresh_target(key, timeout)
        }
        fn send_prepared<'a>(
            &'a self,
            ctx: &'a ActionContext,
            target: &'a BoundTarget,
            body: &'a PreparedSend,
            held: Option<&'a HeldQueueChoice>,
        ) -> BoxFuture<'a, super::super::super::command::PreparedResult<ActionReceipt>> {
            self.inner.send_prepared(ctx, target, body, held)
        }
        fn stop<'a>(
            &'a self,
            ctx: &'a ActionContext,
            target: &'a BoundTarget,
            execution: &'a str,
        ) -> BoxFuture<'a, super::super::super::command::PreparedResult<ActionReceipt>> {
            Box::pin(async move {
                self.calls.fetch_add(1, Ordering::SeqCst);
                let result = self.inner.stop(ctx, target, execution).await;
                *self.inner.busy.lock().unwrap() = true;
                self.switched.store(true, Ordering::SeqCst);
                result
            })
        }
        fn receipt<'a>(
            &'a self,
            action: &'a str,
            device: &'a str,
        ) -> BoxFuture<'a, ZResult<Option<ActionReceipt>>> {
            self.inner.receipt(action, device)
        }
    }
    struct PauseInspectStop {
        inner: Arc<Fake>,
        store: SqlLocalQueueStore,
        actor: QueueActor,
        key: TargetKey,
    }
    impl QueueHostPort for PauseInspectStop {
        fn session_lock(&self, key: &TargetKey) -> Arc<tokio::sync::Mutex<()>> {
            self.inner.session_lock(key)
        }
        fn read<'a>(
            &'a self,
            key: &'a TargetKey,
            timeout: Duration,
        ) -> BoxFuture<'a, ZResult<BoundQueueSession>> {
            self.inner.read(key, timeout)
        }
        fn refresh_target<'a>(
            &'a self,
            key: &'a TargetKey,
            timeout: Duration,
        ) -> BoxFuture<'a, ZResult<BoundTarget>> {
            self.inner.refresh_target(key, timeout)
        }
        fn send_prepared<'a>(
            &'a self,
            ctx: &'a ActionContext,
            target: &'a BoundTarget,
            body: &'a PreparedSend,
            held: Option<&'a HeldQueueChoice>,
        ) -> BoxFuture<'a, super::super::super::command::PreparedResult<ActionReceipt>> {
            self.inner.send_prepared(ctx, target, body, held)
        }
        fn stop<'a>(
            &'a self,
            ctx: &'a ActionContext,
            target: &'a BoundTarget,
            execution: &'a str,
        ) -> BoxFuture<'a, super::super::super::command::PreparedResult<ActionReceipt>> {
            Box::pin(async move {
                assert!(!self.store.view(&self.actor, &self.key).unwrap().auto_drain);
                self.inner.stop(ctx, target, execution).await
            })
        }
        fn receipt<'a>(
            &'a self,
            action: &'a str,
            device: &'a str,
        ) -> BoxFuture<'a, ZResult<Option<ActionReceipt>>> {
            self.inner.receipt(action, device)
        }
    }
    #[tokio::test]
    async fn resume_uses_queue_version_and_preserves_fifo() {
        let fake = Arc::new(Fake {
            sent: Mutex::new(vec![]),
            busy: Mutex::new(true),
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
        let actor = QueueActor {
            device_id: "d".into(),
            device_name: "phone".into(),
        };
        let key = TargetKey {
            session_id: "session".into(),
        };
        let svc = Arc::new(ZcodeQueueService::new(
            store.clone(),
            fake.clone(),
            Arc::new(Gate),
        ));
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
        svc.stop_current(&actor, &key, "stop-request", "A")
            .await
            .unwrap();
        let paused = store.view(&actor, &key).unwrap();
        assert!(svc
            .resume(&actor, &key, "stale", paused.version - 1)
            .await
            .is_err());
        let resumed = svc
            .resume(&actor, &key, "resume", paused.version)
            .await
            .unwrap();
        assert!(resumed.auto_drain);
        svc.flush_one(&key).await.unwrap();
        assert_eq!(*fake.sent.lock().unwrap(), vec!["B"]);
        assert_eq!(store.view(&actor, &key).unwrap().items[0].text_preview, "C");
    }
    #[tokio::test]
    async fn manual_stop_pauses_before_fifo_can_flush() {
        let fake = Arc::new(Fake {
            sent: Mutex::new(vec![]),
            busy: Mutex::new(true),
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
        let actor = QueueActor {
            device_id: "d".into(),
            device_name: "phone".into(),
        };
        let key = TargetKey {
            session_id: "session".into(),
        };
        let host = Arc::new(PauseInspectStop {
            inner: fake.clone(),
            store: store.clone(),
            actor: actor.clone(),
            key: key.clone(),
        });
        let svc = Arc::new(ZcodeQueueService::new(store.clone(), host, Arc::new(Gate)));
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
        svc.stop_current(&actor, &key, "stop-request", "A")
            .await
            .unwrap();
        svc.flush_one(&key).await.unwrap();
        assert!(fake.receipts.lock().unwrap().contains_key("stopped:A"));
        assert!(fake.sent.lock().unwrap().is_empty());
        assert!(!store.view(&actor, &key).unwrap().auto_drain);
    }
    #[tokio::test]
    async fn manual_unknown_survives_recovery_and_blocks_new_stop_and_flush() {
        let fake = Arc::new(Fake {
            sent: Mutex::new(vec![]),
            busy: Mutex::new(true),
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
        let host = Arc::new(FailedPriorityStop {
            inner: fake.clone(),
            written: true,
            code: codes::NO_SESSION,
            calls: std::sync::atomic::AtomicUsize::new(0),
        });
        let store = SqlLocalQueueStore::new(DeviceStore::memory());
        let svc = Arc::new(ZcodeQueueService::new(
            store.clone(),
            host.clone(),
            Arc::new(Gate),
        ));
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
        let result = svc.stop_current(&actor, &key, "stop-1", "A").await.unwrap();
        assert_eq!(
            result.state,
            super::super::types::QueueOperationState::Unknown
        );
        store
            .recover(chrono::Utc::now().timestamp_millis())
            .unwrap();
        let view = store.view(&actor, &key).unwrap();
        assert_eq!(
            svc.resume(&actor, &key, "resume", view.version)
                .await
                .err()
                .unwrap()
                .code,
            codes::QUEUE_REVIEW_REQUIRED
        );
        *fake.busy.lock().unwrap() = false;
        svc.flush_one(&key).await.unwrap();
        assert!(fake.sent.lock().unwrap().is_empty());
        assert!(svc.stop_current(&actor, &key, "stop-2", "A").await.is_err());
        assert_eq!(host.calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            store
                .unreviewed_manual_stop(&actor, &key)
                .unwrap()
                .as_deref(),
            Some("stop-1")
        );
        svc.acknowledge_manual_stop_unknown(&actor, &key, "stop-1", "review")
            .await
            .unwrap();
        let reviewed = store.view(&actor, &key).unwrap();
        assert!(!reviewed.auto_drain);
        svc.flush_one(&key).await.unwrap();
        assert!(fake.sent.lock().unwrap().is_empty());
        svc.resume(&actor, &key, "resume-after-review", reviewed.version)
            .await
            .unwrap();
        svc.flush_one(&key).await.unwrap();
        assert_eq!(*fake.sent.lock().unwrap(), vec!["B"]);
        assert!(store
            .unreviewed_manual_stop(&actor, &key)
            .unwrap()
            .is_none());
    }
    #[tokio::test]
    async fn expired_priority_does_not_begin_any_new_write() {
        let fake = Arc::new(Fake {
            sent: Mutex::new(vec![]),
            busy: Mutex::new(true),
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
        let item = svc
            .enqueue(EnqueueCommand {
                request_id: "C".into(),
                actor: actor.clone(),
                session_id: key.session_id,
                workspace_path: "/synthetic".into(),
                workspace_identity: None,
                draft: QueueDraft {
                    text: "C".into(),
                    attachments: vec![],
                },
                prepared: PreparedSend::from_stored("C".into()).unwrap(),
                edited_from: None,
            })
            .await
            .unwrap();
        let cmd = super::super::types::PriorityCommand {
            request_id: "p".into(),
            item_id: item.item_id,
            expected_version: item.item_version,
            confirm_stop: true,
            expected_execution_id: Some("A".into()),
            held_queue: None,
        };
        assert!(svc
            .prioritize_inner(
                &actor,
                cmd,
                tokio::time::Instant::now() - Duration::from_millis(1)
            )
            .await
            .is_err());
        assert!(store.operation(&actor, "p").unwrap().is_none());
        assert!(fake.sent.lock().unwrap().is_empty());
        assert!(!fake.receipts.lock().unwrap().contains_key("stopped:A"));
    }
    #[tokio::test]
    async fn deadline_after_stop_claim_emits_zero_stop() {
        let fake = Arc::new(Fake {
            sent: Mutex::new(vec![]),
            busy: Mutex::new(true),
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
        *svc.before_priority_port.lock().unwrap() = Some(Arc::new(|stage| {
            if stage == "stop" {
                std::thread::sleep(Duration::from_millis(650));
            }
        }));
        let actor = QueueActor {
            device_id: "d".into(),
            device_name: "phone".into(),
        };
        let key = TargetKey {
            session_id: "session".into(),
        };
        let item = svc
            .enqueue(EnqueueCommand {
                request_id: "C".into(),
                actor: actor.clone(),
                session_id: key.session_id.clone(),
                workspace_path: "/synthetic".into(),
                workspace_identity: None,
                draft: QueueDraft {
                    text: "C".into(),
                    attachments: vec![],
                },
                prepared: PreparedSend::from_stored("C".into()).unwrap(),
                edited_from: None,
            })
            .await
            .unwrap();
        let cmd = super::super::types::PriorityCommand {
            request_id: "p".into(),
            item_id: item.item_id,
            expected_version: item.item_version,
            confirm_stop: true,
            expected_execution_id: Some("A".into()),
            held_queue: None,
        };
        let result = svc
            .prioritize_inner(
                &actor,
                cmd,
                tokio::time::Instant::now() + Duration::from_millis(500),
            )
            .await
            .unwrap();
        assert_eq!(
            result.state,
            super::super::types::QueueOperationState::Failed
        );
        let view = store.view(&actor, &key).unwrap();
        assert!(!view.auto_drain);
        assert_eq!(
            view.items[0].state,
            super::super::types::QueueItemState::Queued
        );
        assert!(fake.sent.lock().unwrap().is_empty());
        assert!(!fake.receipts.lock().unwrap().contains_key("stopped:A"));
    }
    #[tokio::test]
    async fn deadline_after_send_claim_emits_zero_message() {
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
        *svc.before_priority_port.lock().unwrap() = Some(Arc::new(|stage| {
            if stage == "send" {
                std::thread::sleep(Duration::from_millis(650));
            }
        }));
        let actor = QueueActor {
            device_id: "d".into(),
            device_name: "phone".into(),
        };
        let key = TargetKey {
            session_id: "session".into(),
        };
        let item = svc
            .enqueue(EnqueueCommand {
                request_id: "C".into(),
                actor: actor.clone(),
                session_id: key.session_id.clone(),
                workspace_path: "/synthetic".into(),
                workspace_identity: None,
                draft: QueueDraft {
                    text: "C".into(),
                    attachments: vec![],
                },
                prepared: PreparedSend::from_stored("C".into()).unwrap(),
                edited_from: None,
            })
            .await
            .unwrap();
        let cmd = super::super::types::PriorityCommand {
            request_id: "p".into(),
            item_id: item.item_id,
            expected_version: item.item_version,
            confirm_stop: true,
            expected_execution_id: Some("A".into()),
            held_queue: None,
        };
        let result = svc
            .prioritize_inner(
                &actor,
                cmd,
                tokio::time::Instant::now() + Duration::from_millis(500),
            )
            .await
            .unwrap();
        assert_eq!(
            result.state,
            super::super::types::QueueOperationState::Failed
        );
        let view = store.view(&actor, &key).unwrap();
        assert!(!view.auto_drain);
        assert_eq!(
            view.items[0].state,
            super::super::types::QueueItemState::Queued
        );
        assert!(fake.sent.lock().unwrap().is_empty());
        assert!(!fake.receipts.lock().unwrap().contains_key("stopped:A"));
    }
    #[tokio::test]
    async fn priority_waits_for_matching_terminal_and_leaves_queue_paused() {
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
        let svc = Arc::new(ZcodeQueueService::new(
            store.clone(),
            fake.clone(),
            Arc::new(Gate),
        ));
        let actor = QueueActor {
            device_id: "d".into(),
            device_name: "phone".into(),
        };
        let key = TargetKey {
            session_id: "session".into(),
        };
        let mut items = Vec::new();
        for text in ["B", "C"] {
            items.push(
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
                .unwrap(),
            );
        }
        svc.flush_one(&key).await.unwrap();
        fake.revision.store(2, Ordering::SeqCst);
        let mut wrong = fake.read(&key, Duration::ZERO).await.unwrap().view;
        wrong.execution_terminal = Some(ExecutionTerminal {
            input_id: "other-input".into(),
            result_type: "success".into(),
            revision: 2,
        });
        svc.on_session(&wrong);
        let cmd = |id: &str| super::super::types::PriorityCommand {
            request_id: id.into(),
            item_id: items[1].item_id.clone(),
            expected_version: items[1].item_version,
            confirm_stop: false,
            expected_execution_id: None,
            held_queue: None,
        };
        let result = svc.prioritize(&actor, cmd("wrong-terminal")).await;
        assert!(
            result.is_err()
                || result.unwrap().state == super::super::types::QueueOperationState::Failed
        );
        assert_eq!(*fake.sent.lock().unwrap(), vec!["B"]);
        fake.terminal_after_send.store(true, Ordering::SeqCst);
        svc.prioritize(&actor, cmd("correct-terminal"))
            .await
            .unwrap();
        assert_eq!(*fake.sent.lock().unwrap(), vec!["B", "C"]);
        assert!(!store.view(&actor, &key).unwrap().auto_drain);
    }
    #[tokio::test]
    async fn dispatched_unknown_requires_review_then_resume_without_resend() {
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
            send_failure: Mutex::new(Some((true, codes::NOT_READY))),
        });
        let db = DeviceStore::memory();
        let store = SqlLocalQueueStore::new(db.clone());
        let svc = Arc::new(ZcodeQueueService::new(
            store.clone(),
            fake.clone(),
            Arc::new(Gate),
        ));
        let actor = QueueActor {
            device_id: "d".into(),
            device_name: "phone".into(),
        };
        let key = TargetKey {
            session_id: "session".into(),
        };
        let mut items = Vec::new();
        for text in ["B", "C"] {
            items.push(
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
                .unwrap(),
            );
        }
        svc.flush_one(&key).await.unwrap();
        let action: String = db.with(|c| {
            c.query_row(
                "SELECT dispatch_action_id FROM zcode_send_queue WHERE request_id='B'",
                [],
                |r| r.get(0),
            )
            .unwrap()
        });
        let paused = store.view(&actor, &key).unwrap();
        assert_eq!(
            svc.resume(&actor, &key, "early", paused.version)
                .await
                .err()
                .unwrap()
                .code,
            codes::QUEUE_REVIEW_REQUIRED
        );
        svc.flush_one(&key).await.unwrap();
        assert_eq!(*fake.sent.lock().unwrap(), vec!["B"]);
        assert!(svc
            .prioritize(
                &actor,
                super::super::types::PriorityCommand {
                    request_id: "p".into(),
                    item_id: items[1].item_id.clone(),
                    expected_version: items[1].item_version,
                    confirm_stop: false,
                    expected_execution_id: None,
                    held_queue: None
                }
            )
            .await
            .is_err());
        let foreign = QueueActor {
            device_id: "other".into(),
            device_name: "other".into(),
        };
        assert!(svc
            .acknowledge_dispatched_unknown(&foreign, &key, &action, "review-foreign")
            .await
            .is_err());
        assert_eq!(
            store
                .unreviewed_dispatch_unknown(&actor, &key)
                .unwrap()
                .as_deref(),
            Some(action.as_str())
        );
        db.with(|c| c.execute_batch("CREATE TRIGGER fail_unknown_review BEFORE INSERT ON write_audit WHEN NEW.action='zcode_queue_unknown_review' BEGIN SELECT RAISE(ABORT,'synthetic'); END;").unwrap());
        assert!(svc
            .acknowledge_dispatched_unknown(&actor, &key, &action, "review")
            .await
            .is_err());
        assert_eq!(
            store
                .unreviewed_dispatch_unknown(&actor, &key)
                .unwrap()
                .as_deref(),
            Some(action.as_str())
        );
        db.with(|c| c.execute_batch("DROP TRIGGER fail_unknown_review").unwrap());
        svc.acknowledge_dispatched_unknown(&actor, &key, &action, "review")
            .await
            .unwrap();
        let paused = store.view(&actor, &key).unwrap();
        assert!(!paused.auto_drain);
        svc.flush_one(&key).await.unwrap();
        assert_eq!(*fake.sent.lock().unwrap(), vec!["B"]);
        svc.resume(&actor, &key, "resume", paused.version)
            .await
            .unwrap();
        svc.acknowledge_dispatched_unknown(&actor, &key, &action, "review")
            .await
            .unwrap();
        assert!(store.view(&actor, &key).unwrap().auto_drain);
        svc.flush_one(&key).await.unwrap();
        assert_eq!(*fake.sent.lock().unwrap(), vec!["B", "C"]);
        let state: String = db.with(|c| {
            c.query_row(
                "SELECT state FROM zcode_send_queue WHERE request_id='B'",
                [],
                |r| r.get(0),
            )
            .unwrap()
        });
        assert_eq!(state, "unknown");
    }
    #[tokio::test]
    async fn priority_unknown_review_unblocks_next_after_resume() {
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
            send_failure: Mutex::new(Some((true, codes::NOT_READY))),
        });
        let db = DeviceStore::memory();
        let store = SqlLocalQueueStore::new(db.clone());
        let svc = Arc::new(ZcodeQueueService::new(
            store.clone(),
            fake.clone(),
            Arc::new(Gate),
        ));
        let actor = QueueActor {
            device_id: "d".into(),
            device_name: "phone".into(),
        };
        let key = TargetKey {
            session_id: "session".into(),
        };
        let mut items = Vec::new();
        for text in ["B", "C"] {
            items.push(
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
                .unwrap(),
            );
        }
        let result = svc
            .prioritize(
                &actor,
                super::super::types::PriorityCommand {
                    request_id: "priority".into(),
                    item_id: items[0].item_id.clone(),
                    expected_version: items[0].item_version,
                    confirm_stop: false,
                    expected_execution_id: None,
                    held_queue: None,
                },
            )
            .await
            .unwrap();
        assert_eq!(
            result.state,
            super::super::types::QueueOperationState::Unknown
        );
        let action: String = db.with(|c| {
            c.query_row(
                "SELECT dispatch_action_id FROM zcode_send_queue WHERE request_id='B'",
                [],
                |r| r.get(0),
            )
            .unwrap()
        });
        svc.acknowledge_dispatched_unknown(&actor, &key, &action, "review")
            .await
            .unwrap();
        let paused = store.view(&actor, &key).unwrap();
        assert!(!paused.auto_drain);
        svc.resume(&actor, &key, "resume", paused.version)
            .await
            .unwrap();
        svc.flush_one(&key).await.unwrap();
        assert_eq!(*fake.sent.lock().unwrap(), vec!["B", "C"]);
        assert_eq!(
            store.operation(&actor, "priority").unwrap().unwrap().state,
            super::super::types::QueueOperationState::Unknown
        );
    }
    #[tokio::test]
    async fn priority_and_auto_flush_have_one_winner() {
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
        let svc = Arc::new(ZcodeQueueService::new(
            store.clone(),
            fake.clone(),
            Arc::new(Gate),
        ));
        let actor = QueueActor {
            device_id: "d".into(),
            device_name: "phone".into(),
        };
        let key = TargetKey {
            session_id: "session".into(),
        };
        let mut items = Vec::new();
        for text in ["B", "C"] {
            items.push(
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
                .unwrap(),
            );
        }
        let lock = fake.lock.clone();
        let guard = lock.lock().await;
        let flush_svc = svc.clone();
        let flush_key = key.clone();
        let flush = tokio::spawn(async move { flush_svc.flush_one(&flush_key).await });
        let priority_svc = svc.clone();
        let priority_actor = actor.clone();
        let c_id = items[1].item_id.clone();
        let c_version = items[1].item_version;
        let priority = tokio::spawn(async move {
            priority_svc
                .prioritize(
                    &priority_actor,
                    super::super::types::PriorityCommand {
                        request_id: "priority".into(),
                        item_id: c_id,
                        expected_version: c_version,
                        confirm_stop: false,
                        expected_execution_id: None,
                        held_queue: None,
                    },
                )
                .await
        });
        tokio::task::yield_now().await;
        drop(guard);
        let _ = flush.await.unwrap();
        let _ = priority.await.unwrap();
        assert_eq!(fake.sent.lock().unwrap().len(), 1);
        assert!(!store.view(&actor, &key).unwrap().auto_drain);
        assert!(!fake.receipts.lock().unwrap().contains_key("stopped:A"));
    }
    #[tokio::test]
    async fn fifo_accepted_without_terminal_blocks_priority() {
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
        let svc = Arc::new(ZcodeQueueService::new(
            store.clone(),
            fake.clone(),
            Arc::new(Gate),
        ));
        let actor = QueueActor {
            device_id: "d".into(),
            device_name: "phone".into(),
        };
        let key = TargetKey {
            session_id: "session".into(),
        };
        let mut items = Vec::new();
        for text in ["B", "C"] {
            items.push(
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
                .unwrap(),
            );
        }
        svc.flush_one(&key).await.unwrap();
        assert_eq!(*fake.sent.lock().unwrap(), vec!["B"]);
        let result = svc
            .prioritize(
                &actor,
                super::super::types::PriorityCommand {
                    request_id: "p".into(),
                    item_id: items[1].item_id.clone(),
                    expected_version: items[1].item_version,
                    confirm_stop: false,
                    expected_execution_id: None,
                    held_queue: None,
                },
            )
            .await;
        assert!(
            result.is_err()
                || result.unwrap().state == super::super::types::QueueOperationState::Failed
        );
        assert_eq!(*fake.sent.lock().unwrap(), vec!["B"]);
        let view = store.view(&actor, &key).unwrap();
        assert!(!view.auto_drain);
        assert_eq!(view.items[0].item_id, items[1].item_id);
    }
    #[tokio::test]
    async fn new_execution_after_stop_is_not_stopped_again() {
        let fake = Arc::new(Fake {
            sent: Mutex::new(vec![]),
            busy: Mutex::new(true),
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
        let host = Arc::new(NewExecutionStop {
            inner: fake.clone(),
            switched: AtomicBool::new(false),
            calls: std::sync::atomic::AtomicUsize::new(0),
        });
        let store = SqlLocalQueueStore::new(DeviceStore::memory());
        let svc = Arc::new(ZcodeQueueService::new(
            store.clone(),
            host.clone(),
            Arc::new(Gate),
        ));
        let actor = QueueActor {
            device_id: "d".into(),
            device_name: "phone".into(),
        };
        let key = TargetKey {
            session_id: "session".into(),
        };
        let item = svc
            .enqueue(EnqueueCommand {
                request_id: "C".into(),
                actor: actor.clone(),
                session_id: key.session_id.clone(),
                workspace_path: "/synthetic".into(),
                workspace_identity: None,
                draft: QueueDraft {
                    text: "C".into(),
                    attachments: vec![],
                },
                prepared: PreparedSend::from_stored("C".into()).unwrap(),
                edited_from: None,
            })
            .await
            .unwrap();
        let result = svc
            .prioritize(
                &actor,
                super::super::types::PriorityCommand {
                    request_id: "p".into(),
                    item_id: item.item_id,
                    expected_version: item.item_version,
                    confirm_stop: true,
                    expected_execution_id: Some("A".into()),
                    held_queue: None,
                },
            )
            .await
            .unwrap();
        assert_eq!(
            result.state,
            super::super::types::QueueOperationState::Unknown
        );
        assert_eq!(host.calls.load(Ordering::SeqCst), 1);
        assert!(fake.sent.lock().unwrap().is_empty());
        assert!(!store.view(&actor, &key).unwrap().auto_drain);
    }
    struct PriorityTraceHost {
        inner: Arc<Fake>,
        db: DeviceStore,
        events: Mutex<Vec<String>>,
    }
    impl QueueHostPort for PriorityTraceHost {
        fn session_lock(&self, key: &TargetKey) -> Arc<tokio::sync::Mutex<()>> {
            self.inner.session_lock(key)
        }
        fn read<'a>(
            &'a self,
            key: &'a TargetKey,
            timeout: Duration,
        ) -> BoxFuture<'a, ZResult<BoundQueueSession>> {
            self.inner.read(key, timeout)
        }
        fn refresh_target<'a>(
            &'a self,
            key: &'a TargetKey,
            timeout: Duration,
        ) -> BoxFuture<'a, ZResult<BoundTarget>> {
            self.inner.refresh_target(key, timeout)
        }
        fn send_prepared<'a>(
            &'a self,
            ctx: &'a ActionContext,
            target: &'a BoundTarget,
            body: &'a PreparedSend,
            held: Option<&'a HeldQueueChoice>,
        ) -> BoxFuture<'a, super::super::super::command::PreparedResult<ActionReceipt>> {
            Box::pin(async move {
                self.events
                    .lock()
                    .unwrap()
                    .push(format!("send:{}", body.wire_text()));
                self.inner.send_prepared(ctx, target, body, held).await
            })
        }
        fn stop<'a>(
            &'a self,
            ctx: &'a ActionContext,
            target: &'a BoundTarget,
            execution: &'a str,
        ) -> BoxFuture<'a, super::super::super::command::PreparedResult<ActionReceipt>> {
            Box::pin(async move {
                self.db.with(|c| { let paused: i64=c.query_row("SELECT auto_drain FROM zcode_queue_state WHERE session_id=?1",[&target.session_id],|r|r.get(0)).unwrap(); assert_eq!(paused,0); let (claimed,action):(i64,String)=c.query_row("SELECT stop_claimed,stop_action_id FROM zcode_queue_operations WHERE request_id='priority'",[],|r|Ok((r.get(0)?,r.get(1)?))).unwrap(); assert_eq!(claimed,1); assert_eq!(action,ctx.action_id); let mut stmt=c.prepare("SELECT action FROM write_audit WHERE session_id=?1 AND action IN ('zcode_queue_priority','zcode_queue_stop') ORDER BY id").unwrap(); let actions=stmt.query_map([&target.session_id],|r|r.get::<_,String>(0)).unwrap().collect::<Result<Vec<_>,_>>().unwrap(); assert_eq!(actions,vec!["zcode_queue_priority","zcode_queue_stop"]); });
                {
                    let mut events = self.events.lock().unwrap();
                    events.push("pause_persisted".into());
                    events.push("stop_claimed".into());
                    events.push(format!("stop:{execution}"));
                }
                let receipt = self.inner.stop(ctx, target, execution).await?;
                assert_eq!(receipt.state, ActionState::Confirmed);
                self.events
                    .lock()
                    .unwrap()
                    .push(format!("terminal:{execution}"));
                Ok(receipt)
            })
        }
        fn receipt<'a>(
            &'a self,
            action: &'a str,
            device: &'a str,
        ) -> BoxFuture<'a, ZResult<Option<ActionReceipt>>> {
            self.inner.receipt(action, device)
        }
    }
    #[tokio::test]
    async fn priority_persists_pause_and_claim_before_one_stop_and_one_send() {
        let fake = Arc::new(Fake {
            sent: Mutex::new(vec![]),
            busy: Mutex::new(true),
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
        let db = DeviceStore::memory();
        let store = SqlLocalQueueStore::new(db.clone());
        let host = Arc::new(PriorityTraceHost {
            inner: fake.clone(),
            db,
            events: Mutex::new(vec![]),
        });
        let svc = Arc::new(ZcodeQueueService::new(
            store.clone(),
            host.clone(),
            Arc::new(Gate),
        ));
        let actor = QueueActor {
            device_id: "d".into(),
            device_name: "phone".into(),
        };
        let key = TargetKey {
            session_id: "session".into(),
        };
        let mut items = Vec::new();
        for text in ["B", "C"] {
            items.push(
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
                .unwrap(),
            );
        }
        svc.prioritize(
            &actor,
            super::super::types::PriorityCommand {
                request_id: "priority".into(),
                item_id: items[1].item_id.clone(),
                expected_version: items[1].item_version,
                confirm_stop: true,
                expected_execution_id: Some("A".into()),
                held_queue: None,
            },
        )
        .await
        .unwrap();
        assert_eq!(
            *host.events.lock().unwrap(),
            vec![
                "pause_persisted",
                "stop_claimed",
                "stop:A",
                "terminal:A",
                "send:C"
            ]
        );
        assert_eq!(
            host.events
                .lock()
                .unwrap()
                .iter()
                .filter(|s| s.starts_with("stop:"))
                .count(),
            1
        );
        assert_eq!(
            host.events
                .lock()
                .unwrap()
                .iter()
                .filter(|s| s.starts_with("send:"))
                .count(),
            1
        );
        let view = store.view(&actor, &key).unwrap();
        assert!(!view.auto_drain);
        assert_eq!(view.items[0].item_id, items[0].item_id);
    }
    #[tokio::test]
    async fn cancelled_manual_stop_finishes_without_repeating_frame() {
        let fake = Arc::new(Fake {
            sent: Mutex::new(vec![]),
            busy: Mutex::new(true),
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
        let host = Arc::new(BlockingPriorityStop {
            inner: fake.clone(),
            entered: Notify::new(),
            release: Notify::new(),
            calls: std::sync::atomic::AtomicUsize::new(0),
        });
        let store = SqlLocalQueueStore::new(DeviceStore::memory());
        let svc = Arc::new(ZcodeQueueService::new(
            store.clone(),
            host.clone(),
            Arc::new(Gate),
        ));
        let actor = QueueActor {
            device_id: "d".into(),
            device_name: "phone".into(),
        };
        let key = TargetKey {
            session_id: "session".into(),
        };
        let task_svc = svc.clone();
        let task_actor = actor.clone();
        let task_key = key.clone();
        let task = tokio::spawn(async move {
            task_svc
                .stop_current(&task_actor, &task_key, "stop", "A")
                .await
        });
        host.entered.notified().await;
        task.abort();
        host.release.notify_one();
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if store
                    .operation(&actor, "stop")
                    .unwrap()
                    .is_some_and(|o| o.state == super::super::types::QueueOperationState::Completed)
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let replay = svc.stop_current(&actor, &key, "stop", "A").await.unwrap();
        assert_eq!(
            replay.state,
            super::super::types::QueueOperationState::Completed
        );
        assert_eq!(host.calls.load(Ordering::SeqCst), 1);
        assert!(fake.sent.lock().unwrap().is_empty());
        assert!(!store.view(&actor, &key).unwrap().auto_drain);
    }
    #[tokio::test]
    async fn priority_claim_reopen_never_repeats_stop_or_sends() {
        let fake = Arc::new(Fake {
            sent: Mutex::new(vec![]),
            busy: Mutex::new(true),
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
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("queue.sqlite");
        let open = || {
            let conn = rusqlite::Connection::open(&path).unwrap();
            crate::database::schema::init(&conn);
            crate::database::migration::migrate(&conn).unwrap();
            SqlLocalQueueStore::new(DeviceStore::Owned(Arc::new(std::sync::Mutex::new(conn))))
        };
        let actor = QueueActor {
            device_id: "d".into(),
            device_name: "phone".into(),
        };
        let key = TargetKey {
            session_id: "session".into(),
        };
        let item;
        let action;
        {
            let store = open();
            item = store
                .enqueue(
                    &EnqueueCommand {
                        request_id: "C".into(),
                        actor: actor.clone(),
                        session_id: key.session_id.clone(),
                        workspace_path: "/synthetic".into(),
                        workspace_identity: None,
                        draft: QueueDraft {
                            text: "C".into(),
                            attachments: vec![],
                        },
                        prepared: PreparedSend::from_stored("C".into()).unwrap(),
                        edited_from: None,
                    },
                    1,
                )
                .unwrap();
            let owner = store
                .begin_priority(
                    &actor,
                    &super::super::types::PriorityCommand {
                        request_id: "p".into(),
                        item_id: item.item_id.clone(),
                        expected_version: item.item_version,
                        confirm_stop: true,
                        expected_execution_id: Some("A".into()),
                        held_queue: None,
                    },
                    2,
                )
                .unwrap();
            action = owner.stop_action_id;
            assert!(store.mark_stop_claimed(&actor, "p", 1, "A").unwrap());
        }
        let store = open();
        store.recover(3).unwrap();
        let recovery = store.pending_priority_recovery().unwrap();
        assert_eq!(recovery.len(), 1);
        assert_eq!(recovery[0].stop_action_id, action);
        let svc = Arc::new(ZcodeQueueService::new(
            store.clone(),
            fake.clone(),
            Arc::new(Gate),
        ));
        let cmd = super::super::types::PriorityCommand {
            request_id: "p".into(),
            item_id: item.item_id,
            expected_version: item.item_version,
            confirm_stop: true,
            expected_execution_id: Some("A".into()),
            held_queue: None,
        };
        assert_eq!(
            svc.prioritize(&actor, cmd.clone()).await.unwrap().state,
            super::super::types::QueueOperationState::Unknown
        );
        let mut changed = cmd;
        changed.request_id = "new".into();
        assert!(svc.prioritize(&actor, changed).await.is_err());
        assert!(fake.sent.lock().unwrap().is_empty());
        assert!(!fake.receipts.lock().unwrap().contains_key("stopped:A"));
        assert!(!store.view(&actor, &key).unwrap().auto_drain);
    }
    struct RecoveryReceiptHost {
        inner: Arc<Fake>,
        reads: Mutex<Vec<(String, String)>>,
    }
    impl QueueHostPort for RecoveryReceiptHost {
        fn session_lock(&self, key: &TargetKey) -> Arc<tokio::sync::Mutex<()>> {
            self.inner.session_lock(key)
        }
        fn read<'a>(
            &'a self,
            key: &'a TargetKey,
            timeout: Duration,
        ) -> BoxFuture<'a, ZResult<BoundQueueSession>> {
            self.inner.read(key, timeout)
        }
        fn refresh_target<'a>(
            &'a self,
            key: &'a TargetKey,
            timeout: Duration,
        ) -> BoxFuture<'a, ZResult<BoundTarget>> {
            self.inner.refresh_target(key, timeout)
        }
        fn send_prepared<'a>(
            &'a self,
            ctx: &'a ActionContext,
            target: &'a BoundTarget,
            body: &'a PreparedSend,
            held: Option<&'a HeldQueueChoice>,
        ) -> BoxFuture<'a, super::super::super::command::PreparedResult<ActionReceipt>> {
            self.inner.send_prepared(ctx, target, body, held)
        }
        fn stop<'a>(
            &'a self,
            ctx: &'a ActionContext,
            target: &'a BoundTarget,
            execution: &'a str,
        ) -> BoxFuture<'a, super::super::super::command::PreparedResult<ActionReceipt>> {
            self.inner.stop(ctx, target, execution)
        }
        fn receipt<'a>(
            &'a self,
            action: &'a str,
            device: &'a str,
        ) -> BoxFuture<'a, ZResult<Option<ActionReceipt>>> {
            Box::pin(async move {
                self.reads
                    .lock()
                    .unwrap()
                    .push((action.into(), device.into()));
                self.inner.receipt(action, device).await
            })
        }
    }
    #[tokio::test]
    async fn priority_recovery_reads_original_stop_receipt_without_writes() {
        let fake = Arc::new(Fake {
            sent: Mutex::new(vec![]),
            busy: Mutex::new(true),
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
        let actor = QueueActor {
            device_id: "d".into(),
            device_name: "phone".into(),
        };
        let key = TargetKey {
            session_id: "session".into(),
        };
        let item = store
            .enqueue(
                &EnqueueCommand {
                    request_id: "C".into(),
                    actor: actor.clone(),
                    session_id: key.session_id.clone(),
                    workspace_path: "/synthetic".into(),
                    workspace_identity: None,
                    draft: QueueDraft {
                        text: "C".into(),
                        attachments: vec![],
                    },
                    prepared: PreparedSend::from_stored("C".into()).unwrap(),
                    edited_from: None,
                },
                1,
            )
            .unwrap();
        let owner = store
            .begin_priority(
                &actor,
                &super::super::types::PriorityCommand {
                    request_id: "p".into(),
                    item_id: item.item_id,
                    expected_version: item.item_version,
                    confirm_stop: true,
                    expected_execution_id: Some("A".into()),
                    held_queue: None,
                },
                2,
            )
            .unwrap();
        store.mark_stop_claimed(&actor, "p", 1, "A").unwrap();
        let host = Arc::new(RecoveryReceiptHost {
            inner: fake.clone(),
            reads: Mutex::new(vec![]),
        });
        let svc = ZcodeQueueService::new(store.clone(), host.clone(), Arc::new(Gate));
        svc.recover().await.unwrap();
        assert_eq!(
            *host.reads.lock().unwrap(),
            vec![(owner.stop_action_id, "d".into())]
        );
        assert_eq!(
            store.operation(&actor, "p").unwrap().unwrap().state,
            super::super::types::QueueOperationState::Unknown
        );
        assert!(fake.sent.lock().unwrap().is_empty());
        assert!(!fake.receipts.lock().unwrap().contains_key("stopped:A"));
    }
    #[tokio::test]
    async fn idle_prioritize_does_not_require_preexisting_pause() {
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
        let svc = Arc::new(ZcodeQueueService::new(
            store.clone(),
            fake.clone(),
            Arc::new(Gate),
        ));
        let actor = QueueActor {
            device_id: "d".into(),
            device_name: "phone".into(),
        };
        let key = TargetKey {
            session_id: "session".into(),
        };
        let item = svc
            .enqueue(EnqueueCommand {
                request_id: "C".into(),
                actor: actor.clone(),
                session_id: key.session_id.clone(),
                workspace_path: "/synthetic".into(),
                workspace_identity: None,
                draft: QueueDraft {
                    text: "C".into(),
                    attachments: vec![],
                },
                prepared: PreparedSend::from_stored("C".into()).unwrap(),
                edited_from: None,
            })
            .await
            .unwrap();
        assert!(store.view(&actor, &key).unwrap().auto_drain);
        let cmd = super::super::types::PriorityCommand {
            request_id: "p".into(),
            item_id: item.item_id,
            expected_version: item.item_version,
            confirm_stop: false,
            expected_execution_id: None,
            held_queue: None,
        };
        svc.prioritize(&actor, cmd.clone()).await.unwrap();
        svc.prioritize(&actor, cmd).await.unwrap();
        assert_eq!(*fake.sent.lock().unwrap(), vec!["C"]);
        assert!(!fake.receipts.lock().unwrap().contains_key("stopped:A"));
        assert!(!store.view(&actor, &key).unwrap().auto_drain);
    }
    #[tokio::test]
    async fn unconfirmed_or_stale_execution_never_sends_stop_or_message() {
        let fake = Arc::new(Fake {
            sent: Mutex::new(vec![]),
            busy: Mutex::new(true),
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
        let svc = Arc::new(ZcodeQueueService::new(
            store.clone(),
            fake.clone(),
            Arc::new(Gate),
        ));
        let actor = QueueActor {
            device_id: "d".into(),
            device_name: "phone".into(),
        };
        let key = TargetKey {
            session_id: "session".into(),
        };
        let item = svc
            .enqueue(EnqueueCommand {
                request_id: "C".into(),
                actor: actor.clone(),
                session_id: key.session_id.clone(),
                workspace_path: "/synthetic".into(),
                workspace_identity: None,
                draft: QueueDraft {
                    text: "C".into(),
                    attachments: vec![],
                },
                prepared: PreparedSend::from_stored("C".into()).unwrap(),
                edited_from: None,
            })
            .await
            .unwrap();
        for (request, confirm, expected) in [("unconfirmed", false, "A"), ("stale", true, "old-A")]
        {
            let result = svc
                .prioritize(
                    &actor,
                    super::super::types::PriorityCommand {
                        request_id: request.into(),
                        item_id: item.item_id.clone(),
                        expected_version: item.item_version,
                        confirm_stop: confirm,
                        expected_execution_id: Some(expected.into()),
                        held_queue: None,
                    },
                )
                .await;
            assert!(result.is_err());
        }
        assert!(fake.sent.lock().unwrap().is_empty());
        assert!(!fake.receipts.lock().unwrap().contains_key("stopped:A"));
        let view = store.view(&actor, &key).unwrap();
        assert!(!view.auto_drain);
        assert_eq!(view.items[0].item_id, item.item_id);
    }
    #[tokio::test]
    async fn priority_stop_claim_failure_releases_unwritten_item() {
        let fake = Arc::new(Fake {
            sent: Mutex::new(vec![]),
            busy: Mutex::new(true),
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
        let db = DeviceStore::memory();
        let store = SqlLocalQueueStore::new(db.clone());
        let svc = Arc::new(ZcodeQueueService::new(
            store.clone(),
            fake.clone(),
            Arc::new(Gate),
        ));
        let actor = QueueActor {
            device_id: "d".into(),
            device_name: "phone".into(),
        };
        let key = TargetKey {
            session_id: "session".into(),
        };
        let item = svc
            .enqueue(EnqueueCommand {
                request_id: "C".into(),
                actor: actor.clone(),
                session_id: key.session_id.clone(),
                workspace_path: "/synthetic".into(),
                workspace_identity: None,
                draft: QueueDraft {
                    text: "C".into(),
                    attachments: vec![],
                },
                prepared: PreparedSend::from_stored("C".into()).unwrap(),
                edited_from: None,
            })
            .await
            .unwrap();
        db.with(|c| c.execute_batch("CREATE TRIGGER fail_priority_claim BEFORE INSERT ON write_audit WHEN NEW.action='zcode_queue_stop' BEGIN SELECT RAISE(ABORT,'synthetic'); END;").unwrap());
        assert!(svc
            .prioritize(
                &actor,
                super::super::types::PriorityCommand {
                    request_id: "p".into(),
                    item_id: item.item_id.clone(),
                    expected_version: item.item_version,
                    confirm_stop: true,
                    expected_execution_id: Some("A".into()),
                    held_queue: None
                }
            )
            .await
            .is_err());
        assert_eq!(
            store.operation(&actor, "p").unwrap().unwrap().state,
            super::super::types::QueueOperationState::Failed
        );
        let operation = store.operation(&actor, "p").unwrap().unwrap();
        let json = serde_json::to_value(&operation).unwrap();
        assert_eq!(json["stopFailure"]["phase"], "before_write");
        assert_eq!(json["stopFailure"]["code"], codes::STORAGE_FAILED);
        let replay = svc
            .prioritize(
                &actor,
                super::super::types::PriorityCommand {
                    request_id: "p".into(),
                    item_id: item.item_id.clone(),
                    expected_version: item.item_version,
                    confirm_stop: true,
                    expected_execution_id: Some("A".into()),
                    held_queue: None,
                },
            )
            .await
            .unwrap();
        assert_eq!(serde_json::to_value(replay).unwrap(), json);
        assert!(store
            .edit(
                &actor,
                &super::super::types::ItemCommand {
                    request_id: "edit".into(),
                    item_id: item.item_id,
                    expected_version: item.item_version
                },
                3
            )
            .is_ok());
        assert!(fake.sent.lock().unwrap().is_empty());
        assert!(!fake.receipts.lock().unwrap().contains_key("stopped:A"));
    }
    #[tokio::test]
    async fn priority_dispatch_claim_failure_after_stop_is_unknown_not_resend() {
        let fake = Arc::new(Fake {
            sent: Mutex::new(vec![]),
            busy: Mutex::new(true),
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
        let db = DeviceStore::memory();
        let store = SqlLocalQueueStore::new(db.clone());
        let svc = Arc::new(ZcodeQueueService::new(
            store.clone(),
            fake.clone(),
            Arc::new(Gate),
        ));
        let actor = QueueActor {
            device_id: "d".into(),
            device_name: "phone".into(),
        };
        let key = TargetKey {
            session_id: "session".into(),
        };
        let item = svc
            .enqueue(EnqueueCommand {
                request_id: "C".into(),
                actor: actor.clone(),
                session_id: key.session_id.clone(),
                workspace_path: "/synthetic".into(),
                workspace_identity: None,
                draft: QueueDraft {
                    text: "C".into(),
                    attachments: vec![],
                },
                prepared: PreparedSend::from_stored("C".into()).unwrap(),
                edited_from: None,
            })
            .await
            .unwrap();
        db.with(|c| c.execute_batch("CREATE TRIGGER fail_dispatch_claim BEFORE INSERT ON write_audit WHEN NEW.action='zcode_queue_dispatch' BEGIN SELECT RAISE(ABORT,'synthetic'); END;").unwrap());
        assert!(svc
            .prioritize(
                &actor,
                super::super::types::PriorityCommand {
                    request_id: "p".into(),
                    item_id: item.item_id,
                    expected_version: item.item_version,
                    confirm_stop: true,
                    expected_execution_id: Some("A".into()),
                    held_queue: None
                }
            )
            .await
            .is_err());
        assert_eq!(
            store.operation(&actor, "p").unwrap().unwrap().state,
            super::super::types::QueueOperationState::Unknown
        );
        assert!(fake.receipts.lock().unwrap().contains_key("stopped:A"));
        assert!(fake.sent.lock().unwrap().is_empty());
        assert!(!store.view(&actor, &key).unwrap().auto_drain);
    }
    #[tokio::test(start_paused = true)]
    async fn foreground_timeout_keeps_started_stop_alive_and_blocks_next_write() {
        let fake = Arc::new(Fake {
            sent: Mutex::new(vec![]),
            busy: Mutex::new(true),
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
        let host = Arc::new(BlockingPriorityStop {
            inner: fake.clone(),
            entered: Notify::new(),
            release: Notify::new(),
            calls: std::sync::atomic::AtomicUsize::new(0),
        });
        let store = SqlLocalQueueStore::new(DeviceStore::memory());
        let svc = Arc::new(ZcodeQueueService::new(
            store.clone(),
            host.clone(),
            Arc::new(Gate),
        ));
        let actor = QueueActor {
            device_id: "d".into(),
            device_name: "phone".into(),
        };
        let key = TargetKey {
            session_id: "session".into(),
        };
        let item = svc
            .enqueue(EnqueueCommand {
                request_id: "C".into(),
                actor: actor.clone(),
                session_id: key.session_id.clone(),
                workspace_path: "/synthetic".into(),
                workspace_identity: None,
                draft: QueueDraft {
                    text: "C".into(),
                    attachments: vec![],
                },
                prepared: PreparedSend::from_stored("C".into()).unwrap(),
                edited_from: None,
            })
            .await
            .unwrap();
        let task_svc = svc.clone();
        let task_actor = actor.clone();
        let task = tokio::spawn(async move {
            task_svc
                .prioritize(
                    &task_actor,
                    super::super::types::PriorityCommand {
                        request_id: "p".into(),
                        item_id: item.item_id,
                        expected_version: item.item_version,
                        confirm_stop: true,
                        expected_execution_id: Some("A".into()),
                        held_queue: None,
                    },
                )
                .await
        });
        host.entered.notified().await;
        tokio::time::advance(Duration::from_secs(60)).await;
        let foreground = task.await.unwrap().unwrap();
        assert_eq!(
            foreground.state,
            super::super::types::QueueOperationState::InFlight
        );
        host.release.notify_one();
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if store
                    .operation(&actor, "p")
                    .unwrap()
                    .is_some_and(|o| o.state == super::super::types::QueueOperationState::Unknown)
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(fake.receipts.lock().unwrap().contains_key("stopped:A"));
        assert_eq!(host.calls.load(Ordering::SeqCst), 1);
        assert!(fake.sent.lock().unwrap().is_empty());
        assert!(!store.view(&actor, &key).unwrap().auto_drain);
    }
    #[tokio::test]
    async fn priority_changed_path_never_stops() {
        priority_mismatched_selected_workspace_never_stops("/old", Some("new-id")).await;
    }
    #[tokio::test]
    async fn priority_changed_identity_never_stops() {
        priority_mismatched_selected_workspace_never_stops("/synthetic", Some("old-id")).await;
    }
    async fn priority_mismatched_selected_workspace_never_stops(
        path: &str,
        identity: Option<&str>,
    ) {
        {
            let fake = Arc::new(Fake {
                sent: Mutex::new(vec![]),
                busy: Mutex::new(true),
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
                identity: Some("new-id".into()),
                blocked_session: AtomicBool::new(false),
                read_calls: std::sync::atomic::AtomicUsize::new(0),
                read_failures: std::sync::atomic::AtomicUsize::new(0),
                refresh_failures: std::sync::atomic::AtomicUsize::new(0),
                send_failure: Mutex::new(None),
            });
            let store = SqlLocalQueueStore::new(DeviceStore::memory());
            let svc = Arc::new(ZcodeQueueService::new(
                store.clone(),
                fake.clone(),
                Arc::new(Gate),
            ));
            let actor = QueueActor {
                device_id: "d".into(),
                device_name: "phone".into(),
            };
            let key = TargetKey {
                session_id: "session".into(),
            };
            let mut selected = None;
            for (text, p, id) in [("B", "/synthetic", Some("new-id")), ("C", path, identity)] {
                let item = svc
                    .enqueue(EnqueueCommand {
                        request_id: text.into(),
                        actor: actor.clone(),
                        session_id: key.session_id.clone(),
                        workspace_path: p.into(),
                        workspace_identity: id.map(str::to_owned),
                        draft: QueueDraft {
                            text: text.into(),
                            attachments: vec![],
                        },
                        prepared: PreparedSend::from_stored(text.into()).unwrap(),
                        edited_from: None,
                    })
                    .await
                    .unwrap();
                if text == "C" {
                    selected = Some(item);
                }
            }
            let item = selected.unwrap();
            let result = svc
                .prioritize(
                    &actor,
                    super::super::types::PriorityCommand {
                        request_id: "p".into(),
                        item_id: item.item_id.clone(),
                        expected_version: item.item_version,
                        confirm_stop: true,
                        expected_execution_id: Some("A".into()),
                        held_queue: None,
                    },
                )
                .await;
            assert!(
                result.is_err()
                    || result.unwrap().state == super::super::types::QueueOperationState::Failed
            );
            let operation = store.operation(&actor, "p").unwrap().unwrap();
            let json = serde_json::to_value(operation).unwrap();
            assert_eq!(json["stopFailure"]["code"], codes::NO_SESSION);
            assert_eq!(json["stopFailure"]["phase"], "before_write");
            assert!(!fake.receipts.lock().unwrap().contains_key("stopped:A"));
            assert!(fake.sent.lock().unwrap().is_empty());
            assert!(store
                .edit(
                    &actor,
                    &super::super::types::ItemCommand {
                        request_id: "edit".into(),
                        item_id: item.item_id,
                        expected_version: item.item_version
                    },
                    3
                )
                .is_ok());
        }
    }
    #[tokio::test]
    async fn manual_claim_failure_safely_closes() {
        manual_claim_failure_case(false).await;
    }
    #[tokio::test]
    async fn manual_claim_and_closure_failure_can_retry() {
        manual_claim_failure_case(true).await;
    }
    async fn manual_claim_failure_case(fail_close: bool) {
        let fake = Arc::new(Fake {
            sent: Mutex::new(vec![]),
            busy: Mutex::new(true),
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
        let db = DeviceStore::memory();
        let store = SqlLocalQueueStore::new(db.clone());
        let svc = Arc::new(ZcodeQueueService::new(
            store.clone(),
            fake.clone(),
            Arc::new(Gate),
        ));
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
        db.with(|c| { c.execute_batch("CREATE TRIGGER fail_manual_claim BEFORE INSERT ON write_audit WHEN NEW.action='zcode_queue_stop_claim' BEGIN SELECT RAISE(ABORT,'synthetic'); END;").unwrap(); if fail_close { c.execute_batch("CREATE TRIGGER fail_manual_close BEFORE INSERT ON write_audit WHEN NEW.action='zcode_queue_stop_end' BEGIN SELECT RAISE(ABORT,'synthetic'); END;").unwrap(); }});
        assert!(svc.stop_current(&actor, &key, "stop", "A").await.is_err());
        if !fail_close {
            assert_eq!(
                store.operation(&actor, "stop").unwrap().unwrap().state,
                super::super::types::QueueOperationState::Failed
            );
        }
        db.with(|c| {
            c.execute_batch(
                "DROP TRIGGER fail_manual_claim; DROP TRIGGER IF EXISTS fail_manual_close;",
            )
            .unwrap();
        });
        let replay = svc.stop_current(&actor, &key, "stop", "A").await.unwrap();
        assert_eq!(
            replay.state,
            super::super::types::QueueOperationState::Failed
        );
        let json = serde_json::to_value(&replay).unwrap();
        assert_eq!(json["stopFailure"]["code"], codes::STORAGE_FAILED);
        assert_eq!(json["stopFailure"]["phase"], "before_write");
        assert!(!fake.receipts.lock().unwrap().contains_key("stopped:A"));
        assert!(fake.sent.lock().unwrap().is_empty());
        let paused = store.view(&actor, &key).unwrap();
        assert!(!paused.auto_drain);
        *fake.busy.lock().unwrap() = false;
        svc.resume(&actor, &key, "resume", paused.version)
            .await
            .unwrap();
        svc.flush_one(&key).await.unwrap();
        assert_eq!(*fake.sent.lock().unwrap(), vec!["B"]);
    }
    #[tokio::test]
    async fn manual_reason_needs_evidence() {
        manual_reason_case(codes::NEEDS_EVIDENCE, false).await;
    }
    #[tokio::test]
    async fn manual_reason_stale_generation() {
        manual_reason_case(codes::STALE_GENERATION, false).await;
    }
    #[tokio::test]
    async fn manual_reason_storage_failed() {
        manual_reason_case(codes::STORAGE_FAILED, false).await;
    }
    #[tokio::test]
    async fn manual_reason_may_have_written() {
        manual_reason_case(codes::NO_SESSION, true).await;
    }
    async fn manual_reason_case(code: &'static str, written: bool) {
        let fake = Arc::new(Fake {
            sent: Mutex::new(vec![]),
            busy: Mutex::new(true),
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
        let host = Arc::new(FailedPriorityStop {
            inner: fake.clone(),
            written,
            code,
            calls: std::sync::atomic::AtomicUsize::new(0),
        });
        let store = SqlLocalQueueStore::new(DeviceStore::memory());
        let svc = Arc::new(ZcodeQueueService::new(
            store.clone(),
            host.clone(),
            Arc::new(Gate),
        ));
        let actor = QueueActor {
            device_id: "d".into(),
            device_name: "phone".into(),
        };
        let key = TargetKey {
            session_id: "session".into(),
        };
        let operation = svc.stop_current(&actor, &key, "stop", "A").await.unwrap();
        let json = serde_json::to_value(&operation).unwrap();
        assert_eq!(json["stopFailure"]["code"], code);
        assert_eq!(
            json["stopFailure"]["phase"],
            if written {
                "may_have_written"
            } else {
                "before_write"
            }
        );
        assert_eq!(
            serde_json::to_value(svc.stop_current(&actor, &key, "stop", "A").await.unwrap())
                .unwrap(),
            json
        );
        assert_eq!(
            serde_json::to_value(store.operation(&actor, "stop").unwrap().unwrap()).unwrap(),
            json
        );
        assert_eq!(host.calls.load(Ordering::SeqCst), 1);
        assert!(fake.sent.lock().unwrap().is_empty());
    }
    #[tokio::test]
    async fn stop_reason_needs_evidence() {
        stop_reason_case(codes::NEEDS_EVIDENCE, false).await;
    }
    #[tokio::test]
    async fn stop_reason_stale_generation() {
        stop_reason_case(codes::STALE_GENERATION, false).await;
    }
    #[tokio::test]
    async fn stop_reason_storage_failed() {
        stop_reason_case(codes::STORAGE_FAILED, false).await;
    }
    #[tokio::test]
    async fn stop_reason_may_have_written() {
        stop_reason_case(codes::NO_SESSION, true).await;
    }
    async fn stop_reason_case(code: &'static str, written: bool) {
        let fake = Arc::new(Fake {
            sent: Mutex::new(vec![]),
            busy: Mutex::new(true),
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
        let host = Arc::new(FailedPriorityStop {
            inner: fake.clone(),
            written,
            code,
            calls: std::sync::atomic::AtomicUsize::new(0),
        });
        let store = SqlLocalQueueStore::new(DeviceStore::memory());
        let svc = Arc::new(ZcodeQueueService::new(
            store.clone(),
            host.clone(),
            Arc::new(Gate),
        ));
        let actor = QueueActor {
            device_id: "d".into(),
            device_name: "phone".into(),
        };
        let key = TargetKey {
            session_id: "session".into(),
        };
        let item = svc
            .enqueue(EnqueueCommand {
                request_id: "C".into(),
                actor: actor.clone(),
                session_id: key.session_id.clone(),
                workspace_path: "/synthetic".into(),
                workspace_identity: None,
                draft: QueueDraft {
                    text: "C".into(),
                    attachments: vec![],
                },
                prepared: PreparedSend::from_stored("C".into()).unwrap(),
                edited_from: None,
            })
            .await
            .unwrap();
        let cmd = super::super::types::PriorityCommand {
            request_id: "p".into(),
            item_id: item.item_id,
            expected_version: item.item_version,
            confirm_stop: true,
            expected_execution_id: Some("A".into()),
            held_queue: None,
        };
        let operation = svc.prioritize(&actor, cmd.clone()).await.unwrap();
        let json = serde_json::to_value(&operation).unwrap();
        assert_eq!(json["stopFailure"]["code"], code);
        assert_eq!(
            json["stopFailure"]["phase"],
            if written {
                "may_have_written"
            } else {
                "before_write"
            }
        );
        assert_eq!(
            serde_json::to_value(svc.prioritize(&actor, cmd).await.unwrap()).unwrap(),
            json
        );
        assert_eq!(
            serde_json::to_value(store.operation(&actor, "p").unwrap().unwrap()).unwrap(),
            json
        );
        assert_eq!(host.calls.load(Ordering::SeqCst), 1);
        assert!(fake.sent.lock().unwrap().is_empty());
    }
    struct BreakTargetReadHost {
        inner: Arc<Fake>,
        db: DeviceStore,
        broken: AtomicBool,
    }
    impl QueueHostPort for BreakTargetReadHost {
        fn session_lock(&self, key: &TargetKey) -> Arc<tokio::sync::Mutex<()>> {
            self.inner.session_lock(key)
        }
        fn read<'a>(
            &'a self,
            key: &'a TargetKey,
            timeout: Duration,
        ) -> BoxFuture<'a, ZResult<BoundQueueSession>> {
            self.inner.read(key, timeout)
        }
        fn refresh_target<'a>(
            &'a self,
            key: &'a TargetKey,
            timeout: Duration,
        ) -> BoxFuture<'a, ZResult<BoundTarget>> {
            Box::pin(async move {
                let target = self.inner.refresh_target(key, timeout).await?;
                if !self.broken.swap(true, Ordering::SeqCst) {
                    self.db.with(|c| {
                        c.execute_batch(
                            "ALTER TABLE zcode_send_queue RENAME TO synthetic_hidden_queue",
                        )
                        .unwrap()
                    });
                }
                Ok(target)
            })
        }
        fn send_prepared<'a>(
            &'a self,
            ctx: &'a ActionContext,
            target: &'a BoundTarget,
            body: &'a PreparedSend,
            held: Option<&'a HeldQueueChoice>,
        ) -> BoxFuture<'a, super::super::super::command::PreparedResult<ActionReceipt>> {
            self.inner.send_prepared(ctx, target, body, held)
        }
        fn stop<'a>(
            &'a self,
            ctx: &'a ActionContext,
            target: &'a BoundTarget,
            execution: &'a str,
        ) -> BoxFuture<'a, super::super::super::command::PreparedResult<ActionReceipt>> {
            self.inner.stop(ctx, target, execution)
        }
        fn receipt<'a>(
            &'a self,
            action: &'a str,
            device: &'a str,
        ) -> BoxFuture<'a, ZResult<Option<ActionReceipt>>> {
            self.inner.receipt(action, device)
        }
    }
    #[tokio::test]
    async fn target_lookup_storage_failure_recovers_without_stop() {
        let fake = Arc::new(Fake {
            sent: Mutex::new(vec![]),
            busy: Mutex::new(true),
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
        let db = DeviceStore::memory();
        let store = SqlLocalQueueStore::new(db.clone());
        let host = Arc::new(BreakTargetReadHost {
            inner: fake.clone(),
            db: db.clone(),
            broken: AtomicBool::new(false),
        });
        let svc = Arc::new(ZcodeQueueService::new(store.clone(), host, Arc::new(Gate)));
        let actor = QueueActor {
            device_id: "d".into(),
            device_name: "phone".into(),
        };
        let key = TargetKey {
            session_id: "session".into(),
        };
        let item = svc
            .enqueue(EnqueueCommand {
                request_id: "C".into(),
                actor: actor.clone(),
                session_id: key.session_id.clone(),
                workspace_path: "/synthetic".into(),
                workspace_identity: None,
                draft: QueueDraft {
                    text: "C".into(),
                    attachments: vec![],
                },
                prepared: PreparedSend::from_stored("C".into()).unwrap(),
                edited_from: None,
            })
            .await
            .unwrap();
        let cmd = super::super::types::PriorityCommand {
            request_id: "p".into(),
            item_id: item.item_id.clone(),
            expected_version: item.item_version,
            confirm_stop: true,
            expected_execution_id: Some("A".into()),
            held_queue: None,
        };
        assert!(svc.prioritize(&actor, cmd.clone()).await.is_err());
        db.with(|c| {
            c.execute_batch("ALTER TABLE synthetic_hidden_queue RENAME TO zcode_send_queue")
                .unwrap()
        });
        let replay = svc.prioritize(&actor, cmd.clone()).await.unwrap();
        assert_eq!(
            replay.state,
            super::super::types::QueueOperationState::Failed
        );
        let replay_json = serde_json::to_value(&replay).unwrap();
        assert!(replay_json.get("stopFailure").is_none());
        assert_eq!(
            serde_json::to_value(store.operation(&actor, "p").unwrap().unwrap()).unwrap(),
            replay_json
        );
        assert_eq!(
            serde_json::to_value(svc.prioritize(&actor, cmd).await.unwrap()).unwrap(),
            replay_json
        );
        assert!(store
            .edit(
                &actor,
                &super::super::types::ItemCommand {
                    request_id: "edit".into(),
                    item_id: item.item_id,
                    expected_version: item.item_version
                },
                3
            )
            .is_ok());
        assert!(!fake.receipts.lock().unwrap().contains_key("stopped:A"));
        assert!(fake.sent.lock().unwrap().is_empty());
    }
    #[tokio::test]
    async fn manual_preflight_failure_preserves_reason_for_replay() {
        let fake = Arc::new(Fake {
            sent: Mutex::new(vec![]),
            busy: Mutex::new(true),
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
            read_failures: std::sync::atomic::AtomicUsize::new(1),
            refresh_failures: std::sync::atomic::AtomicUsize::new(0),
            send_failure: Mutex::new(None),
        });
        let store = SqlLocalQueueStore::new(DeviceStore::memory());
        let svc = Arc::new(ZcodeQueueService::new(
            store.clone(),
            fake.clone(),
            Arc::new(Gate),
        ));
        let actor = QueueActor {
            device_id: "d".into(),
            device_name: "phone".into(),
        };
        let key = TargetKey {
            session_id: "session".into(),
        };
        assert!(svc.stop_current(&actor, &key, "stop", "A").await.is_err());
        let operation = store.operation(&actor, "stop").unwrap().unwrap();
        let json = serde_json::to_value(operation).unwrap();
        assert_eq!(json["stopFailure"]["code"], codes::NOT_READY);
        assert_eq!(json["stopFailure"]["phase"], "before_write");
        assert_eq!(
            serde_json::to_value(svc.stop_current(&actor, &key, "stop", "A").await.unwrap())
                .unwrap(),
            json
        );
        assert!(!fake.receipts.lock().unwrap().contains_key("stopped:A"));
        assert!(fake.sent.lock().unwrap().is_empty());
    }
    #[tokio::test]
    async fn manual_claim_and_close_fail_then_file_restart_never_forges_reason() {
        let fake = Arc::new(Fake {
            sent: Mutex::new(vec![]),
            busy: Mutex::new(true),
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
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("queue.sqlite");
        let open = || {
            let conn = rusqlite::Connection::open(&path).unwrap();
            crate::database::schema::init(&conn);
            crate::database::migration::migrate(&conn).unwrap();
            DeviceStore::Owned(Arc::new(std::sync::Mutex::new(conn)))
        };
        let actor = QueueActor {
            device_id: "d".into(),
            device_name: "phone".into(),
        };
        let key = TargetKey {
            session_id: "session".into(),
        };
        {
            let db = open();
            let store = SqlLocalQueueStore::new(db.clone());
            let svc = Arc::new(ZcodeQueueService::new(store, fake.clone(), Arc::new(Gate)));
            db.with(|c|c.execute_batch("CREATE TRIGGER fail_claim BEFORE INSERT ON write_audit WHEN NEW.action='zcode_queue_stop_claim' BEGIN SELECT RAISE(ABORT,'synthetic'); END; CREATE TRIGGER fail_end BEFORE INSERT ON write_audit WHEN NEW.action='zcode_queue_stop_end' BEGIN SELECT RAISE(ABORT,'synthetic'); END;").unwrap());
            assert!(svc.stop_current(&actor, &key, "stop", "A").await.is_err());
            db.with(|c| {
                c.execute_batch("DROP TRIGGER fail_claim; DROP TRIGGER fail_end;")
                    .unwrap()
            });
        }
        let store = SqlLocalQueueStore::new(open());
        store.recover(1).unwrap();
        let operation = store.operation(&actor, "stop").unwrap().unwrap();
        assert_eq!(
            operation.state,
            super::super::types::QueueOperationState::Failed
        );
        assert!(operation.stop_failure.is_none());
        assert!(!store.view(&actor, &key).unwrap().auto_drain);
        assert!(!fake.receipts.lock().unwrap().contains_key("stopped:A"));
        assert!(fake.sent.lock().unwrap().is_empty());
    }
    #[tokio::test]
    async fn pending_manual_failure_cache_is_bounded_and_keyed_by_request() {
        let fake = Arc::new(Fake {
            sent: Mutex::new(vec![]),
            busy: Mutex::new(true),
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
        let svc = ZcodeQueueService::new(
            SqlLocalQueueStore::new(DeviceStore::memory()),
            fake,
            Arc::new(Gate),
        );
        for i in 0..200 {
            svc.remember_manual_claim_failure(
                &format!("req-{i}"),
                super::super::types::StopFailure {
                    phase: "before_write".into(),
                    code: format!("code-{i}"),
                },
            );
        }
        let pending = svc.manual_claim_failures.lock().unwrap();
        assert_eq!(pending.len(), 128);
        assert_eq!(pending.get("req-0").unwrap().code, "code-0");
        assert!(!pending.contains_key("req-199"));
    }
    /// 正常回合运行中：本地有一条 accepted/startNow 且无终态的发送（**无重启标记**）。
    /// 此时“插队”必须能先停止当前回合、再发指定项，不能把普通运行中的 accepted
    /// 误判成重启待核对屏障。停止后宿主给出该次发送的终态（真实宿主行为）。
    struct TerminalAfterStop {
        inner: Arc<Fake>,
        stopped: AtomicBool,
    }
    impl QueueHostPort for TerminalAfterStop {
        fn session_lock(&self, key: &TargetKey) -> Arc<tokio::sync::Mutex<()>> {
            self.inner.session_lock(key)
        }
        fn read<'a>(
            &'a self,
            key: &'a TargetKey,
            timeout: Duration,
        ) -> BoxFuture<'a, ZResult<BoundQueueSession>> {
            Box::pin(async move {
                let mut bound = self.inner.read(key, timeout).await?;
                if self.stopped.load(Ordering::SeqCst) {
                    bound.view.revision = Some(2);
                    bound.view.execution_terminal = Some(ExecutionTerminal {
                        input_id: "B-input".into(),
                        result_type: "success".into(),
                        revision: 2,
                    });
                }
                Ok(bound)
            })
        }
        fn refresh_target<'a>(
            &'a self,
            key: &'a TargetKey,
            timeout: Duration,
        ) -> BoxFuture<'a, ZResult<BoundTarget>> {
            self.inner.refresh_target(key, timeout)
        }
        fn activate<'a>(
            &'a self,
            key: &'a TargetKey,
            timeout: Duration,
        ) -> BoxFuture<'a, ZResult<()>> {
            self.inner.activate(key, timeout)
        }
        fn send_prepared<'a>(
            &'a self,
            ctx: &'a ActionContext,
            target: &'a BoundTarget,
            body: &'a PreparedSend,
            held: Option<&'a HeldQueueChoice>,
        ) -> BoxFuture<'a, super::super::super::command::PreparedResult<ActionReceipt>> {
            self.inner.send_prepared(ctx, target, body, held)
        }
        fn stop<'a>(
            &'a self,
            ctx: &'a ActionContext,
            target: &'a BoundTarget,
            execution: &'a str,
        ) -> BoxFuture<'a, super::super::super::command::PreparedResult<ActionReceipt>> {
            Box::pin(async move {
                let receipt = self.inner.stop(ctx, target, execution).await?;
                self.stopped.store(true, Ordering::SeqCst);
                Ok(receipt)
            })
        }
        fn receipt<'a>(
            &'a self,
            action: &'a str,
            device: &'a str,
        ) -> BoxFuture<'a, ZResult<Option<ActionReceipt>>> {
            self.inner.receipt(action, device)
        }
    }
    #[tokio::test]
    async fn running_accepted_without_restart_flag_still_allows_priority_stop_and_send() {
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
        let host = Arc::new(TerminalAfterStop {
            inner: fake.clone(),
            stopped: AtomicBool::new(false),
        });
        let store = SqlLocalQueueStore::new(DeviceStore::memory());
        let svc = Arc::new(ZcodeQueueService::new(store.clone(), host, Arc::new(Gate)));
        let actor = QueueActor {
            device_id: "d".into(),
            device_name: "phone".into(),
        };
        let key = TargetKey {
            session_id: "session".into(),
        };
        let enqueue = |text: &'static str| EnqueueCommand {
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
        };
        svc.enqueue(enqueue("B")).await.unwrap();
        svc.flush_one(&key).await.unwrap();
        assert_eq!(*fake.sent.lock().unwrap(), vec!["B"]);
        // B 已 accepted/startNow 且无终态；宿主此时把该发送视为运行中的回合。
        *fake.busy.lock().unwrap() = true;
        let c = svc.enqueue(enqueue("C")).await.unwrap();

        let result = svc
            .prioritize(
                &actor,
                super::super::types::PriorityCommand {
                    request_id: "priority".into(),
                    item_id: c.item_id.clone(),
                    expected_version: c.item_version,
                    confirm_stop: true,
                    expected_execution_id: Some("A".into()),
                    held_queue: None,
                },
            )
            .await
            .unwrap();
        assert_eq!(
            result.state,
            super::super::types::QueueOperationState::Completed
        );
        assert!(fake.receipts.lock().unwrap().contains_key("stopped:A"));
        assert_eq!(*fake.sent.lock().unwrap(), vec!["B", "C"]);
        assert!(!store.view(&actor, &key).unwrap().auto_drain);
    }

    #[tokio::test]
    async fn prioritize_c_stops_a_then_sends_only_c() {
        let fake = Arc::new(Fake {
            sent: Mutex::new(vec![]),
            busy: Mutex::new(true),
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
        let svc = Arc::new(ZcodeQueueService::new(
            store.clone(),
            fake.clone(),
            Arc::new(Gate),
        ));
        let actor = QueueActor {
            device_id: "d".into(),
            device_name: "phone".into(),
        };
        let key = TargetKey {
            session_id: "session".into(),
        };
        let mut ids = Vec::new();
        for text in ["B", "C"] {
            ids.push(
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
                .unwrap(),
            );
        }
        svc.prioritize(
            &actor,
            super::super::types::PriorityCommand {
                request_id: "priority".into(),
                item_id: ids[1].item_id.clone(),
                expected_version: ids[1].item_version,
                confirm_stop: true,
                expected_execution_id: Some("A".into()),
                held_queue: None,
            },
        )
        .await
        .unwrap();
        assert!(fake.receipts.lock().unwrap().contains_key("stopped:A"));
        assert_eq!(*fake.sent.lock().unwrap(), vec!["C"]);
        let view = store.view(&actor, &key).unwrap();
        assert!(!view.auto_drain);
        assert_eq!(view.items[0].item_id, ids[0].item_id);
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
    struct BlockingSend {
        inner: Arc<Fake>,
        entered: Notify,
        release: Notify,
    }
    impl QueueHostPort for BlockingSend {
        fn session_lock(&self, key: &TargetKey) -> Arc<tokio::sync::Mutex<()>> {
            self.inner.session_lock(key)
        }
        fn read<'a>(
            &'a self,
            key: &'a TargetKey,
            timeout: Duration,
        ) -> BoxFuture<'a, ZResult<BoundQueueSession>> {
            self.inner.read(key, timeout)
        }
        fn refresh_target<'a>(
            &'a self,
            key: &'a TargetKey,
            timeout: Duration,
        ) -> BoxFuture<'a, ZResult<BoundTarget>> {
            self.inner.refresh_target(key, timeout)
        }
        fn send_prepared<'a>(
            &'a self,
            ctx: &'a ActionContext,
            target: &'a BoundTarget,
            body: &'a PreparedSend,
            held: Option<&'a HeldQueueChoice>,
        ) -> BoxFuture<'a, super::super::super::command::PreparedResult<ActionReceipt>> {
            Box::pin(async move {
                self.entered.notify_one();
                self.release.notified().await;
                self.inner.send_prepared(ctx, target, body, held).await
            })
        }
        fn stop<'a>(
            &'a self,
            ctx: &'a ActionContext,
            target: &'a BoundTarget,
            execution: &'a str,
        ) -> BoxFuture<'a, super::super::super::command::PreparedResult<ActionReceipt>> {
            self.inner.stop(ctx, target, execution)
        }
        fn receipt<'a>(
            &'a self,
            action: &'a str,
            device: &'a str,
        ) -> BoxFuture<'a, ZResult<Option<ActionReceipt>>> {
            self.inner.receipt(action, device)
        }
    }
    #[tokio::test]
    async fn dispatch_claim_notifies_before_send_ack() {
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
        let host = Arc::new(BlockingSend {
            inner: fake,
            entered: Notify::new(),
            release: Notify::new(),
        });
        let store = SqlLocalQueueStore::new(DeviceStore::memory());
        let svc = Arc::new(ZcodeQueueService::new(
            store.clone(),
            host.clone(),
            Arc::new(Gate),
        ));
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
        let mut events = svc.subscribe_queue();
        let worker = svc.clone();
        let worker_key = key.clone();
        let task = tokio::spawn(async move { worker.flush_one(&worker_key).await });
        host.entered.notified().await;
        assert_eq!(
            tokio::time::timeout(Duration::from_millis(100), events.recv())
                .await
                .unwrap()
                .unwrap(),
            key.session_id
        );
        assert_eq!(
            serde_json::to_value(store.view(&actor, &key).unwrap()).unwrap()["items"][0]["state"],
            "dispatching"
        );
        host.release.notify_one();
        task.await.unwrap().unwrap();
        let other = TargetKey {
            session_id: "second-session".into(),
        };
        let item = svc
            .enqueue(EnqueueCommand {
                request_id: "C".into(),
                actor: actor.clone(),
                session_id: other.session_id.clone(),
                workspace_path: "/synthetic".into(),
                workspace_identity: None,
                draft: QueueDraft {
                    text: "C".into(),
                    attachments: vec![],
                },
                prepared: PreparedSend::from_stored("C".into()).unwrap(),
                edited_from: None,
            })
            .await
            .unwrap();
        let mut priority_events = svc.subscribe_queue();
        let worker = svc.clone();
        let worker_actor = actor.clone();
        let task = tokio::spawn(async move {
            worker
                .prioritize(
                    &worker_actor,
                    super::super::types::PriorityCommand {
                        request_id: "priority-C".into(),
                        item_id: item.item_id,
                        expected_version: item.item_version,
                        confirm_stop: false,
                        expected_execution_id: None,
                        held_queue: None,
                    },
                )
                .await
        });
        host.entered.notified().await;
        assert_eq!(priority_events.recv().await.unwrap(), other.session_id);
        assert_eq!(
            tokio::time::timeout(Duration::from_millis(100), priority_events.recv())
                .await
                .unwrap()
                .unwrap(),
            other.session_id
        );
        assert_eq!(
            serde_json::to_value(store.view(&actor, &other).unwrap()).unwrap()["items"][0]["state"],
            "dispatching"
        );
        host.release.notify_one();
        task.await.unwrap().unwrap();
        // 队列自身事件只能通知订阅端，不能反过来唤醒 worker 形成循环。
        let _ = tokio::time::timeout(Duration::from_millis(10), svc.wake.notified()).await;
        let (tx, rx) = tokio::sync::broadcast::channel(8);
        let listener = tokio::spawn(crate::remote::run_queue_listener(svc.clone(), rx, false));
        tx.send(ControlEvent {
            seq: 1,
            generation: 1,
            session_id: Some(other.session_id.clone()),
            kind: ControlEventKind::LocalQueue,
            data: serde_json::Value::Null,
            device_id: None,
        })
        .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(100), svc.wake.notified())
                .await
                .is_err()
        );
        svc.shutdown();
        listener.await.unwrap();
    }
    struct TempPathHost {
        inner: Arc<Fake>,
        path: String,
    }
    impl QueueHostPort for TempPathHost {
        fn session_lock(&self, key: &TargetKey) -> Arc<tokio::sync::Mutex<()>> {
            self.inner.session_lock(key)
        }
        fn read<'a>(
            &'a self,
            key: &'a TargetKey,
            timeout: Duration,
        ) -> BoxFuture<'a, ZResult<BoundQueueSession>> {
            Box::pin(async move {
                let mut bound = self.inner.read(key, timeout).await?;
                bound.target.workspace_path = self.path.clone();
                bound.target.workspace_key = self.path.clone();
                Ok(bound)
            })
        }
        fn refresh_target<'a>(
            &'a self,
            key: &'a TargetKey,
            timeout: Duration,
        ) -> BoxFuture<'a, ZResult<BoundTarget>> {
            Box::pin(async move { Ok(self.read(key, timeout).await?.target) })
        }
        fn send_prepared<'a>(
            &'a self,
            ctx: &'a ActionContext,
            target: &'a BoundTarget,
            body: &'a PreparedSend,
            held: Option<&'a HeldQueueChoice>,
        ) -> BoxFuture<'a, super::super::super::command::PreparedResult<ActionReceipt>> {
            self.inner.send_prepared(ctx, target, body, held)
        }
        fn stop<'a>(
            &'a self,
            ctx: &'a ActionContext,
            target: &'a BoundTarget,
            execution: &'a str,
        ) -> BoxFuture<'a, super::super::super::command::PreparedResult<ActionReceipt>> {
            self.inner.stop(ctx, target, execution)
        }
        fn receipt<'a>(
            &'a self,
            action: &'a str,
            device: &'a str,
        ) -> BoxFuture<'a, ZResult<Option<ActionReceipt>>> {
            self.inner.receipt(action, device)
        }
    }
    #[tokio::test]
    async fn deleted_attachment_never_dispatches() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = crate::remote::attachments::attachment_dir(tmp.path(), "session");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("sample.txt");
        std::fs::write(&path, b"synthetic").unwrap();
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
        let svc = Arc::new(ZcodeQueueService::new(
            store.clone(),
            Arc::new(TempPathHost {
                inner: fake.clone(),
                path: tmp.path().to_string_lossy().into_owned(),
            }),
            Arc::new(Gate),
        ));
        let actor = QueueActor {
            device_id: "d".into(),
            device_name: "phone".into(),
        };
        let key = TargetKey {
            session_id: "session".into(),
        };
        svc.enqueue(EnqueueCommand {
            request_id: "attachment".into(),
            actor: actor.clone(),
            session_id: key.session_id.clone(),
            workspace_path: tmp.path().to_string_lossy().into_owned(),
            workspace_identity: None,
            draft: QueueDraft {
                text: "synthetic".into(),
                attachments: vec![super::super::types::QueuedAttachment {
                    name: "sample.txt".into(),
                    path: path.to_string_lossy().into_owned(),
                    is_image: false,
                }],
            },
            prepared: PreparedSend::from_stored("synthetic attachment".into()).unwrap(),
            edited_from: None,
        })
        .await
        .unwrap();
        std::fs::remove_file(path).unwrap();
        svc.flush_one(&key).await.unwrap();
        assert!(fake.sent.lock().unwrap().is_empty());
        let view = store.view(&actor, &key).unwrap();
        assert!(!view.auto_drain);
        assert_eq!(
            view.items[0].state,
            super::super::types::QueueItemState::Queued
        );
        let second_path = dir.join("second.txt");
        std::fs::write(&second_path, b"synthetic").unwrap();
        let second = svc
            .enqueue(EnqueueCommand {
                request_id: "attachment-C".into(),
                actor: actor.clone(),
                session_id: key.session_id.clone(),
                workspace_path: tmp.path().to_string_lossy().into_owned(),
                workspace_identity: None,
                draft: QueueDraft {
                    text: "C".into(),
                    attachments: vec![super::super::types::QueuedAttachment {
                        name: "second.txt".into(),
                        path: second_path.to_string_lossy().into_owned(),
                        is_image: false,
                    }],
                },
                prepared: PreparedSend::from_stored("synthetic C".into()).unwrap(),
                edited_from: None,
            })
            .await
            .unwrap();
        std::fs::remove_file(second_path).unwrap();
        *fake.busy.lock().unwrap() = true;
        let result = svc
            .prioritize(
                &actor,
                super::super::types::PriorityCommand {
                    request_id: "priority-C".into(),
                    item_id: second.item_id,
                    expected_version: second.item_version,
                    confirm_stop: true,
                    expected_execution_id: Some("A".into()),
                    held_queue: None,
                },
            )
            .await;
        assert!(result.is_err());
        assert!(!fake.receipts.lock().unwrap().contains_key("stopped:A"));
        assert!(fake.sent.lock().unwrap().is_empty());
        let third_path = dir.join("third.txt");
        std::fs::write(&third_path, b"synthetic").unwrap();
        let third = svc
            .enqueue(EnqueueCommand {
                request_id: "attachment-D".into(),
                actor: actor.clone(),
                session_id: key.session_id.clone(),
                workspace_path: tmp.path().to_string_lossy().into_owned(),
                workspace_identity: None,
                draft: QueueDraft {
                    text: "D".into(),
                    attachments: vec![super::super::types::QueuedAttachment {
                        name: "third.txt".into(),
                        path: third_path.to_string_lossy().into_owned(),
                        is_image: false,
                    }],
                },
                prepared: PreparedSend::from_stored("synthetic D".into()).unwrap(),
                edited_from: None,
            })
            .await
            .unwrap();
        *fake.busy.lock().unwrap() = false;
        *svc.before_priority_port.lock().unwrap() = Some(Arc::new(move |stage| {
            if stage == "send" {
                std::fs::remove_file(&third_path).unwrap();
            }
        }));
        let operation = svc
            .prioritize(
                &actor,
                super::super::types::PriorityCommand {
                    request_id: "priority-D".into(),
                    item_id: third.item_id.clone(),
                    expected_version: third.item_version,
                    confirm_stop: false,
                    expected_execution_id: None,
                    held_queue: None,
                },
            )
            .await
            .unwrap();
        assert_eq!(
            operation.state,
            super::super::types::QueueOperationState::Failed
        );
        assert!(fake.sent.lock().unwrap().is_empty());
        let view = store.view(&actor, &key).unwrap();
        assert!(view.items.iter().any(|item| item.item_id == third.item_id
            && item.state == super::super::types::QueueItemState::Queued));
        let fourth_path = dir.join("fourth.txt");
        std::fs::write(&fourth_path, b"synthetic").unwrap();
        let fourth = svc
            .enqueue(EnqueueCommand {
                request_id: "attachment-E".into(),
                actor: actor.clone(),
                session_id: key.session_id.clone(),
                workspace_path: tmp.path().to_string_lossy().into_owned(),
                workspace_identity: None,
                draft: QueueDraft {
                    text: "E".into(),
                    attachments: vec![super::super::types::QueuedAttachment {
                        name: "fourth.txt".into(),
                        path: fourth_path.to_string_lossy().into_owned(),
                        is_image: false,
                    }],
                },
                prepared: PreparedSend::from_stored("synthetic E".into()).unwrap(),
                edited_from: None,
            })
            .await
            .unwrap();
        *fake.busy.lock().unwrap() = true;
        *svc.before_priority_port.lock().unwrap() = Some(Arc::new(move |stage| {
            if stage == "stop" {
                std::fs::remove_file(&fourth_path).unwrap();
            }
        }));
        let operation = svc
            .prioritize(
                &actor,
                super::super::types::PriorityCommand {
                    request_id: "priority-E".into(),
                    item_id: fourth.item_id,
                    expected_version: fourth.item_version,
                    confirm_stop: true,
                    expected_execution_id: Some("A".into()),
                    held_queue: None,
                },
            )
            .await
            .unwrap();
        assert_eq!(
            operation.state,
            super::super::types::QueueOperationState::Failed
        );
        assert!(!fake.receipts.lock().unwrap().contains_key("stopped:A"));
        assert!(fake.sent.lock().unwrap().is_empty());
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
