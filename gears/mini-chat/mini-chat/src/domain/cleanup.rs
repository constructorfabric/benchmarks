//! Provider-side cleanup (DESIGN "Cleanup on Chat Deletion", "Attachment
//! Deletion" Phase 2, B.9.2): the logic of the attachment-cleanup and
//! chat-cleanup outbox handlers.
//!
//! The outbox adapters (`infra::outbox`) only translate [`CleanupOutcome`] into
//! the shared outbox's `MessageResult`.
//!
//! * Attachment message: acked without action when the chat is soft-deleted
//!   (the chat cleanup owns the files then); done without a call when no
//!   provider file exists; otherwise the primary file is deleted through the
//!   storage label of the message (2xx / 404 = done), then the secondary copy
//!   best effort. A failed delete counts an attempt on the row; the attempt
//!   that reaches `cleanup_worker.max_attempts` makes the row `failed` and the
//!   message is rejected (dead letter).
//! * Chat message: only for a soft-deleted chat; every attachment of the chat
//!   whose cleanup is `pending` is processed like above (a spent budget does
//!   not stop the handler); while one is still `pending` the message is retried
//!   and the vector store is left alone; then the vector store of the chat is
//!   deleted (2xx / 404 = its row is removed). A failing store delete keeps the
//!   row and retries until the delivery that reaches the limit.

use std::sync::Arc;

use serde::Deserialize;
use toolkit_db::DBProvider;
use toolkit_db::secure::TxConfig;
use toolkit_security::{AccessScope, SecurityContext};
use uuid::Uuid;

use crate::config::MiniChatConfig;
use crate::domain::attachments::{AttachmentCleanupEvent, cleanup_status};
use crate::domain::clock::now_utc;
use crate::domain::error::DomainError;
use crate::domain::ports::{
    AttachmentCleanupRepository, AttachmentLifecycleRepository, ChatRepository, StoragePort,
    StorageTarget, VectorStoreRepository,
};

/// The result of one delivery, as the outbox needs it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CleanupOutcome {
    /// Acknowledge the message.
    Done,
    /// Deliver it again later (outbox backoff).
    Retry,
    /// Dead-letter it, for this reason.
    Reject(String),
}

/// What happened to one attachment in one delivery.
enum Settled {
    /// Provider cleanup finished (or was not owed).
    Done,
    /// A failed attempt was counted; budget left.
    Pending,
    /// The retry budget is spent: the row is terminally `failed`.
    Failed,
}

/// The fields of the chat-cleanup payload the handler reads.
#[derive(Debug, Deserialize)]
struct ChatCleanupPayload {
    tenant_id: Uuid,
    chat_id: Uuid,
}

/// The identity of a cleanup call: a system caller of the message's tenant.
fn service_context(tenant_id: Uuid) -> Result<SecurityContext, DomainError> {
    SecurityContext::builder()
        .subject_id(Uuid::nil())
        .subject_type("service")
        .subject_tenant_id(tenant_id)
        .build()
        .map_err(|error| DomainError::internal(format!("cleanup security context: {error}")))
}

/// The files of one attachment.
struct Files<'a> {
    backend: &'a str,
    primary: Option<&'a str>,
    secondary: Option<&'a str>,
}

/// Runs the cleanup handlers.
pub struct CleanupService {
    db: Arc<DBProvider<DomainError>>,
    storage: Arc<dyn StoragePort>,
    config: Arc<MiniChatConfig>,
}

