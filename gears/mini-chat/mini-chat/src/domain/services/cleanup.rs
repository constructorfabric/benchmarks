//! Provider cleanup driven by the shared outbox (S§10.1): deleted or failed
//! attachments (D "Attachment Deletion" Phase 2) and soft-deleted chats
//! (D "Cleanup on Chat Deletion").
//!
//! Provider deletes are idempotent (2xx and 404 are success, any other status
//! is a failed attempt). A failed file delete increments the row's
//! `cleanup_attempts`; reaching `cleanup_worker.max_attempts` makes the row
//! terminal `failed`. Database errors return `Retry` without touching the
//! counters. Secondary (Anthropic) copies are deleted best effort through
//! the Anthropic Files client; without the client or an Anthropic upstream
//! alias the delete is skipped (`secondary_cleanup_skipped`) and logged.

use std::sync::Arc;

use toolkit_db::DBProvider;
use toolkit_db::secure::AccessScope;
use tracing::{info, warn};
use uuid::Uuid;

use crate::config::ProviderKind;
use crate::domain::clock::Clock;
use crate::domain::error::DomainError;
use crate::domain::ports::{HandlerOutcome, SecondaryFilesPort, StoragePort};
use crate::domain::services::ModelResolver;
use crate::infra::db::entity::{attachment, chat};
use crate::infra::db::repos::{AttachmentRepo, ChatRepo, VectorStoreRepo};
use crate::infra::llm::provider_resolver::ProviderResolver;
use crate::infra::metrics::MiniChatMetrics;
use crate::infra::outbox::payloads::{AttachmentCleanupPayload, ChatCleanupPayload};

/// Outcome of recording one failed provider file delete.
enum Failure {
    /// The row stays `pending`; the delete is retried.
    Pending,
    /// The row became terminal `failed`.
    Terminal,
    /// The row changed concurrently; nothing was recorded.
    Conflict,
}

/// Provider file / vector-store cleanup (outbox handler side).
pub struct CleanupService {
    db: Arc<DBProvider<DomainError>>,
    clock: Arc<dyn Clock>,
    storage: Arc<dyn StoragePort>,
    providers: Arc<ProviderResolver>,
    max_attempts: u32,
    metrics: Arc<MiniChatMetrics>,
    secondary: Option<Arc<dyn SecondaryFilesPort>>,
    /// Resolves a chat's model → provider (alias of its secondary copies).
    models: Option<Arc<ModelResolver>>,
}

impl CleanupService {
    /// `max_attempts` is `cleanup_worker.max_attempts`.
    #[must_use]
    pub fn new(
        db: Arc<DBProvider<DomainError>>,
        clock: Arc<dyn Clock>,
        storage: Arc<dyn StoragePort>,
        providers: Arc<ProviderResolver>,
        max_attempts: u32,
        metrics: Arc<MiniChatMetrics>,
    ) -> Self {
        Self {
            db,
            clock,
            storage,
            providers,
            max_attempts,
            metrics,
            secondary: None,
            models: None,
        }
    }

    /// Delete secondary (Anthropic) copies through `secondary`; the chat
    /// cleanup resolves their upstream alias from the chat's model with
    /// `models` (as the upload did).
    #[must_use]
    pub fn with_secondary(
        mut self,
        secondary: Option<Arc<dyn SecondaryFilesPort>>,
        models: Arc<ModelResolver>,
    ) -> Self {
        self.secondary = secondary;
        self.models = Some(models);
        self
    }

    /// One delivery of an attachment cleanup message (`attempt` is 1 on the
    /// first delivery; the retry budget is the row's `cleanup_attempts`).
    pub async fn process_attachment_cleanup(
        &self,
        payload: &AttachmentCleanupPayload,
        attempt: u32,
    ) -> HandlerOutcome {
        self.attachment_cleanup(payload).await.unwrap_or_else(|e| {
            warn!(
                attachment_id = %payload.attachment_id,
                attempt,
                error = %e,
                "attachment cleanup: database error; retrying"
            );
            HandlerOutcome::Retry
        })
    }

