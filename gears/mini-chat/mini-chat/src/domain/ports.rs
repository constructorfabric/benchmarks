//! Domain ports implemented by the infrastructure layer.

use std::sync::Arc;

use async_trait::async_trait;
use mini_chat_sdk::{AuditEvent, PolicySnapshot, UsageEvent, UserLimits};
use uuid::Uuid;

use crate::domain::error::DomainResult;

/// Usage publication failure classes (outbox handler outcome).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UsagePublishError {
    Resolve(String),
    Transient(String),
    Permanent(String),
}

/// Model policy plugin port (no local cache, ADR-0008).
#[async_trait]
pub trait PolicyPort: Send + Sync {
    /// Current version and its snapshot.
    async fn current_snapshot(&self, user_id: Uuid) -> DomainResult<Arc<PolicySnapshot>>;
    /// Snapshot of a specific version (settlement).
    async fn snapshot(&self, user_id: Uuid, version: u64) -> DomainResult<Arc<PolicySnapshot>>;
    /// Per-user limits under a version.
    async fn user_limits(&self, user_id: Uuid, version: u64) -> DomainResult<UserLimits>;
    /// Publish a usage event.
    async fn publish_usage(&self, event: UsageEvent) -> Result<(), UsagePublishError>;
}

/// Delivery outcome of an audit event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuditDelivery {
    Ok,
    /// No audit plugin registered: acknowledged and dropped.
    Dropped,
    Retry(String),
    Reject(String),
}

/// Audit plugin port.
#[async_trait]
pub trait AuditPort: Send + Sync {
    async fn deliver(&self, event: AuditEvent) -> AuditDelivery;
}
