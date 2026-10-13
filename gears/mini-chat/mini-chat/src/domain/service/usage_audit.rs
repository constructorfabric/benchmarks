//! Usage and audit outbox handlers (DESIGN §5.6 "Shared Outbox Processing Model").
//!
//! - Usage: deserialize (`Reject` on failure) → policy plugin lookup (`Retry` when not
//!   registered / client missing / resolution error) → `publish_usage` (`Ok`; `Transient` →
//!   `Retry`; `Permanent` → `Reject`).
//! - Audit: deserialize first (`Reject` even without a plugin) → lookup (`NotRegistered` → `Ok`,
//!   the event is dropped; `ClientMissing` / error → `Retry`) → `emit` with a 30 s timeout
//!   (timeout / `Transient` / `PluginTimeout` → `Retry`; `Permanent` → `Reject`). A `Retry` on the
//!   120th delivery becomes `Reject`.

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use mini_chat_sdk::{
    AuditEvent, AuditPluginError, MiniChatAuditPluginClientV1, MiniChatModelPolicyPluginClientV1,
    PublishError, UsageEvent,
};
use toolkit_db::outbox::{MessageResult, OutboxMessage};

use crate::domain::service::Deps;
use crate::infra::plugin_gateways::PluginLookup;

/// Audit plugin call timeout.
pub const AUDIT_EMIT_TIMEOUT: Duration = Duration::from_secs(30);
/// Deliveries of an audit message before a `Retry` turns into `Reject`.
pub const AUDIT_MAX_ATTEMPTS: i16 = 120;

/// Usage handler logic over an explicit plugin lookup (testable without `ClientHub`).
pub async fn handle_usage<L>(msg: &OutboxMessage, lookup: L) -> MessageResult
where
    L: Future<Output = Result<PluginLookup<dyn MiniChatModelPolicyPluginClientV1>, String>>,
{
    let ev: UsageEvent = match serde_json::from_slice(&msg.payload) {
        Ok(ev) => ev,
        Err(e) => {
            tracing::error!(error = %e, seq = msg.seq, "malformed usage outbox payload");
            return MessageResult::Reject(format!("malformed usage payload: {e}"));
        }
    };
    let client = match lookup.await {
        Ok(PluginLookup::Found(c)) => c,
        Ok(PluginLookup::NotRegistered) => {
            tracing::warn!("no model policy plugin registered; usage publication retried");
            return MessageResult::Retry;
        }
        Ok(PluginLookup::ClientMissing) => {
            tracing::warn!("model policy plugin client missing; usage publication retried");
            return MessageResult::Retry;
        }
        Err(e) => {
            tracing::warn!(error = %e, "model policy plugin resolution failed; usage publication retried");
            return MessageResult::Retry;
        }
    };
    let dedupe_key = ev.dedupe_key.clone();
    match client.publish_usage(ev).await {
        Ok(()) => MessageResult::Ok,
        Err(PublishError::Transient(e)) => {
            tracing::warn!(error = %e, dedupe_key, "transient usage publication failure");
            MessageResult::Retry
        }
        Err(PublishError::Permanent(e)) => {
            tracing::error!(error = %e, dedupe_key, "permanent usage publication failure");
            MessageResult::Reject(format!("publish_usage rejected: {e}"))
        }
    }
}

/// Audit handler logic over an explicit plugin lookup and timeout.
pub async fn handle_audit<L>(msg: &OutboxMessage, lookup: L, timeout: Duration) -> MessageResult
where
    L: Future<Output = Result<PluginLookup<dyn MiniChatAuditPluginClientV1>, String>>,
{
    let ev: AuditEvent = match serde_json::from_slice(&msg.payload) {
        Ok(ev) => ev,
        Err(e) => {
            tracing::error!(error = %e, seq = msg.seq, "malformed audit outbox payload");
            return MessageResult::Reject(format!("malformed audit payload: {e}"));
        }
    };
    let result = match lookup.await {
        Ok(PluginLookup::Found(client)) => {
            match tokio::time::timeout(timeout, client.emit(ev)).await {
                Ok(Ok(())) => MessageResult::Ok,
                Ok(Err(AuditPluginError::Permanent(e))) => {
                    tracing::error!(error = %e, "permanent audit plugin failure");
                    MessageResult::Reject(format!("audit plugin rejected: {e}"))
                }
                Ok(Err(e)) => {
                    tracing::warn!(error = %e, "transient audit plugin failure");
                    MessageResult::Retry
                }
                Err(_) => {
                    tracing::warn!("audit plugin call timed out");
                    MessageResult::Retry
                }
            }
        }
        // No plugin registered: the event is dropped (the gateway rate-limits the warning).
        Ok(PluginLookup::NotRegistered) => MessageResult::Ok,
        Ok(PluginLookup::ClientMissing) => {
            tracing::warn!("audit plugin client missing; retried");
            MessageResult::Retry
        }
        Err(e) => {
            tracing::warn!(error = %e, "audit plugin resolution failed; retried");
            MessageResult::Retry
        }
    };
    match result {
        MessageResult::Retry if msg.attempts >= AUDIT_MAX_ATTEMPTS - 1 => {
            tracing::error!(attempts = msg.attempts, "audit delivery failed on the last attempt");
            MessageResult::Reject(format!("audit delivery failed after {AUDIT_MAX_ATTEMPTS} attempts"))
        }
        other => other,
    }
}

/// Usage queue handler: publishes usage events to the policy plugin.
pub struct UsageHandler {
    deps: Arc<Deps>,
}

impl UsageHandler {
    #[must_use]
    pub fn new(deps: Arc<Deps>) -> Self {
        Self { deps }
    }
}

#[async_trait::async_trait]
impl toolkit_db::outbox::LeasedMessageHandler for UsageHandler {
    async fn handle(&self, msg: &OutboxMessage) -> MessageResult {
        handle_usage(msg, self.deps.policy.lookup()).await
    }
}

/// Audit queue handler: delivers audit events to the audit plugin.
pub struct AuditHandler {
    deps: Arc<Deps>,
}

impl AuditHandler {
    #[must_use]
    pub fn new(deps: Arc<Deps>) -> Self {
        Self { deps }
    }
}

#[async_trait::async_trait]
impl toolkit_db::outbox::LeasedMessageHandler for AuditHandler {
    async fn handle(&self, msg: &OutboxMessage) -> MessageResult {
        handle_audit(msg, self.deps.audit.lookup(), AUDIT_EMIT_TIMEOUT).await
    }
}

#[cfg(test)]
#[path = "usage_audit_tests.rs"]
mod usage_audit_tests;