    /// One delivery of a chat cleanup message (`attempt` is 1 on the first
    /// delivery; it bounds a failing vector-store delete).
    pub async fn process_chat_cleanup(
        &self,
        payload: &ChatCleanupPayload,
        attempt: u32,
    ) -> HandlerOutcome {
        self.chat_cleanup(payload, attempt)
            .await
            .unwrap_or_else(|e| {
                warn!(
                    chat_id = %payload.chat_id,
                    attempt,
                    error = %e,
                    "chat cleanup: database error; retrying"
                );
                HandlerOutcome::Retry
            })
    }

    async fn attachment_cleanup(
        &self,
        p: &AttachmentCleanupPayload,
    ) -> Result<HandlerOutcome, DomainError> {
        let scope = AccessScope::for_tenant(p.tenant_id);
        let conn = self.db.conn()?;
        let Some(chat) = ChatRepo.find_by_id(&conn, &scope, p.chat_id).await? else {
            return Ok(HandlerOutcome::Reject("chat not found".to_owned()));
        };
        if chat.deleted_at.is_some() {
            // The chat cleanup owns the files of a soft-deleted chat.
            info!(attachment_id = %p.attachment_id, "attachment cleanup: chat deleted; skipped");
            return Ok(HandlerOutcome::Ok);
        }
        let Some(row) = AttachmentRepo
            .find_by_id(&conn, &scope, p.attachment_id)
            .await?
            .filter(|a| a.chat_id == p.chat_id)
        else {
            return Ok(HandlerOutcome::Reject("attachment not found".to_owned()));
        };
        if row.cleanup_status.as_deref() != Some("pending") {
            // Not pending: already terminal (a redelivery).
            return Ok(HandlerOutcome::Ok);
        }
        let Some(file_id) = p.provider_file_id.as_deref() else {
            self.mark_done(&scope, &row).await?;
            return Ok(HandlerOutcome::Ok);
        };
        match self
            .delete_file(&p.storage_backend, p.tenant_id, file_id)
            .await
        {
            Ok(()) => {
                self.metrics.cleanup_completed("file");
                if let Some(secondary) = &p.secondary_ref {
                    self.delete_secondary(
                        row.id,
                        &secondary.provider_kind,
                        Some(&secondary.upstream_alias),
                        &secondary.file_id,
                    )
                    .await;
                }
                self.mark_done(&scope, &row).await?;
                Ok(HandlerOutcome::Ok)
            }
            Err(error) => Ok(match self.record_failure(&scope, &row, &error).await? {
                Failure::Pending | Failure::Conflict => HandlerOutcome::Retry,
                Failure::Terminal => HandlerOutcome::Reject(format!(
                    "attachment cleanup: max attempts ({}) reached",
                    self.max_attempts
                )),
            }),
        }
    }

    async fn chat_cleanup(
        &self,
        p: &ChatCleanupPayload,
        attempt: u32,
    ) -> Result<HandlerOutcome, DomainError> {
        let scope = AccessScope::for_tenant(p.tenant_id);
        let conn = self.db.conn()?;
        let Some(chat) = ChatRepo.find_by_id(&conn, &scope, p.chat_id).await? else {
            return Ok(HandlerOutcome::Reject("chat not found".to_owned()));
        };
        if chat.deleted_at.is_none() {
            return Ok(HandlerOutcome::Reject(
                "chat is not soft-deleted".to_owned(),
            ));
        }

        let still_pending = self.clean_chat_attachments(&scope, &chat).await?;
        if still_pending > 0 {
            // The vector store waits until every attachment is terminal.
            info!(chat_id = %chat.id, still_pending, attempt, "chat cleanup: attachments pending; retrying");
            return Ok(HandlerOutcome::Retry);
        }
        self.clean_vector_store(&scope, chat.id, attempt).await
    }

