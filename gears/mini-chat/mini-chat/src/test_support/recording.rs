//! Recording wrappers of the bundled plugins: the harness registers them in place of the plugin
//! clients so tests can see what the outbox handlers delivered.

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

use async_trait::async_trait;
use mini_chat_sdk::{
    AuditPluginError, MiniChatAuditPluginClientV1, MiniChatModelPolicyPluginClientV1,
    ModelPolicyPluginError, PolicySnapshot, PolicyVersionInfo, PublishError, TurnAuditEvent,
    TurnMutationAuditEvent, UsageEvent, UserLicenseStatus, UserLimits,
};
use parking_lot::Mutex;
use serde_json::Value;
use uuid::Uuid;

/// JSON copies of delivered events.
pub type Delivered = Arc<Mutex<Vec<Value>>>;

/// Fault injection and call counting of `publish_usage`.
#[derive(Default)]
pub struct PublishFaults {
    transient: AtomicU32,
    permanent: AtomicU32,
    attempts: AtomicUsize,
    snapshot_errors: Mutex<VecDeque<ModelPolicyPluginError>>,
}

impl PublishFaults {
    /// The next `count` publications fail transiently.
    pub fn fail_transient(&self, count: u32) {
        self.transient.store(count, Ordering::SeqCst);
    }

    /// The next `count` publications fail permanently.
    pub fn fail_permanent(&self, count: u32) {
        self.permanent.store(count, Ordering::SeqCst);
    }

    /// The next `errors.len()` calls of `get_policy_snapshot` fail with these errors, in order.
    pub fn fail_snapshots(&self, errors: impl IntoIterator<Item = ModelPolicyPluginError>) {
        *self.snapshot_errors.lock() = errors.into_iter().collect();
    }

    fn next_snapshot_error(&self) -> Option<ModelPolicyPluginError> {
        self.snapshot_errors.lock().pop_front()
    }

    /// Calls that reached the plugin wrapper, injected failures included.
    pub fn attempts(&self) -> usize {
        self.attempts.load(Ordering::SeqCst)
    }

    fn take(counter: &AtomicU32) -> bool {
        counter
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |left| {
                left.checked_sub(1)
            })
            .is_ok()
    }
}

/// Delegates to a model-policy plugin and records every published usage event.
pub struct RecordingPolicy {
    pub inner: Arc<dyn MiniChatModelPolicyPluginClientV1>,
    pub published: Delivered,
    pub faults: Arc<PublishFaults>,
}

#[async_trait]
impl MiniChatModelPolicyPluginClientV1 for RecordingPolicy {
    async fn get_current_policy_version(
        &self,
        user_id: Uuid,
    ) -> Result<PolicyVersionInfo, ModelPolicyPluginError> {
        self.inner.get_current_policy_version(user_id).await
    }

    async fn get_policy_snapshot(
        &self,
        user_id: Uuid,
        policy_version: i64,
    ) -> Result<PolicySnapshot, ModelPolicyPluginError> {
        if let Some(err) = self.faults.next_snapshot_error() {
            return Err(err);
        }
        self.inner
            .get_policy_snapshot(user_id, policy_version)
            .await
    }

    async fn get_user_limits(
        &self,
        user_id: Uuid,
        policy_version: i64,
    ) -> Result<UserLimits, ModelPolicyPluginError> {
        self.inner.get_user_limits(user_id, policy_version).await
    }

    async fn publish_usage(&self, event: UsageEvent) -> Result<(), PublishError> {
        self.faults.attempts.fetch_add(1, Ordering::SeqCst);
        if PublishFaults::take(&self.faults.transient) {
            return Err(PublishError::Transient(
                "injected transient failure".to_owned(),
            ));
        }
        if PublishFaults::take(&self.faults.permanent) {
            return Err(PublishError::Permanent(
                "injected permanent failure".to_owned(),
            ));
        }
        let json =
            serde_json::to_value(&event).map_err(|err| PublishError::Permanent(err.to_string()))?;
        let result = self.inner.publish_usage(event).await;
        if result.is_ok() {
            self.published.lock().push(json);
        }
        result
    }

    async fn check_user_license(
        &self,
        user_id: Uuid,
    ) -> Result<UserLicenseStatus, ModelPolicyPluginError> {
        self.inner.check_user_license(user_id).await
    }
}

/// Delegates to an audit plugin and records every delivered event.
pub struct RecordingAudit {
    pub inner: Arc<dyn MiniChatAuditPluginClientV1>,
    pub delivered: Delivered,
}

#[async_trait]
impl MiniChatAuditPluginClientV1 for RecordingAudit {
    async fn emit_turn_audit(&self, event: TurnAuditEvent) -> Result<(), AuditPluginError> {
        let json = serde_json::to_value(&event)
            .map_err(|err| AuditPluginError::Permanent(err.to_string()))?;
        let result = self.inner.emit_turn_audit(event).await;
        if result.is_ok() {
            self.delivered.lock().push(json);
        }
        result
    }

    async fn emit_turn_mutation_audit(
        &self,
        event: TurnMutationAuditEvent,
    ) -> Result<(), AuditPluginError> {
        let json = serde_json::to_value(&event)
            .map_err(|err| AuditPluginError::Permanent(err.to_string()))?;
        let result = self.inner.emit_turn_mutation_audit(event).await;
        if result.is_ok() {
            self.delivered.lock().push(json);
        }
        result
    }
}
