//! Attachments (DESIGN "Upload Attachment", "Get Attachment", "Attachment Deletion", "File
//! Upload"): upload with indexing and thumbnails, metadata, deletion.

use std::sync::Arc;

use opentelemetry::KeyValue;
use time::OffsetDateTime;
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;
use toolkit_db::DBProvider;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use toolkit_db::secure::AccessScope;

use crate::config::MiniChatConfig;
use crate::domain::authz::{Authz, ChatAction};
use crate::domain::error::DomainError;
use crate::domain::message_service::Thumbnail;
use crate::domain::model_service::ModelService;
use crate::infra::db::entity::{attachments, chats};
use crate::infra::db::repo;
use crate::infra::db::ts::db_now;
use crate::infra::db::tx::write_tx_with_wakes;
use crate::infra::db::{AttachmentKind, AttachmentStatus};
use crate::infra::llm::ProviderResolver;
use crate::infra::outbox::payloads::{AttachmentCleanupEvent, SecondaryRef};
use crate::infra::outbox::{OutboxEnqueuer, OutboxRecord};
use crate::infra::storage::{AnthropicFiles, FileStorage, VectorStores};
use crate::metrics::Metrics;

pub mod indexing;
pub mod mime;
mod secondary;
pub mod thumbnail;
pub mod upload;

pub use indexing::IndexingTimings;

/// `event_type` of the cleanup of a deleted attachment.
const EVENT_ATTACHMENT_DELETED: &str = "attachment_deleted";

/// An attachment as the API shows it (`AttachmentDetail`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachmentView {
    pub id: Uuid,
    pub filename: String,
    pub content_type: String,
    pub size_bytes: i64,
    pub status: AttachmentStatus,
    pub kind: AttachmentKind,
    /// Only for a `failed` attachment.
    pub error_code: Option<String>,
    /// Only for a `ready` image with a stored preview.
    pub thumbnail: Option<Thumbnail>,
    pub created_at: OffsetDateTime,
}

/// Collaborators of [`AttachmentService`].
pub struct AttachmentDeps {
    pub cfg: Arc<MiniChatConfig>,
    pub db: Arc<DBProvider<DomainError>>,
    pub authz: Arc<Authz>,
    pub models: Arc<ModelService>,
    pub providers: Arc<ProviderResolver>,
    pub files: Arc<dyn FileStorage>,
    pub vector_stores: Arc<dyn VectorStores>,
    pub outbox: Arc<OutboxEnqueuer>,
    pub metrics: Arc<Metrics>,
    /// Anthropic Files client (only when an `anthropic_messages` provider is configured).
    pub anthropic_files: Option<Arc<AnthropicFiles>>,
}

/// Upload, metadata and deletion of chat attachments.
pub struct AttachmentService {
    deps: AttachmentDeps,
    timings: IndexingTimings,
    /// In-flight uploads of this process (`rag.max_concurrent_uploads`).
    uploads: Arc<Semaphore>,
    /// Cancels the background indexing tasks on gear stop.
    cancel: CancellationToken,
}

impl AttachmentService {
    #[must_use]
    pub fn new(deps: AttachmentDeps) -> Self {
        let permits = usize::from(deps.cfg.rag.max_concurrent_uploads);
        Self {
            deps,
            timings: IndexingTimings::default(),
            uploads: Arc::new(Semaphore::new(permits)),
            cancel: CancellationToken::new(),
        }
    }

    /// Replaces the indexing waits and deadlines.
    #[must_use]
    pub fn with_timings(mut self, timings: IndexingTimings) -> Self {
        self.timings = timings;
        self
    }

    /// Cancels the background indexing tasks (gear stop). Rows they leave `uploaded` are failed
    /// later by the upload reaper.
    pub fn shutdown(&self) {
        self.cancel.cancel();
    }

    /// One of the caller's attachments of chat `chat_id`.
    ///
    /// # Errors
    /// `ChatNotFound`, `AttachmentNotFound` (unknown, deleted, in another chat or uploaded by
    /// someone else), PDP and database failures.
    pub async fn get(
        &self,
        ctx: &SecurityContext,
        chat_id: Uuid,
        attachment_id: Uuid,
    ) -> Result<AttachmentView, DomainError> {
        let (scope, _) = self
            .chat_scope(ctx, ChatAction::ReadAttachment, chat_id)
            .await?;
        let conn = self.deps.db.conn()?;
        let row = repo::attachments::find_in_chat(&conn, &scope, chat_id, attachment_id)
            .await?
            .filter(|a| a.deleted_at.is_none() && a.uploaded_by_user_id == ctx.subject_id())
            .ok_or_else(|| attachment_not_found(attachment_id))?;
        AttachmentView::try_from(row)
    }