    /// Clean every `pending` attachment of a soft-deleted chat; returns how
    /// many are still `pending`.
    async fn clean_chat_attachments(
        &self,
        scope: &AccessScope,
        chat: &chat::Model,
    ) -> Result<usize, DomainError> {
        let pending = {
            let conn = self.db.conn()?;
            AttachmentRepo
                .list_by_cleanup_status(&conn, scope, chat.id, "pending")
                .await?
        };
        let alias = if pending.iter().any(|a| a.secondary_file_id.is_some()) {
            self.secondary_alias(chat).await
        } else {
            None
        };
        let mut still_pending = 0_usize;
        for row in &pending {
            if !self
                .clean_chat_attachment(scope, row, alias.as_deref())
                .await?
            {
                still_pending += 1;
            }
        }
        Ok(still_pending)
    }

    /// Delete the chat's vector store (all attachments terminal) and its
    /// `chat_vector_stores` row; a failed delete retries until delivery
    /// `max_attempts`, which rejects and keeps the row.
    async fn clean_vector_store(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
        attempt: u32,
    ) -> Result<HandlerOutcome, DomainError> {
        let conn = self.db.conn()?;
        let Some(store) = VectorStoreRepo.find_by_chat(&conn, scope, chat_id).await? else {
            return Ok(HandlerOutcome::Ok);
        };
        let failed = AttachmentRepo
            .list_by_cleanup_status(&conn, scope, chat_id, "failed")
            .await?
            .len();
        if failed > 0 {
            warn_failed_attachments(chat_id, failed);
        }
        if let Some(vs) = store.vector_store_id.as_deref()
            && let Err(error) = self
                .delete_vector_store(&store.provider, store.tenant_id, vs)
                .await
        {
            return Ok(self.vector_store_failure(chat_id, attempt, &error));
        }
        if store.vector_store_id.is_some() {
            self.metrics.cleanup_completed("vector_store");
            if failed > 0 {
                self.metrics.cleanup_vector_store_with_failed_attachments();
            }
        }
        VectorStoreRepo
            .delete_row(&conn, scope, chat_id, store.id)
            .await?;
        info!(%chat_id, "chat cleanup: provider resources deleted");
        Ok(HandlerOutcome::Ok)
    }

    /// Outcome of a failed vector-store delete on delivery `attempt`.
    fn vector_store_failure(&self, chat_id: Uuid, attempt: u32, error: &str) -> HandlerOutcome {
        warn!(%chat_id, attempt, error, "chat cleanup: vector store delete failed");
        if attempt >= self.max_attempts {
            self.metrics.cleanup_failed("vector_store");
            HandlerOutcome::Reject(format!(
                "vector store delete: max attempts ({}) reached",
                self.max_attempts
            ))
        } else {
            self.metrics
                .cleanup_retry("vector_store", "vector_store_delete_failed");
            HandlerOutcome::Retry
        }
    }

    /// Clean one `pending` attachment of a soft-deleted chat; `true` when the
    /// row is now terminal (`done` / `failed`).
    async fn clean_chat_attachment(
        &self,
        scope: &AccessScope,
        row: &attachment::Model,
        secondary_alias: Option<&str>,
    ) -> Result<bool, DomainError> {
        let Some(file_id) = row.provider_file_id.as_deref() else {
            self.mark_done(scope, row).await?;
            return Ok(true);
        };
        match self
            .delete_file(&row.storage_backend, row.tenant_id, file_id)
            .await
        {
            Ok(()) => {
                self.metrics.cleanup_completed("file");
                if let (Some(file_id), Some(kind)) = (
                    row.secondary_file_id.as_deref(),
                    row.secondary_provider_kind.as_deref(),
                ) {
                    self.delete_secondary(row.id, kind, secondary_alias, file_id)
                        .await;
                }
                self.mark_done(scope, row).await?;
                Ok(true)
            }
            Err(error) => Ok(matches!(
                self.record_failure(scope, row, &error).await?,
                Failure::Terminal
            )),
        }
    }

