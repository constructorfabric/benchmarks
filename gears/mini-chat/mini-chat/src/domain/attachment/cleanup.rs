//! The attachment cleanup handler (DESIGN "Attachment Deletion" phase 2 and "Attachment cleanup
//! state machine"): deletes the provider file of a deleted, abandoned or failed-indexing
//! attachment and records the outcome on the row.

use std::sync::Arc;

use toolkit_security::SecurityContext;
use tracing::{debug, error, info, warn};
use uuid::Uuid;

use super::model::PROVIDER_KIND_ANTHROPIC;
use super::ports::{CleanupRepository, StorageGateway};
use crate::domain::cleanup::{AttachmentCleanupPayload, SecondaryRef};
use crate::domain::clock;
use crate::domain::error::DomainError;
use crate::domain::metrics::Metrics;
use crate::domain::summary::SYSTEM_SUBJECT_ID;

/// The `resource_type` label of file cleanups.
pub(super) const RESOURCE_FILE: &str = "file";

/// The outcome of one delivery of a cleanup message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CleanupResult {
    /// Done, or nothing to do: the message is acknowledged.
    Done,
    /// Asks the shared outbox to deliver the message again later.
    Retry,
    /// The message is dead-lettered with the reason.
    Reject(String),
}

/// The security context of background cleanup: the system identity inside the tenant of the
/// message. It authenticates the OAGW call only; every database query of the cleanup carries the
/// tenant of the message.
pub(super) fn system_context(tenant_id: Uuid) -> Result<SecurityContext, DomainError> {
    SecurityContext::builder()
        .subject_id(SYSTEM_SUBJECT_ID)
        .subject_tenant_id(tenant_id)
        .token_scopes(vec!["*".to_owned()])
        .build()
        .map_err(|err| DomainError::internal(format!("the system security context: {err}")))
}

/// The attachment cleanup handler.
pub struct AttachmentCleanup {
    repo: Arc<dyn CleanupRepository>,
    storage: Arc<dyn StorageGateway>,
    metrics: Arc<dyn Metrics>,
    max_attempts: u32,
}

impl AttachmentCleanup {
    /// Creates the handler; `max_attempts` is `cleanup_worker.max_attempts`.
    #[must_use]
    pub fn new(
        repo: Arc<dyn CleanupRepository>,
        storage: Arc<dyn StorageGateway>,
        metrics: Arc<dyn Metrics>,
        max_attempts: u32,
    ) -> Self {
        Self {
            repo,
            storage,
            metrics,
            max_attempts,
        }
    }

    /// Handles one message. An infrastructure failure (database) asks for a retry without counting
    /// a provider attempt.
    pub async fn handle(&self, payload: &AttachmentCleanupPayload) -> CleanupResult {
        match self.run(payload).await {
            Ok(result) => result,
            Err(err) => {
                error!(attachment_id = %payload.attachment_id, error = %err, "the attachment cleanup hit an infrastructure failure");
                CleanupResult::Retry
            }
        }
    }

    async fn run(&self, payload: &AttachmentCleanupPayload) -> Result<CleanupResult, DomainError> {
        if self
            .repo
            .chat_is_deleted(payload.tenant_id, payload.chat_id)
            .await?
            && self
                .repo
                .cleanup_status(payload.tenant_id, payload.attachment_id)
                .await?
                .is_some()
        {
            // The row has a cleanup owner (the chat deletion marks every row and its cleanup reads
            // them all): the chat cleanup deletes the provider files. A row without an owner is
            // nobody else's, so it is cleaned here.
            debug!(attachment_id = %payload.attachment_id, "the chat is deleted; chat cleanup owns the provider files");
            return Ok(CleanupResult::Done);
        }
        let Some(file_id) = payload.provider_file_id.as_deref() else {
            self.finish(payload).await?;
            return Ok(CleanupResult::Done);
        };
        match self.delete_primary(payload, file_id).await {
            Ok(()) => {
                if let Some(secondary) = &payload.secondary_ref {
                    self.delete_secondary(payload.tenant_id, secondary).await;
                }
                self.finish(payload).await?;
                self.metrics.cleanup_completed(RESOURCE_FILE);
                Ok(CleanupResult::Done)
            }
            Err(cause) => self.record_failure(payload, &cause).await,
        }
    }

    async fn finish(&self, payload: &AttachmentCleanupPayload) -> Result<(), DomainError> {
        self.repo
            .mark_cleanup_done(payload.tenant_id, payload.attachment_id, clock::now())
            .await
    }

    /// Deletes the primary file; the error is the text recorded as `last_cleanup_error`.
    async fn delete_primary(
        &self,
        payload: &AttachmentCleanupPayload,
        file_id: &str,
    ) -> Result<(), String> {
        let ctx = system_context(payload.tenant_id).map_err(|err| err.to_string())?;
        let route = self
            .storage
            .route_for_backend(payload.tenant_id, &payload.storage_backend)
            .map_err(|err| err.to_string())?;
        self.storage
            .delete_file(&ctx, &route, file_id)
            .await
            .map_err(|err| err.to_string())
    }

    /// Deletes the secondary copy best effort: a failure is logged and counted, never blocks.
    async fn delete_secondary(&self, tenant_id: Uuid, secondary: &SecondaryRef) {
        let outcome = match system_context(tenant_id) {
            Ok(ctx) => self
                .storage
                .delete_secondary(&ctx, &secondary.upstream_alias, &secondary.file_id)
                .await
                .map_err(|err| err.to_string()),
            Err(err) => Err(err.to_string()),
        };
        if let Err(cause) = outcome {
            warn!(error = %cause, "the secondary file could not be deleted");
            self.metrics
                .secondary_cleanup_skipped(PROVIDER_KIND_ANTHROPIC);
        }
    }

    /// A failed provider delete: counts the attempt, stays `pending` and retries, or reaches the
    /// attempt limit and dead-letters the message.
    async fn record_failure(
        &self,
        payload: &AttachmentCleanupPayload,
        cause: &str,
    ) -> Result<CleanupResult, DomainError> {
        let failure = self
            .repo
            .record_cleanup_failure(
                payload.tenant_id,
                payload.attachment_id,
                cause,
                self.max_attempts,
                clock::now(),
            )
            .await?;
        if failure.terminal {
            error!(attachment_id = %payload.attachment_id, attempts = failure.attempts, "the provider file could not be deleted; giving up");
            self.metrics.cleanup_failed(RESOURCE_FILE);
            Ok(CleanupResult::Reject(format!(
                "provider file delete: max attempts ({}) reached: {cause}",
                self.max_attempts
            )))
        } else {
            info!(attachment_id = %payload.attachment_id, attempts = failure.attempts, "the provider file delete failed; retrying later");
            self.metrics.cleanup_retry(RESOURCE_FILE, "provider_error");
            Ok(CleanupResult::Retry)
        }
    }
}
