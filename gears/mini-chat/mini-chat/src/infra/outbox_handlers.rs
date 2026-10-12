//! Leased handlers of the usage and audit outbox queues (DESIGN section 5.6
//! "Shared Outbox Processing Model", section 3.2 "Audit plugin and audit
//! outbox", B.9.3).
//!
//! [`UsageHandler`] publishes one SDK [`UsageEvent`] through the model policy
//! gateway (`publish_usage`):
//!
//! | Situation | Result |
//! |-----------|--------|
//! | published | `Ok` |
//! | plugin resolution failure, transient [`PublishError`] | `Retry` |
//! | payload not a `UsageEvent`, permanent [`PublishError`] | `Reject` |
//!
//! [`AuditHandler`] deserializes the SDK [`AuditEvent`] first (a malformed
//! payload is rejected whether or not a plugin can be resolved), then
//! delivers it through the [`AuditGateway`]:
//!
//! | [`AuditDelivery`] | Result | `{prefix}_audit_emit{result}` |
//! |-------------------|--------|-------------------------------|
//! | `Delivered` | `Ok` | `ok` |
//! | `Dropped` (no plugin registered) | `Ok` | `dropped` |
//! | `Retry` | `Retry` | `retry` |
//! | `Retry` on attempt [`AUDIT_MAX_ATTEMPTS`] | `Reject` | `reject` |
//! | `Reject` | `Reject` | `reject` |
//!
//! A malformed payload is `Reject` and counted as `reject`. The bound keeps a
//! misconfigured plugin from blocking the partition (about an hour with the
//! outbox backoff capped at 30 s). Retry backoff, leases and dead letters
//! are the shared outbox's.
//!
//! [`AttachmentCleanupHandler`] deletes the provider file of one attachment
//! (DESIGN section 4 "Attachment Deletion" phase 2, section 3.6 "Attachment
//! cleanup state machine"). Every query is confined to the payload's tenant
//! and chat:
//!
//! | Situation | Result |
//! |-----------|--------|
//! | payload not an [`AttachmentCleanupPayload`] | `Reject` |
//! | parent chat soft-deleted (chat cleanup owns the file), chat or attachment row gone, cleanup already terminal | `Ok` |
//! | no provider file (payload, else the row) | `cleanup_status = done`, `Ok` |
//! | provider delete 2xx or `404` | secondary copy deleted best effort, `cleanup_status = done`, `Ok` |
//! | provider delete failed | `cleanup_attempts + 1`, `last_cleanup_error`, `cleanup_updated_at`; `Retry` |
//! | ... and the attempts reach `cleanup_worker.max_attempts` | `cleanup_status = failed`, `Reject` (dead letter) |
//! | database failure | `Retry`, `cleanup_attempts` unchanged |
//!
//! The handler makes no Vector Stores call: the chat vector store is deleted
//! with the chat.

use std::sync::Arc;

use async_trait::async_trait;
use mini_chat_sdk::{AuditEvent, MiniChatModelPolicyPluginClientV1, PublishError, UsageEvent};
use opentelemetry::KeyValue;
use opentelemetry::metrics::Counter;
use toolkit_db::outbox::{LeasedMessageHandler, MessageResult, OutboxMessage};
use toolkit_security::AccessScope;
use tracing::{debug, error, info, warn};

use crate::domain::error::DomainError;
use crate::domain::ports::{
    CleanupStatus, FileStorage, SecondaryFileStorage, StorageTarget, storage_now,
};
use crate::infra::audit_gateway::{AuditDelivery, AuditGateway};
use crate::infra::db::repo::DbRepository;
use crate::infra::db::repo::attachments as attachment_repo;
use crate::infra::outbox::{AttachmentCleanupPayload, SecondaryFileRef};

/// The audit handler gives up (`Reject`) when a delivery that would be
/// retried is attempt number `AUDIT_MAX_ATTEMPTS` (1-based).
pub const AUDIT_MAX_ATTEMPTS: u32 = 120;

/// Attempt number (1-based) of `msg`; the outbox counts retries from 0.
fn attempt_number(msg: &OutboxMessage) -> u32 {
    u32::try_from(msg.attempts).unwrap_or(0).saturating_add(1)
}

// ── Usage ────────────────────────────────────────────────────────────────

/// Handler of the usage queue: publishes [`UsageEvent`]s to the model policy
/// plugin.
pub struct UsageHandler {
    policy: Arc<dyn MiniChatModelPolicyPluginClientV1>,
}

impl std::fmt::Debug for UsageHandler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UsageHandler").finish_non_exhaustive()
    }
}

