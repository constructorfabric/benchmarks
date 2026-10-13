//! Ports implemented by the infrastructure layer.

use async_trait::async_trait;
use mini_chat_sdk::{PolicySnapshot, UsageEvent, UserLimits};
use uuid::Uuid;

use super::error::DomainError;

/// The result of publishing a usage event to the model-policy plugin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UsagePublication {
    /// The plugin accepted the event.
    Published,
    /// A transient failure: the event must be redelivered.
    Retry(String),
    /// A permanent failure: the event must be dead-lettered.
    Reject(String),
}

/// Access to the model policy: the catalog and the kill switches of the current policy, the
/// per-user limits and the usage sink.
///
/// The gear keeps no snapshot cache (ADR-0008): every call asks the policy plugin for the current
/// policy version and then for the snapshot of that version.
#[async_trait]
pub trait ModelPolicyGateway: Send + Sync {
    /// The snapshot of the policy version that is current for the user.
    ///
    /// # Errors
    /// `PolicyUnavailable` when the plugin cannot be resolved or fails.
    async fn current_snapshot(&self, user_id: Uuid) -> Result<PolicySnapshot, DomainError>;

    /// The immutable snapshot of `policy_version`.
    ///
    /// # Errors
    /// `PolicyUnavailable` when the plugin cannot be resolved or is unavailable (a retry may
    /// succeed); `Internal` when the plugin does not know the version or fails in a way a retry
    /// will not fix (a retry never will; the orphan watchdog finalizes such a turn with a released
    /// reserve).
    async fn snapshot_at(
        &self,
        user_id: Uuid,
        policy_version: i64,
    ) -> Result<PolicySnapshot, DomainError>;

    /// The user's limits under `policy_version`.
    ///
    /// # Errors
    /// `PolicyUnavailable` when the plugin cannot be resolved or fails.
    async fn user_limits(
        &self,
        user_id: Uuid,
        policy_version: i64,
    ) -> Result<UserLimits, DomainError>;

    /// Publishes a settlement event to the plugin (at-least-once).
    async fn publish_usage(&self, event: UsageEvent) -> UsagePublication;
}

/// The trace id of the request being served.
pub trait TraceIdSource: Send + Sync {
    /// The OpenTelemetry trace id of the current span in lower-case hex, `None` when no span with
    /// a valid trace context is active.
    fn current_trace_id(&self) -> Option<String>;
}