    /// Soft-deletes one of the caller's unreferenced attachments and enqueues its provider
    /// cleanup (`attachment_deleted`) in the same transaction. Deleting an already deleted
    /// attachment changes nothing.
    ///
    /// # Errors
    /// `ChatNotFound`, `AttachmentNotFound` (also for another user's attachment, checked first),
    /// `AttachmentLocked` when a message references it, PDP and database failures.
    pub async fn delete(
        &self,
        ctx: &SecurityContext,
        chat_id: Uuid,
        attachment_id: Uuid,
    ) -> Result<(), DomainError> {
        let ((scope, tenant_id), chat) = self
            .load_chat(ctx, ChatAction::DeleteAttachment, chat_id)
            .await?;
        let user_id = ctx.subject_id();
        let outbox = Arc::clone(&self.deps.outbox);
        // The alias of a secondary copy is resolved before the transaction (it may call the
        // policy plugin), only for an attachment that has one.
        let conn = self.deps.db.conn()?;
        let has_secondary = repo::attachments::find_in_chat(&conn, &scope, chat_id, attachment_id)
            .await?
            .is_some_and(|a| a.secondary_file_id.is_some());
        let secondary_alias = if has_secondary {
            self.secondary_alias_for_delete(ctx, &chat).await
        } else {
            // A copy recorded after this read (upload still running) gets the shared alias.
            self.deps.providers.anthropic_files_alias(tenant_id)
        };
        let skipped_secondary = write_tx_with_wakes(&self.deps.db, move |tx, wakes| {
            let (scope, outbox) = (scope.clone(), Arc::clone(&outbox));
            let secondary_alias = secondary_alias.clone();
            Box::pin(async move {
                let row = repo::attachments::find_in_chat(tx, &scope, chat_id, attachment_id)
                    .await?
                    .filter(|a| a.uploaded_by_user_id == user_id)
                    .ok_or_else(|| attachment_not_found(attachment_id))?;
                if row.deleted_at.is_some() {
                    return Ok(false);
                }
                if repo::attachments::is_referenced(tx, &scope, attachment_id).await? {
                    return Err(DomainError::AttachmentLocked);
                }
                let now = db_now();
                if !repo::attachments::soft_delete(tx, &scope, attachment_id, now).await? {
                    return Ok(false);
                }
                let (secondary_ref, skipped_secondary) = secondary_ref(&row, secondary_alias);
                let record = OutboxRecord::attachment_cleanup(&AttachmentCleanupEvent {
                    event_type: EVENT_ATTACHMENT_DELETED.to_owned(),
                    tenant_id,
                    chat_id,
                    attachment_id,
                    provider_file_id: row.provider_file_id,
                    vector_store_id: None,
                    storage_backend: row.storage_backend,
                    attachment_kind: row.attachment_kind,
                    deleted_at: now,
                    secondary_ref,
                })?;
                wakes.add(outbox.enqueue(tx, record).await?);
                Ok(skipped_secondary)
            })
        })
        .await?;
        if skipped_secondary {
            tracing::warn!(%attachment_id, "secondary file alias not resolvable: the secondary copy is not deleted");
            self.deps.metrics.secondary_cleanup_skipped.add(
                1,
                &[KeyValue::new(
                    "provider_kind",
                    repo::attachments::SECONDARY_PROVIDER_ANTHROPIC,
                )],
            );
        }
        Ok(())
    }

    /// Authorizes `action` on the chat and loads it; returns the tenant scope of its child rows
    /// and the chat's tenant.
    async fn chat_scope(
        &self,
        ctx: &SecurityContext,
        action: ChatAction,
        chat_id: Uuid,
    ) -> Result<(AccessScope, Uuid), DomainError> {
        Ok(self.load_chat(ctx, action, chat_id).await?.0)
    }

    /// Authorizes `action`, loads the caller's chat; returns `((tenant scope, tenant), chat)`.
    async fn load_chat(
        &self,
        ctx: &SecurityContext,
        action: ChatAction,
        chat_id: Uuid,
    ) -> Result<((AccessScope, Uuid), chats::Model), DomainError> {
        let scope = self
            .deps
            .authz
            .chat_scope(ctx, action, Some(chat_id))
            .await?;
        let conn = self.deps.db.conn()?;
        let chat = repo::chats::load_scoped(&conn, &scope, chat_id)
            .await?
            .ok_or_else(|| DomainError::ChatNotFound {
                id: chat_id.to_string(),
            })?;
        Ok(((scope.tenant_only(), chat.tenant_id), chat))
    }
}

/// The `secondary_ref` of a deleted attachment with an uploaded secondary copy, addressed to
/// `alias` (resolved at delete time); the flag is set when a copy exists but no alias could be
/// resolved, so the copy is not deleted.
fn secondary_ref(row: &attachments::Model, alias: Option<String>) -> (Option<SecondaryRef>, bool) {
    let Some(file_id) = row.secondary_file_id.clone() else {
        return (None, false);
    };
    match alias {
        Some(upstream_alias) => (
            Some(SecondaryRef {
                file_id,
                provider_kind: row
                    .secondary_provider_kind
                    .clone()
                    .unwrap_or_else(|| repo::attachments::SECONDARY_PROVIDER_ANTHROPIC.to_owned()),
                upstream_alias,
            }),
            false,
        ),
        None => (None, true),
    }
}

fn attachment_not_found(id: Uuid) -> DomainError {
    DomainError::AttachmentNotFound { id: id.to_string() }
}

impl TryFrom<attachments::Model> for AttachmentView {
    type Error = DomainError;

    fn try_from(row: attachments::Model) -> Result<Self, DomainError> {
        let status = AttachmentStatus::parse(&row.status).ok_or_else(|| {
            DomainError::Internal(format!(
                "stored attachment status `{}` is not valid",
                row.status
            ))
        })?;
        let kind = AttachmentKind::parse(&row.attachment_kind).ok_or_else(|| {
            DomainError::Internal(format!(
                "stored attachment kind `{}` is not valid",
                row.attachment_kind
            ))
        })?;
        let thumbnail = match (
            kind == AttachmentKind::Image && status == AttachmentStatus::Ready,
            row.img_thumbnail,
            row.img_thumbnail_width,
            row.img_thumbnail_height,
        ) {
            (true, Some(data), Some(width), Some(height)) => Some(Thumbnail {
                width,
                height,
                data,
            }),
            _ => None,
        };
        Ok(Self {
            id: row.id,
            filename: row.filename,
            content_type: row.content_type,
            size_bytes: row.size_bytes,
            status,
            kind,
            error_code: row
                .error_code
                .filter(|_| status == AttachmentStatus::Failed),
            thumbnail,
            created_at: row.created_at,
        })
    }
}

#[cfg(test)]
mod tests;