impl UsageHandler {
    /// Handler publishing through `policy` (the model policy gateway, whose
    /// resolution failures are transient [`PublishError`]s).
    #[must_use]
    pub fn new(policy: Arc<dyn MiniChatModelPolicyPluginClientV1>) -> Self {
        Self { policy }
    }
}

#[async_trait]
impl LeasedMessageHandler for UsageHandler {
    async fn handle(&self, msg: &OutboxMessage) -> MessageResult {
        let event: UsageEvent = match serde_json::from_slice(&msg.payload) {
            Ok(event) => event,
            Err(err) => {
                error!(
                    target: "mini_chat.outbox.usage",
                    partition_id = msg.partition_id,
                    seq = msg.seq,
                    error = %err,
                    "malformed usage payload rejected"
                );
                return MessageResult::Reject(format!("malformed usage payload: {err}"));
            }
        };
        let dedupe_key = event.dedupe_key.clone();
        match self.policy.publish_usage(event).await {
            Ok(()) => {
                debug!(target: "mini_chat.outbox.usage", dedupe_key = %dedupe_key, "usage event published");
                MessageResult::Ok
            }
            Err(err @ PublishError::Transient(_)) => {
                warn!(
                    target: "mini_chat.outbox.usage",
                    dedupe_key = %dedupe_key,
                    attempts = msg.attempts,
                    error = %err,
                    "usage publication failed; retrying"
                );
                MessageResult::Retry
            }
            Err(err @ PublishError::Permanent(_)) => {
                error!(
                    target: "mini_chat.outbox.usage",
                    dedupe_key = %dedupe_key,
                    error = %err,
                    "usage publication rejected permanently"
                );
                MessageResult::Reject(err.to_string())
            }
        }
    }
}

// ── Audit ────────────────────────────────────────────────────────────────

/// `result` label of the audit emit counter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuditEmitResult {
    Ok,
    Retry,
    Reject,
    Dropped,
}

impl AuditEmitResult {
    /// Label value.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Retry => "retry",
            Self::Reject => "reject",
            Self::Dropped => "dropped",
        }
    }
}

/// Handler outcome of one audit delivery at attempt `attempt` (1-based).
#[must_use]
pub fn audit_outcome(delivery: AuditDelivery, attempt: u32) -> (MessageResult, AuditEmitResult) {
    match delivery {
        AuditDelivery::Delivered => (MessageResult::Ok, AuditEmitResult::Ok),
        AuditDelivery::Dropped => (MessageResult::Ok, AuditEmitResult::Dropped),
        AuditDelivery::Retry(reason) if attempt >= AUDIT_MAX_ATTEMPTS => (
            MessageResult::Reject(format!(
                "audit delivery still failing after {attempt} attempts: {reason}"
            )),
            AuditEmitResult::Reject,
        ),
        AuditDelivery::Retry(_) => (MessageResult::Retry, AuditEmitResult::Retry),
        AuditDelivery::Reject(reason) => (MessageResult::Reject(reason), AuditEmitResult::Reject),
    }
}

/// Handler of the audit queue: delivers [`AuditEvent`]s to the audit plugin.
pub struct AuditHandler {
    gateway: Arc<AuditGateway>,
    emitted: Counter<u64>,
}

impl std::fmt::Debug for AuditHandler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuditHandler").finish_non_exhaustive()
    }
}

impl AuditHandler {
    /// Handler delivering through `gateway`; outcomes are counted in
    /// `{metrics_prefix}_audit_emit{result}`.
    #[must_use]
    pub fn new(gateway: Arc<AuditGateway>, metrics_prefix: &str) -> Self {
        let scope = opentelemetry::InstrumentationScope::builder("mini-chat").build();
        let emitted = opentelemetry::global::meter_with_scope(scope)
            .u64_counter(format!("{metrics_prefix}_audit_emit"))
            .with_description("Delivery outcomes of the mini-chat audit outbox handler")
            .build();
        Self { gateway, emitted }
    }

    fn count(&self, result: AuditEmitResult) {
        self.emitted
            .add(1, &[KeyValue::new("result", result.as_str())]);
    }
}

#[async_trait]
impl LeasedMessageHandler for AuditHandler {
    async fn handle(&self, msg: &OutboxMessage) -> MessageResult {
        let event: AuditEvent = match serde_json::from_slice(&msg.payload) {
            Ok(event) => event,
            Err(err) => {
                error!(
                    target: "mini_chat.outbox.audit",
                    partition_id = msg.partition_id,
                    seq = msg.seq,
                    error = %err,
                    "malformed audit payload rejected"
                );
                self.count(AuditEmitResult::Reject);
                return MessageResult::Reject(format!("malformed audit payload: {err}"));
            }
        };
        let attempt = attempt_number(msg);
        let (result, emitted) = audit_outcome(self.gateway.deliver(event).await, attempt);
        match &result {
            MessageResult::Retry => warn!(
                target: "mini_chat.outbox.audit",
                attempt,
                "audit delivery failed; retrying"
            ),
            MessageResult::Reject(reason) => error!(
                target: "mini_chat.outbox.audit",
                attempt,
                reason = %reason,
                "audit event dead-lettered"
            ),
            MessageResult::Ok => {}
        }
        self.count(emitted);
        result
    }
}

