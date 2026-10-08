//! Chat CRUD (DESIGN §3.3 "Create / List / Get / Update / Delete Chat",
//! "Cleanup on Chat Deletion", ADR-0009).

use std::collections::HashMap;
use std::sync::Arc;

use toolkit_db::DBProvider;
use toolkit_odata::{ODataQuery, Page};
use toolkit_security::{AccessScope, SecurityContext};
use uuid::Uuid;

use crate::domain::authz::{self, Pep};
use crate::domain::clock::Clock;
use crate::domain::error::DomainError;
use crate::domain::models::ChatDetail;
use crate::domain::ports::{OutboxPort, PendingWakes};
use crate::domain::services::ModelResolver;
use crate::infra::db::entity::chat;
use crate::infra::db::repos::{AttachmentRepo, ChatRepo, MessageRepo};
use crate::infra::db::tx::with_retry;
use crate::infra::outbox::payloads::ChatCleanupPayload;

/// Maximum title length in characters (after trimming).
const MAX_TITLE_CHARS: usize = 255;

/// Trim `raw`; the result must have 1..=255 characters (Unicode scalar
/// values, not bytes).
///
/// # Errors
/// `InvalidTitle` when the trimmed title is empty or too long.
pub fn validate_title(raw: &str) -> Result<String, DomainError> {
    let title = raw.trim();
    let chars = title.chars().count();
    if chars == 0 || chars > MAX_TITLE_CHARS {
        return Err(DomainError::InvalidTitle);
    }
    Ok(title.to_owned())
}

/// Chat lifecycle service; also provides the scoped chat lookup every
/// chat-scoped service starts with ([`ChatService::load_scoped`]).
pub struct ChatService {
    db: Arc<DBProvider<DomainError>>,
    clock: Arc<dyn Clock>,
    pep: Arc<Pep>,
    models: Arc<ModelResolver>,
    outbox: Arc<dyn OutboxPort>,
}

impl ChatService {
    #[must_use]
    pub fn new(
        db: Arc<DBProvider<DomainError>>,
        clock: Arc<dyn Clock>,
        pep: Arc<Pep>,
        models: Arc<ModelResolver>,
        outbox: Arc<dyn OutboxPort>,
    ) -> Self {
        Self {
            db,
            clock,
            pep,
            models,
            outbox,
        }
    }

    /// Create a chat owned by the caller. The title is validated before the
    /// authorization check and the model lookup.
    ///
    /// # Errors
    /// `InvalidTitle`, `AuthzDenied` / `AuthzUnavailable`, `InvalidModel`
    /// (unknown / disabled model, or no enabled model), policy plugin and
    /// database failures.
    pub async fn create(
        &self,
        ctx: &SecurityContext,
        title: Option<String>,
        model: Option<String>,
    ) -> Result<ChatDetail, DomainError> {
        let title = title.as_deref().map(validate_title).transpose()?;
        let scope = self.pep.chat_scope(ctx, authz::CREATE, None).await?;
        let entry = self
            .models
            .resolve_for_new_chat(ctx.subject_id(), model.as_deref())
            .await?;
        let now = self.clock.now();
        let row = chat::Model {
            id: Uuid::new_v4(),
            tenant_id: ctx.subject_tenant_id(),
            user_id: ctx.subject_id(),
            model: entry.id,
            title,
            is_temporary: false,
            created_at: now,
            updated_at: now,
            deleted_at: None,
        };
        let conn = self.db.conn()?;
        let row = ChatRepo.insert(&conn, &scope, row).await?;
        Ok(ChatDetail::from_row(row, 0))
    }

    /// Chat metadata with its message count.
    ///
    /// # Errors
    /// `ChatNotFound` (missing, deleted, or not the caller's), authorization
    /// and database failures.
    pub async fn get(&self, ctx: &SecurityContext, id: Uuid) -> Result<ChatDetail, DomainError> {
        let (scope, row) = self.load_scoped(ctx, authz::READ, id).await?;
        let conn = self.db.conn()?;
        let counts = MessageRepo
            .count_active_by_chats(&conn, &scope, &[row.id])
            .await?;
        let count = counts.get(&row.id).copied().unwrap_or(0);
        Ok(ChatDetail::from_row(row, count))
    }

