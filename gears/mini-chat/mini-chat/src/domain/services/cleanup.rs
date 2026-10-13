//! Cleanup of provider files and vector stores, driven by the outbox (spec
//! §13.1; DESIGN §3.6 "Cleanup on Chat Deletion", §4 "Attachment Deletion"
//! Phase 2, B.9.2).
//!
//! Two entry points, one per queue, each executing one delivery:
//! - [`CleanupService::cleanup_attachment`]: one attachment (user deletion,
//!   abandoned upload, failed indexing);
//! - [`CleanupService::cleanup_chat`]: every `pending` attachment of a
//!   soft-deleted chat, then the chat's vector store once all of them are
//!   terminal.
//!
//! The secondary (Anthropic) copy of an image is deleted best effort after the
//! primary file (a failure is only logged), on the upstream holding it: the
//! provider whose alias is the payload's `secondary_ref.upstream_alias` on
//! attachment cleanup, the chat model's Anthropic provider on chat cleanup.
//!
//! Provider `404` counts as success (the storage client folds it into `Ok`). A
//! failed provider delete is recorded on the row (`cleanup_attempts`,
//! `last_cleanup_error`, `cleanup_updated_at`); at `cleanup_worker.max_attempts`
//! the row becomes terminal `failed`. Metrics are not exported: the
//! `mini_chat_cleanup_*` events are log lines.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use toolkit_db::DBProvider;
use toolkit_db::secure::DBRunner;
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::config::{MiniChatConfig, ProviderKind};
use crate::domain::clock::now_utc;
use crate::domain::error::{DomainError, DomainResult};
use crate::domain::services::attachment::{has_secondary_copy, secondary_provider};
use crate::domain::services::model_catalog::ModelCatalogService;
use crate::infra::db::repos::attachment::cleanup_status;
use crate::infra::db::repos::{AttachmentRepo, ChatRepo, VectorStoreRepo};
use crate::infra::llm::anthropic_files::SECONDARY_PROVIDER_KIND;
use crate::infra::llm::{AnthropicFilesClient, ProviderResolver, RagClient, ResolvedProvider};
use crate::infra::outbox::payloads::{AttachmentCleanupPayload, ChatCleanupPayload};

/// Longest `last_cleanup_error` stored (characters).
const MAX_ERROR_CHARS: usize = 500;

/// Infrastructure of [`CleanupService`].
pub struct CleanupDeps {
    pub config: Arc<MiniChatConfig>,
    pub db: Arc<DBProvider<DomainError>>,
    pub providers: Arc<ProviderResolver>,
    /// Files / vector stores client over OAGW.
    pub rag: Arc<RagClient>,
    /// Anthropic Files client (only when an `anthropic_messages` entry exists).
    pub anthropic_files: Option<Arc<AnthropicFilesClient>>,
    /// Catalog lookup of a chat's model (provider holding secondary copies).
    pub models: Arc<ModelCatalogService>,
}

/// Result of one delivery, mapped to an outbox result by the handler.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CleanupOutcome {
    /// Nothing (more) to do.
    Done,
    /// Try again on a later delivery.
    Retry(String),
    /// Permanent failure (dead letter).
    Reject(String),
}

/// Outcome of the provider cleanup of one attachment.
#[derive(Debug, Clone, PartialEq, Eq)]
enum FileOutcome {
    /// Cleanup finished (`cleanup_status = 'done'`).
    Done,
    /// The delete failed; the attachment stays `pending`.
    Retry(String),
    /// The delete failed and the attempt budget is exhausted (`failed`).
    Failed(String),
}

/// What to delete for one attachment.
struct FileTarget<'a> {
    tenant_id: Uuid,
    attachment_id: Uuid,
    provider_file_id: Option<&'a str>,
    storage_backend: &'a str,
    secondary: Option<SecondaryTarget<'a>>,
}

/// The secondary (Anthropic) copy of an attachment.
#[derive(Clone, Copy)]
struct SecondaryTarget<'a> {
    provider_kind: &'a str,
    file_id: &'a str,
    holder: SecondaryHolder<'a>,
}

