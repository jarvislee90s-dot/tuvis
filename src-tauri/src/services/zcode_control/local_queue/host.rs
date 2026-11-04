use super::super::{
    command::{PreparedResult, PreparedSend},
    types::{
        ActionContext, ActionReceipt, BoundTarget, HeldQueueChoice, SessionView, TargetKey, ZResult,
    },
};
use futures::future::BoxFuture;
use std::{sync::Arc, time::Duration};
use tokio::sync::Mutex;

pub(crate) struct BoundQueueSession {
    pub target: BoundTarget,
    pub view: SessionView,
}
pub(crate) trait QueueHostPort: Send + Sync {
    fn publish_queue_change(&self, _session_id: &str) {}
    fn session_lock(&self, key: &TargetKey) -> Arc<Mutex<()>>;
    fn activation_epoch(&self) -> Option<u64> {
        None
    }
    fn read<'a>(
        &'a self,
        key: &'a TargetKey,
        timeout: Duration,
    ) -> BoxFuture<'a, ZResult<BoundQueueSession>>;
    fn refresh_target<'a>(
        &'a self,
        _key: &'a TargetKey,
        _timeout: Duration,
    ) -> BoxFuture<'a, ZResult<BoundTarget>> {
        Box::pin(async {
            Err(super::super::types::ZcodeError::of(
                super::super::types::codes::NOT_READY,
            ))
        })
    }
    fn activate<'a>(
        &'a self,
        _key: &'a TargetKey,
        _timeout: Duration,
    ) -> BoxFuture<'a, ZResult<()>> {
        Box::pin(async {
            Err(super::super::types::ZcodeError::of(
                super::super::types::codes::NOT_READY,
            ))
        })
    }
    fn send_prepared<'a>(
        &'a self,
        ctx: &'a ActionContext,
        target: &'a BoundTarget,
        body: &'a PreparedSend,
        held: Option<&'a HeldQueueChoice>,
    ) -> BoxFuture<'a, PreparedResult<ActionReceipt>>;
    fn stop<'a>(
        &'a self,
        ctx: &'a ActionContext,
        target: &'a BoundTarget,
        execution_id: &'a str,
    ) -> BoxFuture<'a, PreparedResult<ActionReceipt>>;
    fn receipt<'a>(
        &'a self,
        action_id: &'a str,
        device_id: &'a str,
    ) -> BoxFuture<'a, ZResult<Option<ActionReceipt>>>;
}

impl QueueHostPort for super::super::hub::ZcodeHub {
    fn publish_queue_change(&self, session_id: &str) {
        self.queue_publish_change(session_id);
    }
    fn activation_epoch(&self) -> Option<u64> {
        self.queue_bridge_epoch()
    }
    fn session_lock(&self, key: &TargetKey) -> Arc<Mutex<()>> {
        self.session_write_lock(&key.session_id)
    }
    fn read<'a>(
        &'a self,
        key: &'a TargetKey,
        timeout: Duration,
    ) -> BoxFuture<'a, ZResult<BoundQueueSession>> {
        Box::pin(async move {
            if timeout.is_zero() {
                self.queue_read(key).await
            } else {
                tokio::time::timeout(timeout, self.queue_read(key))
                    .await
                    .map_err(|_| {
                        super::super::types::ZcodeError::of(super::super::types::codes::NOT_READY)
                    })?
            }
        })
    }
    fn refresh_target<'a>(
        &'a self,
        key: &'a TargetKey,
        timeout: Duration,
    ) -> BoxFuture<'a, ZResult<BoundTarget>> {
        Box::pin(async move {
            tokio::time::timeout(timeout, self.queue_refresh_target(key))
                .await
                .map_err(|_| {
                    super::super::types::ZcodeError::of(super::super::types::codes::NOT_READY)
                })?
        })
    }
    fn activate<'a>(&'a self, key: &'a TargetKey, timeout: Duration) -> BoxFuture<'a, ZResult<()>> {
        Box::pin(async move {
            tokio::time::timeout(timeout, self.session(key))
                .await
                .map_err(|_| {
                    super::super::types::ZcodeError::of(super::super::types::codes::NOT_READY)
                })??;
            Ok(())
        })
    }
    fn send_prepared<'a>(
        &'a self,
        ctx: &'a ActionContext,
        target: &'a BoundTarget,
        body: &'a PreparedSend,
        held: Option<&'a HeldQueueChoice>,
    ) -> BoxFuture<'a, PreparedResult<ActionReceipt>> {
        Box::pin(async move { self.queue_send_prepared(ctx, target, body, held).await })
    }
    fn stop<'a>(
        &'a self,
        ctx: &'a ActionContext,
        target: &'a BoundTarget,
        execution_id: &'a str,
    ) -> BoxFuture<'a, PreparedResult<ActionReceipt>> {
        Box::pin(async move { self.queue_stop(ctx, target, execution_id).await })
    }
    fn receipt<'a>(
        &'a self,
        action_id: &'a str,
        device_id: &'a str,
    ) -> BoxFuture<'a, ZResult<Option<ActionReceipt>>> {
        Box::pin(async move { self.queue_receipt(action_id, device_id) })
    }
}