impl CleanupService {
    /// Creates the service.
    #[must_use]
    pub fn new<S: StoragePort + 'static>(
        db: Arc<DBProvider<DomainError>>,
        storage: Arc<S>,
        config: Arc<MiniChatConfig>,
    ) -> Self {
        Self {
            db,
            storage,
            config,
        }
    }

    /// Handles one attachment-cleanup message.
    pub async fn attachment(&self, payload: &[u8]) -> CleanupOutcome {
        let event: AttachmentCleanupEvent = match serde_json::from_slice(payload) {
            Ok(event) => event,
            Err(error) => {
                tracing::error!(%error, "attachment cleanup: corrupt payload rejected");
                return CleanupOutcome::Reject(format!(
                    "payload is not an attachment cleanup event: {error}"
                ));
            }
        };
        match self.attachment_event(&event).await {
            Ok(outcome) => outcome,
            Err(error) => {
                tracing::warn!(%error, attachment_id = %event.attachment_id,
                    "attachment cleanup hit a database error; retrying");
                CleanupOutcome::Retry
            }
        }
    }

    async fn attachment_event(
        &self,
        event: &AttachmentCleanupEvent,
    ) -> Result<CleanupOutcome, DomainError> {
        let scope = AccessScope::for_tenant(event.tenant_id);
        let conn = self.db.conn()?;
        // A soft-deleted chat: its cleanup owns every provider file now.
        if conn.chat_is_deleted(&scope, event.chat_id).await? != Some(false) {
            return Ok(CleanupOutcome::Done);
        }
        let row = conn
            .find_attachment(&scope, event.chat_id, event.attachment_id)
            .await?;
        if row.is_none_or(|row| row.cleanup_status.as_deref() != Some(cleanup_status::PENDING)) {
            // Already done / failed: a repeated delivery.
            return Ok(CleanupOutcome::Done);
        }
        let files = Files {
            backend: &event.storage_backend,
            primary: event.provider_file_id.as_deref(),
            secondary: event
                .secondary_ref
                .as_ref()
                .map(|secondary| secondary.file_id.as_str()),
        };
        let settled = self
            .settle(&scope, event.tenant_id, event.attachment_id, &files)
            .await?;
        Ok(match settled {
            Settled::Done => CleanupOutcome::Done,
            Settled::Pending => CleanupOutcome::Retry,
            Settled::Failed => CleanupOutcome::Reject(format!(
                "attachment cleanup: max attempts ({}) reached",
                self.config.cleanup_worker.max_attempts
            )),
        })
    }

    /// Handles one chat-cleanup message; `attempts` is the delivery count so far
    /// (`0` on the first delivery).
    pub async fn chat(&self, payload: &[u8], attempts: u32) -> CleanupOutcome {
        let parsed: ChatCleanupPayload = match serde_json::from_slice(payload) {
            Ok(parsed) => parsed,
            Err(error) => {
                tracing::error!(%error, "chat cleanup: corrupt payload rejected");
                return CleanupOutcome::Reject(format!(
                    "payload is not a chat cleanup message: {error}"
                ));
            }
        };
        match self.chat_message(&parsed, attempts).await {
            Ok(outcome) => outcome,
            Err(error) => {
                tracing::warn!(%error, chat_id = %parsed.chat_id,
                    "chat cleanup hit a database error; retrying");
                CleanupOutcome::Retry
            }
        }
    }

    async fn chat_message(
        &self,
        message: &ChatCleanupPayload,
        attempts: u32,
    ) -> Result<CleanupOutcome, DomainError> {
        let scope = AccessScope::for_tenant(message.tenant_id);
        let conn = self.db.conn()?;
        match conn.chat_is_deleted(&scope, message.chat_id).await? {
            Some(true) => {}
            Some(false) => {
                return Ok(CleanupOutcome::Reject(
                    "chat is not soft-deleted".to_owned(),
                ));
            }
            None => return Ok(CleanupOutcome::Reject("chat not found".to_owned())),
        }
        let rows = conn
            .pending_cleanup_attachments(&scope, message.chat_id)
            .await?;
        let mut pending = false;
        for row in &rows {
            let files = Files {
                backend: &row.storage_backend,
                primary: row.provider_file_id.as_deref(),
                secondary: row.secondary_file_id.as_deref(),
            };
            let settled = self
                .settle(&scope, message.tenant_id, row.id, &files)
                .await?;
            pending |= matches!(settled, Settled::Pending);
        }
        if pending {
            return Ok(CleanupOutcome::Retry);
        }
        self.delete_store(&scope, message, attempts).await
    }

    /// The vector store of the chat; its row is the durable marker of the work.
    async fn delete_store(
        &self,
        scope: &AccessScope,
        message: &ChatCleanupPayload,
        attempts: u32,
    ) -> Result<CleanupOutcome, DomainError> {
        let conn = self.db.conn()?;
        let Some(store) = conn.find_vector_store(scope, message.chat_id).await? else {
            return Ok(CleanupOutcome::Done);
        };
        if let Some(store_id) = &store.vector_store_id {
            let ctx = service_context(message.tenant_id)?;
            let target = StorageTarget {
                tenant_id: message.tenant_id,
                backend: store.provider.clone(),
            };
            if let Err(error) = self
                .storage
                .delete_vector_store(&ctx, &target, store_id)
                .await
            {
                tracing::warn!(%error, chat_id = %message.chat_id, "vector store delete failed");
                let limit = self.config.cleanup_worker.max_attempts;
                return Ok(if attempts.saturating_add(1) >= limit {
                    CleanupOutcome::Reject(format!(
                        "vector store delete: max attempts ({limit}) reached"
                    ))
                } else {
                    CleanupOutcome::Retry
                });
            }
        }
        conn.delete_vector_store_row(scope, message.chat_id).await?;
        Ok(CleanupOutcome::Done)
    }

    /// Deletes the provider files of one `pending` attachment and records the
    /// outcome on its row.
    async fn settle(
        &self,
        scope: &AccessScope,
        tenant_id: Uuid,
        id: Uuid,
        files: &Files<'_>,
    ) -> Result<Settled, DomainError> {
        match self.delete_files(tenant_id, files).await {
            Ok(()) => {
                self.db
                    .conn()?
                    .mark_cleanup_done(scope, id, now_utc())
                    .await?;
                Ok(Settled::Done)
            }
            Err(error) => {
                tracing::warn!(%error, attachment_id = %id, "provider file delete failed");
                self.record_failure(scope, id, &error.to_string()).await
            }
        }
    }

    /// Counts a failed attempt; the one that reaches the limit makes the row
    /// terminally `failed`. Both writes commit together.
    async fn record_failure(
        &self,
        scope: &AccessScope,
        id: Uuid,
        error: &str,
    ) -> Result<Settled, DomainError> {
        let limit = i32::try_from(self.config.cleanup_worker.max_attempts).unwrap_or(i32::MAX);
        let now = now_utc();
        let spent = self
            .db
            .db()
            .transaction_with_retry(TxConfig::default(), DomainError::db_cause, |tx| {
                let (scope, error) = (scope.clone(), error.to_owned());
                Box::pin(async move {
                    let Some(attempts) = tx.record_cleanup_attempt(&scope, id, &error, now).await?
                    else {
                        return Ok(None);
                    };
                    if attempts < limit {
                        return Ok(Some(false));
                    }
                    tx.mark_cleanup_failed(&scope, id, now).await?;
                    Ok(Some(true))
                })
            })
            .await?;
        Ok(match spent {
            // Somebody else finished the row meanwhile.
            None => Settled::Done,
            Some(false) => Settled::Pending,
            Some(true) => Settled::Failed,
        })
    }

    /// Primary file first (2xx / 404 = success), then the secondary copy best
    /// effort. No provider file: nothing to do.
    async fn delete_files(
        &self,
        tenant_id: Uuid,
        files: &Files<'_>,
    ) -> Result<(), crate::domain::ports::StorageError> {
        let Some(primary) = files.primary else {
            return Ok(());
        };
        let ctx =
            service_context(tenant_id).map_err(|error| crate::domain::ports::StorageError {
                transient: false,
                message: error.to_string(),
            })?;
        let target = StorageTarget {
            tenant_id,
            backend: files.backend.to_owned(),
        };
        self.storage.delete_file(&ctx, &target, primary).await?;
        if let Some(secondary) = files.secondary
            && let Err(error) = self.storage.delete_file(&ctx, &target, secondary).await
        {
            tracing::warn!(%error, "secondary provider file delete failed; ignored");
        }
        Ok(())
    }
}
