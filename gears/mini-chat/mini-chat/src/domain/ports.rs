//! Ports to external systems. Production implementations live in `infra`; tests use fakes.

use std::sync::Arc;

use async_trait::async_trait;
use mini_chat_sdk::{MiniChatAuditEvent, PolicySnapshot, UsageEvent, UserLimits};
use toolkit_security::{AccessScope, SecurityContext};
use uuid::Uuid;

use crate::domain::error::DomainError;

/// Authorization (PEP) port. Every returned chat scope already carries the subject's tenant
/// and owner predicates (defence in depth).
#[async_trait]
pub trait AuthzPort: Send + Sync {
    /// Scope for a Chat-resource action (`create`, `list`, `read`, `send_message`, ...).
    async fn chat_scope(
        &self,
        ctx: &SecurityContext,
        action: &str,
        chat_id: Option<Uuid>,
    ) -> Result<AccessScope, DomainError>;

    /// Decision-only check on the Model resource (`list`, `read`).
    async fn model_access(&self, ctx: &SecurityContext, action: &str) -> Result<(), DomainError>;

    /// Scope for the UserQuota resource (`read`).
    async fn quota_scope(&self, ctx: &SecurityContext) -> Result<AccessScope, DomainError>;
}

/// Model policy plugin port (DESIGN §5.2). No caching (ADR-0008).
#[async_trait]
pub trait PolicyPort: Send + Sync {
    /// Current policy snapshot for the user (resolves the version, then the snapshot).
    async fn current_snapshot(&self, user_id: Uuid) -> Result<Arc<PolicySnapshot>, DomainError>;

    /// Snapshot of a given version (settlement).
    async fn snapshot_by_version(&self, user_id: Uuid, version: i64) -> Result<Arc<PolicySnapshot>, DomainError>;

    /// Per-user limits of a version.
    async fn user_limits(&self, user_id: Uuid, version: i64) -> Result<UserLimits, DomainError>;

    /// Publishes a settled usage event (usage outbox handler).
    async fn publish_usage(&self, event: UsageEvent) -> Result<(), PublishFailure>;
}

/// Outcome of a failed usage publication.
#[derive(Debug, Clone)]
pub enum PublishFailure {
    /// Plugin missing or transient error: the handler returns `Retry`.
    Transient(String),
    /// Permanent error: the handler returns `Reject`.
    Permanent(String),
}

/// Result of an audit delivery attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuditDelivery {
    Delivered,
    /// No audit plugin registered: acknowledged and dropped.
    NoPlugin,
}

#[derive(Debug, Clone)]
pub enum AuditFailure {
    /// Transient plugin error, timeout, resolution failure or client missing: `Retry`.
    Transient(String),
    /// Permanent plugin error: `Reject`.
    Permanent(String),
}

/// Audit plugin port.
#[async_trait]
pub trait AuditPort: Send + Sync {
    async fn emit(&self, event: MiniChatAuditEvent) -> Result<AuditDelivery, AuditFailure>;
}