// ── Attachment cleanup ───────────────────────────────────────────────────

/// Handler of the attachment cleanup queue: deletes the provider file of one
/// attachment and records the cleanup outcome (module docs).
pub struct AttachmentCleanupHandler {
    repo: Arc<DbRepository>,
    files: Arc<dyn FileStorage>,
    secondary: Option<Arc<dyn SecondaryFileStorage>>,
    max_attempts: u32,
}

impl std::fmt::Debug for AttachmentCleanupHandler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AttachmentCleanupHandler")
            .field("max_attempts", &self.max_attempts)
            .finish_non_exhaustive()
    }
}

/// Logs why a message is acknowledged without a provider call.
fn log_ack(payload: &AttachmentCleanupPayload, message: &str) {
    debug!(
        target: "mini_chat.outbox.attachment_cleanup",
        attachment_id = %payload.attachment_id,
        "{message}"
    );
}

/// A database failure of the handler: the message is retried without
/// counting an attempt.
fn infra_retry(attachment: &AttachmentCleanupPayload, err: &DomainError) -> MessageResult {
    warn!(
        target: "mini_chat.outbox.attachment_cleanup",
        attachment_id = %attachment.attachment_id,
        error = %err,
        "attachment cleanup: database failure; retrying"
    );
    MessageResult::Retry
}

impl AttachmentCleanupHandler {
    /// Handler over the repositories, the provider Files API and
    /// `cleanup_worker.max_attempts`; secondary copies are skipped (logged)
    /// until a [`SecondaryFileStorage`] is supplied.
    #[must_use]
    pub fn new(repo: Arc<DbRepository>, files: Arc<dyn FileStorage>, max_attempts: u32) -> Self {
        Self {
            repo,
            files,
            secondary: None,
            max_attempts: max_attempts.max(1),
        }
    }

    /// Deletes secondary (Anthropic) copies through `secondary`.
    #[must_use]
    pub fn with_secondary(mut self, secondary: Arc<dyn SecondaryFileStorage>) -> Self {
        self.secondary = Some(secondary);
        self
    }

    /// Processes one decoded payload.
    pub async fn process(&self, payload: &AttachmentCleanupPayload) -> MessageResult {
        match self.provider_file(payload).await {
            Err(result) => result,
            Ok(None) => self.mark_done(payload).await,
            Ok(Some(file_id)) => self.delete_primary(payload, &file_id).await,
        }
    }

    /// The provider file to delete (the payload's, else the row's; `None`
    /// when the upload never reached the provider), or the final result when
    /// this handler has nothing to do.
    async fn provider_file(
        &self,
        payload: &AttachmentCleanupPayload,
    ) -> Result<Option<String>, MessageResult> {
        let scope = AccessScope::for_tenant(payload.tenant_id);
        let conn = self
            .repo
            .db()
            .conn()
            .map_err(|err| infra_retry(payload, &DomainError::from(err)))?;
        let chat_deleted = attachment_repo::chat_deleted(&conn, &scope, payload.chat_id)
            .await
            .map_err(|err| infra_retry(payload, &err))?;
        match chat_deleted {
            Some(false) => {}
            Some(true) => {
                log_ack(
                    payload,
                    "chat is soft-deleted; chat cleanup owns the provider file",
                );
                return Err(MessageResult::Ok);
            }
            None => {
                log_ack(
                    payload,
                    "attachment cleanup for an unknown chat acknowledged",
                );
                return Err(MessageResult::Ok);
            }
        }
        let row = attachment_repo::find(&conn, &scope, payload.chat_id, payload.attachment_id)
            .await
            .map_err(|err| infra_retry(payload, &err))?;
        let Some(row) = row else {
            log_ack(
                payload,
                "attachment cleanup for an unknown attachment acknowledged",
            );
            return Err(MessageResult::Ok);
        };
        let terminal = [CleanupStatus::Done.as_str(), CleanupStatus::Failed.as_str()];
        if row
            .cleanup_status
            .as_deref()
            .is_some_and(|status| terminal.contains(&status))
        {
            return Err(MessageResult::Ok);
        }
        Ok(payload.provider_file_id.clone().or(row.provider_file_id))
    }

