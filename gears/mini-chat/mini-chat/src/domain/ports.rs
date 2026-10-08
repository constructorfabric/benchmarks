//! Ports implemented by the infrastructure layer.

use async_trait::async_trait;
use mini_chat_sdk::{PolicySnapshot, PublishError, UsageEvent, UserLimits};
use uuid::Uuid;

use crate::domain::error::DomainError;

/// Model policy (catalog, kill switches, limits, usage publication).
#[async_trait]
pub trait PolicyProvider: Send + Sync {
    /// Current policy snapshot for the user.
    async fn current_snapshot(&self, user_id: Uuid) -> Result<PolicySnapshot, DomainError>;
    /// Snapshot of a specific version (settlement).
    async fn snapshot(&self, user_id: Uuid, version: u64) -> Result<PolicySnapshot, DomainError>;
    /// Per-user limits under a version.
    async fn user_limits(&self, user_id: Uuid, version: u64) -> Result<UserLimits, DomainError>;
    /// Publish one usage event.
    async fn publish_usage(&self, event: UsageEvent) -> Result<(), PublishError>;
}