    /// Alias of the chat model's provider when it is `anthropic_messages`
    /// (resolved like the upload of the secondary copies); `None` otherwise.
    async fn secondary_alias(&self, chat: &chat::Model) -> Option<String> {
        let models = self.models.as_ref()?;
        match models.resolve_chat_model(chat.user_id, &chat.model).await {
            Ok((_, model)) => self.providers.alias_if_kind(
                &model.provider_id,
                ProviderKind::AnthropicMessages,
                chat.tenant_id,
            ),
            Err(e) => {
                warn!(chat_id = %chat.id, error = %e, "chat model not resolved for the secondary file cleanup");
                None
            }
        }
    }

    async fn mark_done(
        &self,
        scope: &AccessScope,
        row: &attachment::Model,
    ) -> Result<(), DomainError> {
        let conn = self.db.conn()?;
        AttachmentRepo
            .mark_cleanup_done(&conn, scope, row.chat_id, row.id, self.clock.now())
            .await?;
        Ok(())
    }

    /// Count one failed provider delete of `row` (CAS on its attempts).
    async fn record_failure(
        &self,
        scope: &AccessScope,
        row: &attachment::Model,
        error: &str,
    ) -> Result<Failure, DomainError> {
        let next = i64::from(row.cleanup_attempts) + 1;
        let terminal = next >= i64::from(self.max_attempts);
        let conn = self.db.conn()?;
        let n = AttachmentRepo
            .record_cleanup_failure(
                &conn,
                scope,
                row.chat_id,
                row.id,
                row.cleanup_attempts,
                error,
                terminal,
                self.clock.now(),
            )
            .await?;
        warn!(
            attachment_id = %row.id,
            cleanup_attempts = next,
            terminal,
            error,
            "provider file delete failed"
        );
        Ok(match (n, terminal) {
            (0, _) => Failure::Conflict,
            (_, true) => {
                self.metrics.cleanup_failed("file");
                Failure::Terminal
            }
            (_, false) => {
                self.metrics.cleanup_retry("file", "provider_error");
                Failure::Pending
            }
        })
    }

    /// Delete a secondary copy (best effort: a failure is logged). Without
    /// the Anthropic Files client or an upstream alias the delete is skipped
    /// (`secondary_cleanup_skipped`).
    async fn delete_secondary(
        &self,
        attachment_id: Uuid,
        provider_kind: &str,
        alias: Option<&str>,
        secondary_file_id: &str,
    ) {
        let (Some(files), Some(alias)) = (&self.secondary, alias) else {
            self.metrics.secondary_cleanup_skipped(provider_kind);
            warn!(
                %attachment_id,
                provider_kind,
                secondary_file_id,
                "secondary file cleanup skipped: no client or upstream alias for the provider kind"
            );
            return;
        };
        if let Err(error) = files.delete(alias, secondary_file_id).await {
            warn!(
                %attachment_id,
                provider_kind,
                secondary_file_id,
                %error,
                "secondary file delete failed (not retried)"
            );
        }
    }

    async fn delete_file(
        &self,
        storage_backend: &str,
        tenant_id: Uuid,
        file_id: &str,
    ) -> Result<(), String> {
        let target = self
            .providers
            .resolve_storage_backend(storage_backend, tenant_id)
            .map_err(|e| e.to_string())?;
        self.storage
            .delete_file(&target, file_id)
            .await
            .map_err(|e| e.to_string())
    }

    async fn delete_vector_store(
        &self,
        storage_backend: &str,
        tenant_id: Uuid,
        vector_store_id: &str,
    ) -> Result<(), String> {
        let target = self
            .providers
            .resolve_storage_backend(storage_backend, tenant_id)
            .map_err(|e| e.to_string())?;
        self.storage
            .delete_vector_store(&target, vector_store_id)
            .await
            .map_err(|e| e.to_string())
    }
}

/// The vector store is deleted although `failed` attachments keep provider
/// file debt (`cleanup_vector_store_with_failed_attachments`, recorded after
/// the delete).
fn warn_failed_attachments(chat_id: Uuid, failed: usize) {
    warn!(
        %chat_id,
        failed,
        "chat cleanup: deleting the vector store with failed attachment cleanups"
    );
}