    /// Deletes the primary file (and the secondary copy, best effort).
    async fn delete_primary(
        &self,
        payload: &AttachmentCleanupPayload,
        file_id: &str,
    ) -> MessageResult {
        let target = StorageTarget {
            backend: payload.storage_backend.clone(),
            tenant_id: payload.tenant_id,
        };
        match self.files.delete_file(&target, file_id).await {
            Ok(()) => {
                if let Some(secondary) = &payload.secondary_ref {
                    self.delete_secondary(payload, secondary).await;
                }
                self.mark_done(payload).await
            }
            Err(err) => self.record_failure(payload, &err.to_string()).await,
        }
    }

    async fn mark_done(&self, payload: &AttachmentCleanupPayload) -> MessageResult {
        let scope = AccessScope::for_tenant(payload.tenant_id);
        let conn = match self.repo.db().conn() {
            Ok(conn) => conn,
            Err(err) => return infra_retry(payload, &DomainError::from(err)),
        };
        match attachment_repo::mark_cleanup_done(
            &conn,
            self.repo.timestamps(),
            &scope,
            (payload.chat_id, payload.attachment_id),
            storage_now(),
        )
        .await
        {
            Ok(_) => {
                info!(target: "mini_chat.outbox.attachment_cleanup", attachment_id = %payload.attachment_id, "attachment provider cleanup done");
                MessageResult::Ok
            }
            Err(err) => infra_retry(payload, &err),
        }
    }

    /// Records the failed attempt; `Reject` once the limit is reached.
    async fn record_failure(
        &self,
        payload: &AttachmentCleanupPayload,
        error: &str,
    ) -> MessageResult {
        let scope = AccessScope::for_tenant(payload.tenant_id);
        let ts = self.repo.timestamps();
        let ids = (payload.chat_id, payload.attachment_id);
        let max_attempts = self.max_attempts;
        let error_text = error.to_owned();
        let recorded = self
            .repo
            .db()
            .db()
            .transaction_ref_mapped(move |tx| {
                Box::pin(async move {
                    attachment_repo::record_cleanup_failure_in_tx(
                        tx,
                        ts,
                        &scope,
                        ids,
                        &error_text,
                        max_attempts,
                        storage_now(),
                    )
                    .await
                })
            })
            .await;
        match recorded {
            Err(err) => infra_retry(payload, &err),
            Ok(None) => MessageResult::Ok,
            Ok(Some(failure)) if failure.terminal => {
                error!(
                    target: "mini_chat.outbox.attachment_cleanup",
                    attachment_id = %payload.attachment_id,
                    attempts = failure.attempts,
                    error = %error,
                    "attachment provider cleanup failed permanently"
                );
                MessageResult::Reject(format!(
                    "provider file delete: max attempts ({max_attempts}) reached"
                ))
            }
            Ok(Some(failure)) => {
                warn!(
                    target: "mini_chat.outbox.attachment_cleanup",
                    attachment_id = %payload.attachment_id,
                    attempts = failure.attempts,
                    error = %error,
                    "provider file delete failed; retrying"
                );
                MessageResult::Retry
            }
        }
    }

    /// Best-effort delete of the secondary copy; never blocks the primary
    /// cleanup outcome.
    async fn delete_secondary(
        &self,
        payload: &AttachmentCleanupPayload,
        secondary: &SecondaryFileRef,
    ) {
        let Some(store) = &self.secondary else {
            warn!(target: "mini_chat.outbox.attachment_cleanup", attachment_id = %payload.attachment_id, provider_kind = %secondary.provider_kind, "secondary file cleanup skipped: no secondary storage configured");
            return;
        };
        if let Err(err) = store
            .delete_secondary_file(
                payload.tenant_id,
                &secondary.file_id,
                &secondary.provider_kind,
                &secondary.upstream_alias,
            )
            .await
        {
            warn!(target: "mini_chat.outbox.attachment_cleanup", attachment_id = %payload.attachment_id, error = %err, "secondary file delete failed");
        }
    }
}

#[async_trait]
impl LeasedMessageHandler for AttachmentCleanupHandler {
    async fn handle(&self, msg: &OutboxMessage) -> MessageResult {
        match serde_json::from_slice::<AttachmentCleanupPayload>(&msg.payload) {
            Ok(payload) => self.process(&payload).await,
            Err(err) => {
                error!(
                    target: "mini_chat.outbox.attachment_cleanup",
                    partition_id = msg.partition_id,
                    seq = msg.seq,
                    error = %err,
                    "malformed attachment cleanup payload rejected"
                );
                MessageResult::Reject(format!("malformed attachment cleanup payload: {err}"))
            }
        }
    }
}

#[cfg(test)]
#[path = "outbox_handlers_tests.rs"]
mod tests;