/// Where a secondary copy lives.
#[derive(Clone, Copy)]
enum SecondaryHolder<'a> {
    /// Upstream alias recorded at delete time (`secondary_ref.upstream_alias`).
    Alias(&'a str),
    /// Provider derived from the chat's model (`None`: not derivable).
    Provider(Option<&'a ResolvedProvider>),
}

/// Provider cleanup service behind the two cleanup outbox handlers.
pub struct CleanupService {
    max_attempts: u32,
    db: Arc<DBProvider<DomainError>>,
    providers: Arc<ProviderResolver>,
    rag: Arc<RagClient>,
    anthropic_files: Option<Arc<AnthropicFilesClient>>,
    models: Arc<ModelCatalogService>,
}

impl CleanupService {
    #[must_use]
    pub fn new(d: CleanupDeps) -> Self {
        Self {
            max_attempts: d.config.cleanup_worker.max_attempts,
            db: d.db,
            providers: d.providers,
            rag: d.rag,
            anthropic_files: d.anthropic_files,
            models: d.models,
        }
    }

    /// `cleanup_worker.max_attempts`.
    #[must_use]
    pub fn max_attempts(&self) -> u32 {
        self.max_attempts
    }

    /// One delivery of an attachment-cleanup message (DESIGN §4 "Attachment
    /// Deletion", Phase 2). A database error is a `Retry` without a limit.
    pub async fn cleanup_attachment(&self, p: &AttachmentCleanupPayload) -> CleanupOutcome {
        match self.attachment_inner(p).await {
            Ok(outcome) => outcome,
            Err(e) => {
                warn!(attachment_id = %p.attachment_id, error = %e, "attachment cleanup: database error; will retry");
                CleanupOutcome::Retry(format!("database error: {e}"))
            }
        }
    }

    /// One delivery of a chat-cleanup message; `delivery` is 1-based (all
    /// deliveries of the message count toward `cleanup_worker.max_attempts`).
    pub async fn cleanup_chat(&self, p: &ChatCleanupPayload, delivery: u32) -> CleanupOutcome {
        match self.chat_inner(p, delivery).await {
            Ok(outcome) => outcome,
            Err(e) => {
                warn!(chat_id = %p.chat_id, error = %e, "chat cleanup: database error; will retry");
                CleanupOutcome::Retry(format!("database error: {e}"))
            }
        }
    }

    async fn attachment_inner(&self, p: &AttachmentCleanupPayload) -> DomainResult<CleanupOutcome> {
        let conn = self.db.conn()?;
        let chat = ChatRepo::find_any(&conn, p.tenant_id, p.chat_id).await?;
        if chat.is_some_and(|c| c.deleted_at.is_some()) {
            // Ownership moved to the chat cleanup when the chat was soft-deleted.
            debug!(chat_id = %p.chat_id, attachment_id = %p.attachment_id, "attachment cleanup skipped: chat is soft-deleted");
            return Ok(CleanupOutcome::Done);
        }
        let Some(row) =
            AttachmentRepo::find_one(&conn, p.tenant_id, p.chat_id, p.attachment_id).await?
        else {
            warn!(attachment_id = %p.attachment_id, "attachment cleanup: attachment not found");
            return Ok(CleanupOutcome::Reject("attachment not found".to_owned()));
        };
        if row.cleanup_status.as_deref() == Some(cleanup_status::DONE) {
            return Ok(CleanupOutcome::Done);
        }
        let target = FileTarget {
            tenant_id: p.tenant_id,
            attachment_id: p.attachment_id,
            provider_file_id: p.provider_file_id.as_deref(),
            storage_backend: &p.storage_backend,
            secondary: p.secondary_ref.as_ref().map(|s| SecondaryTarget {
                provider_kind: s.provider_kind.as_str(),
                file_id: s.file_id.as_str(),
                holder: SecondaryHolder::Alias(s.upstream_alias.as_str()),
            }),
        };
        Ok(match self.clean_file(&conn, &target).await? {
            FileOutcome::Done => CleanupOutcome::Done,
            FileOutcome::Retry(reason) => CleanupOutcome::Retry(reason),
            FileOutcome::Failed(_) => CleanupOutcome::Reject(format!(
                "provider file delete: max attempts ({}) reached",
                self.max_attempts
            )),
        })
    }

    async fn chat_inner(
        &self,
        p: &ChatCleanupPayload,
        delivery: u32,
    ) -> DomainResult<CleanupOutcome> {
        let conn = self.db.conn()?;
        let chat = ChatRepo::find_any(&conn, p.tenant_id, p.chat_id)
            .await?
            .filter(|c| c.deleted_at.is_some());
        let Some(chat) = chat else {
            warn!(chat_id = %p.chat_id, "chat cleanup rejected: chat is not soft-deleted");
            return Ok(CleanupOutcome::Reject(
                "chat is not soft-deleted".to_owned(),
            ));
        };

        let pending = AttachmentRepo::with_cleanup_status(
            &conn,
            p.tenant_id,
            p.chat_id,
            cleanup_status::PENDING,
        )
        .await?;
        let holder = if self.anthropic_files.is_some() && pending.iter().any(has_secondary_copy) {
            secondary_provider(&self.models, &self.providers, &chat).await
        } else {
            None
        };
        for a in &pending {
            let secondary = a
                .secondary_file_id
                .as_deref()
                .zip(a.secondary_provider_kind.as_deref())
                .map(|(file_id, provider_kind)| SecondaryTarget {
                    provider_kind,
                    file_id,
                    holder: SecondaryHolder::Provider(holder.as_ref()),
                });
            let target = FileTarget {
                tenant_id: p.tenant_id,
                attachment_id: a.id,
                provider_file_id: a.provider_file_id.as_deref(),
                storage_backend: &a.storage_backend,
                secondary,
            };
            // A failed attachment is terminal: the chat cleanup continues with the rest.
            self.clean_file(&conn, &target).await?;
        }

        let still_pending = AttachmentRepo::with_cleanup_status(
            &conn,
            p.tenant_id,
            p.chat_id,
            cleanup_status::PENDING,
        )
        .await?;
        if !still_pending.is_empty() {
            return Ok(CleanupOutcome::Retry(format!(
                "{} attachment cleanup(s) still pending",
                still_pending.len()
            )));
        }
        let failed = AttachmentRepo::with_cleanup_status(
            &conn,
            p.tenant_id,
            p.chat_id,
            cleanup_status::FAILED,
        )
        .await?
        .len();
        self.delete_vector_store(&conn, p, failed, delivery).await
    }

    /// Delete the chat's vector store (all attachments are terminal) and its row.
    async fn delete_vector_store(
        &self,
        conn: &impl DBRunner,
        p: &ChatCleanupPayload,
        failed_attachments: usize,
        delivery: u32,
    ) -> DomainResult<CleanupOutcome> {
        let Some(row) = VectorStoreRepo::find(conn, p.tenant_id, p.chat_id).await? else {
            return Ok(CleanupOutcome::Done);
        };
        let Some(vector_store_id) = row.vector_store_id.as_deref() else {
            // Creation placeholder: no provider store exists, nothing to delete.
            return Ok(
                if VectorStoreRepo::delete_placeholder(conn, p.tenant_id, p.chat_id, row.id).await?
                {
                    CleanupOutcome::Done
                } else {
                    CleanupOutcome::Retry("vector store row changed".to_owned())
                },
            );
        };
        if failed_attachments > 0 {
            // mini_chat_cleanup_vector_store_with_failed_attachments_total
            warn!(
                chat_id = %p.chat_id,
                failed_attachments,
                "deleting the vector store although attachment cleanup failed for some files"
            );
        }
        let result = self
            .delete_store_at(&row.provider, p.tenant_id, vector_store_id)
            .await;
        match result {
            Ok(()) => {
                VectorStoreRepo::delete_row(conn, p.tenant_id, p.chat_id, row.id).await?;
                // mini_chat_cleanup_completed_total{resource_type="vector_store"}
                info!(chat_id = %p.chat_id, "vector store deleted");
                Ok(CleanupOutcome::Done)
            }
            Err(error) => Ok(self.vector_store_failure(p.chat_id, delivery, &error)),
        }
    }

    /// Outcome of a failed vector-store delete: retry, or reject on the delivery
    /// that reaches `max_attempts` (the row is kept for a dead-letter replay).
    fn vector_store_failure(&self, chat_id: Uuid, delivery: u32, error: &str) -> CleanupOutcome {
        // mini_chat_cleanup_retry_total{resource_type="vector_store",reason="vector_store_delete_failed"}
        warn!(%chat_id, delivery, error, "vector store delete failed");
        if delivery >= self.max_attempts {
            // mini_chat_cleanup_failed_total{resource_type="vector_store"}
            CleanupOutcome::Reject(format!(
                "vector store delete: max attempts ({}) reached",
                self.max_attempts
            ))
        } else {
            CleanupOutcome::Retry(format!("vector store delete failed: {error}"))
        }
    }

    /// `DELETE` the provider file `file_id` on the provider behind `backend`.
    async fn delete_file_at(
        &self,
        backend: &str,
        tenant_id: Uuid,
        file_id: &str,
    ) -> Result<(), String> {
        let provider = self
            .providers
            .resolve_storage(backend, tenant_id)
            .map_err(|e| e.to_string())?;
        self.rag
            .delete_file(&provider, file_id)
            .await
            .map_err(|e| e.to_string())
    }

    /// `DELETE` the provider vector store `vector_store_id` on the provider behind `backend`.
    async fn delete_store_at(
        &self,
        backend: &str,
        tenant_id: Uuid,
        vector_store_id: &str,
    ) -> Result<(), String> {
        let provider = self
            .providers
            .resolve_storage(backend, tenant_id)
            .map_err(|e| e.to_string())?;
        self.rag
            .delete_vector_store(&provider, vector_store_id)
            .await
            .map_err(|e| e.to_string())
    }

    /// Delete the provider file(s) of one attachment and record the outcome on its row.
    async fn clean_file(
        &self,
        conn: &impl DBRunner,
        t: &FileTarget<'_>,
    ) -> DomainResult<FileOutcome> {
        let now = now_utc();
        let Some(file_id) = t.provider_file_id else {
            // The upload never reached the provider.
            AttachmentRepo::mark_cleanup_done(conn, t.tenant_id, t.attachment_id, now).await?;
            return Ok(FileOutcome::Done);
        };
        let result = self
            .delete_file_at(t.storage_backend, t.tenant_id, file_id)
            .await;
        match result {
            Ok(()) => {
                if let Some(secondary) = t.secondary {
                    self.delete_secondary(t, secondary).await;
                }
                AttachmentRepo::mark_cleanup_done(conn, t.tenant_id, t.attachment_id, now).await?;
                // mini_chat_cleanup_completed_total{resource_type="file"}
                debug!(attachment_id = %t.attachment_id, "provider file deleted");
                Ok(FileOutcome::Done)
            }
            Err(error) => self.record_file_failure(conn, t, &error, now).await,
        }
    }

    /// Record a failed provider delete on the attachment row.
    async fn record_file_failure(
        &self,
        conn: &impl DBRunner,
        t: &FileTarget<'_>,
        error: &str,
        now: DateTime<Utc>,
    ) -> DomainResult<FileOutcome> {
        let error: String = error.chars().take(MAX_ERROR_CHARS).collect();
        let max = i32::try_from(self.max_attempts).unwrap_or(i32::MAX);
        let recorded = AttachmentRepo::record_cleanup_failure(
            conn,
            t.tenant_id,
            t.attachment_id,
            &error,
            max,
            now,
        )
        .await?;
        Ok(match recorded {
            // Finished by another path meanwhile.
            None => FileOutcome::Done,
            Some(f) if f.failed => {
                // mini_chat_cleanup_failed_total{resource_type="file"}
                warn!(attachment_id = %t.attachment_id, attempts = f.attempts, %error, "provider file cleanup failed terminally");
                FileOutcome::Failed(error)
            }
            Some(f) => {
                // mini_chat_cleanup_retry_total{resource_type="file",reason="provider_error"}
                warn!(attachment_id = %t.attachment_id, attempts = f.attempts, %error, "provider file delete failed");
                FileOutcome::Retry(error)
            }
        })
    }
}

impl CleanupService {
    /// Delete the secondary (Anthropic) copy, best effort: a failure is logged
    /// and does not change the attachment's cleanup outcome.
    #[allow(clippy::cognitive_complexity)] // tracing macros inflate the score
    async fn delete_secondary(&self, t: &FileTarget<'_>, s: SecondaryTarget<'_>) {
        let Some((client, provider)) = self.secondary_target(t.tenant_id, s) else {
            // mini_chat_secondary_cleanup_skipped{provider_kind}
            warn!(
                attachment_id = %t.attachment_id,
                provider_kind = s.provider_kind,
                secondary_file_id = s.file_id,
                "secondary file cleanup skipped: no client or upstream for the secondary provider"
            );
            return;
        };
        match client.delete(&provider, s.file_id).await {
            Ok(()) => debug!(attachment_id = %t.attachment_id, "secondary file deleted"),
            Err(e) => warn!(
                attachment_id = %t.attachment_id,
                secondary_file_id = s.file_id,
                error = %e,
                "secondary file delete failed (best effort, not retried)"
            ),
        }
    }

    /// Client and Anthropic provider holding a secondary copy; `None` when
    /// either is missing.
    fn secondary_target(
        &self,
        tenant_id: Uuid,
        s: SecondaryTarget<'_>,
    ) -> Option<(&AnthropicFilesClient, ResolvedProvider)> {
        if s.provider_kind != SECONDARY_PROVIDER_KIND {
            return None;
        }
        let client = self.anthropic_files.as_deref()?;
        let provider = match s.holder {
            SecondaryHolder::Provider(p) => p?.clone(),
            SecondaryHolder::Alias(alias) => self.provider_by_alias(tenant_id, alias)?,
        };
        Some((client, provider))
    }

    /// The Anthropic provider whose upstream alias is `alias`; when no entry
    /// has it any more (config changed), the lowest-id Anthropic entry routed
    /// by the recorded alias, with a warning.
    fn provider_by_alias(&self, tenant_id: Uuid, alias: &str) -> Option<ResolvedProvider> {
        let kind = ProviderKind::AnthropicMessages;
        if let Some(p) = self.providers.resolve_by_alias(kind, alias, tenant_id) {
            return Some(p);
        }
        warn!(
            alias,
            "no Anthropic provider has the recorded upstream alias; using the first one"
        );
        let mut p = self.providers.resolve_kind(kind, tenant_id)?;
        alias.clone_into(&mut p.alias);
        Some(p)
    }
}