    /// One page of the caller's non-deleted chats (default order
    /// `updated_at desc, id desc`; page size default 20, max 100).
    ///
    /// # Errors
    /// `InvalidQuery` (bad filter / order field / cursor), authorization and
    /// database failures.
    pub async fn list(
        &self,
        ctx: &SecurityContext,
        query: &ODataQuery,
    ) -> Result<Page<ChatDetail>, DomainError> {
        let scope = self.pep.chat_scope(ctx, authz::LIST, None).await?;
        let conn = self.db.conn()?;
        let page = ChatRepo
            .list_page(&conn, &scope, query, self.db.db().backend())
            .await?;
        let ids: Vec<Uuid> = page.items.iter().map(|c| c.id).collect();
        let counts: HashMap<Uuid, i64> = MessageRepo
            .count_active_by_chats(&conn, &scope, &ids)
            .await?;
        Ok(page.map_items(|row| {
            let count = counts.get(&row.id).copied().unwrap_or(0);
            ChatDetail::from_row(row, count)
        }))
    }

    /// Set the chat title (trimmed, 1..=255 characters) and bump
    /// `updated_at`. Nothing else is modified.
    ///
    /// # Errors
    /// `InvalidTitle`, `ChatNotFound`, authorization and database failures.
    pub async fn rename(
        &self,
        ctx: &SecurityContext,
        id: Uuid,
        title: &str,
    ) -> Result<ChatDetail, DomainError> {
        let title = validate_title(title)?;
        let scope = self.pep.chat_scope(ctx, authz::UPDATE, Some(id)).await?;
        let conn = self.db.conn()?;
        let row = ChatRepo
            .rename(&conn, &scope, id, &title, self.clock.now())
            .await?
            .ok_or(DomainError::ChatNotFound)?;
        let counts = MessageRepo
            .count_active_by_chats(&conn, &scope, &[row.id])
            .await?;
        let count = counts.get(&row.id).copied().unwrap_or(0);
        Ok(ChatDetail::from_row(row, count))
    }

    /// Soft-delete the chat, mark its attachments for cleanup and enqueue the
    /// chat-cleanup message, in one transaction; the outbox is woken after
    /// commit. Child rows are not modified (ADR-0009).
    ///
    /// # Errors
    /// `ChatNotFound` (also for an already deleted chat), authorization,
    /// outbox (`OutboxPayloadTooLarge`, `Internal`) and database failures.
    pub async fn delete(&self, ctx: &SecurityContext, id: Uuid) -> Result<(), DomainError> {
        let scope = self.pep.chat_scope(ctx, authz::DELETE, Some(id)).await?;
        let now = self.clock.now();
        let outbox = Arc::clone(&self.outbox);
        let wakes = with_retry(&self.db, move |tx| {
            let (scope, outbox) = (scope.clone(), Arc::clone(&outbox));
            Box::pin(async move {
                let row = ChatRepo
                    .soft_delete(tx, &scope, id, now)
                    .await?
                    .ok_or(DomainError::ChatNotFound)?;
                AttachmentRepo
                    .mark_chat_cleanup_pending(tx, &scope, id, now)
                    .await?;
                let mut wakes = PendingWakes::new();
                let payload = ChatCleanupPayload::new(row.tenant_id, row.id, now);
                outbox
                    .enqueue_chat_cleanup(tx, &payload, &mut wakes)
                    .await?;
                Ok(wakes)
            })
        })
        .await?;
        wakes.fire_all();
        Ok(())
    }

    /// Authorize `action` on chat `id` and load the non-deleted chat with
    /// the resulting tenant + owner scope.
    ///
    /// # Errors
    /// `ChatNotFound` when the chat is missing, deleted or not the caller's;
    /// authorization and database failures.
    pub(crate) async fn load_scoped(
        &self,
        ctx: &SecurityContext,
        action: &str,
        id: Uuid,
    ) -> Result<(AccessScope, chat::Model), DomainError> {
        let scope = self.pep.chat_scope(ctx, action, Some(id)).await?;
        let conn = self.db.conn()?;
        let row = ChatRepo
            .find_active(&conn, &scope, id)
            .await?
            .ok_or(DomainError::ChatNotFound)?;
        Ok((scope, row))
    }
}

#[cfg(test)]
#[path = "chat_service_tests.rs"]
mod tests;
