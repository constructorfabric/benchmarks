//! Chat CRUD (DESIGN §3.3 Create/List/Get/Update/Delete Chat).

use std::sync::Arc;

use toolkit_db::DBProvider;
use toolkit_odata::{ODataQuery, Page};
use toolkit_security::{AccessScope, SecurityContext};
use uuid::Uuid;

use super::ListError;
use super::model_catalog::ModelCatalogService;
use crate::domain::authz::{ChatAuthz, actions};
use crate::domain::clock::now_utc;
use crate::domain::error::{DomainError, DomainResult};
use crate::infra::db::entities::chat;
use crate::infra::db::repos::chat::NewChat;
use crate::infra::db::repos::{ChatRepo, MessageRepo};
use crate::infra::db::tx::with_tx_retry;
use crate::infra::outbox::payloads::{CHAT_CLEANUP_PAYLOAD_TYPE, ChatCleanupPayload};
use crate::infra::outbox::{OutboxEnqueuer, QueueKind};

/// Maximum title length in characters (after trimming).
pub const MAX_TITLE_CHARS: usize = 255;

/// A chat with its number of non-deleted messages.
#[derive(Debug, Clone)]
pub struct ChatView {
    pub chat: chat::Model,
    pub message_count: i64,
}

/// Trimmed title of 1..=255 characters.
///
/// # Errors
/// `InvalidTitle` for an empty, whitespace-only or longer title.
pub fn validate_title(raw: &str) -> DomainResult<String> {
    let trimmed = raw.trim();
    let chars = trimmed.chars().count();
    if (1..=MAX_TITLE_CHARS).contains(&chars) {
        Ok(trimmed.to_owned())
    } else {
        Err(DomainError::InvalidTitle)
    }
}

pub struct ChatService {
    db: Arc<DBProvider<DomainError>>,
    authz: Arc<ChatAuthz>,
    models: Arc<ModelCatalogService>,
    outbox: Arc<OutboxEnqueuer>,
}

impl ChatService {
    #[must_use]
    pub fn new(
        db: Arc<DBProvider<DomainError>>,
        authz: Arc<ChatAuthz>,
        models: Arc<ModelCatalogService>,
        outbox: Arc<OutboxEnqueuer>,
    ) -> Self {
        Self {
            db,
            authz,
            models,
            outbox,
        }
    }

    /// Create a chat. The title is validated before authorization and model lookup.
    ///
    /// # Errors
    /// `InvalidTitle`, authorization errors, `InvalidModel`, database failures.
    pub async fn create(
        &self,
        ctx: &SecurityContext,
        title: Option<String>,
        model: Option<String>,
    ) -> DomainResult<ChatView> {
        let title = title.as_deref().map(validate_title).transpose()?;
        let scope = self.authz.chat_scope(ctx, actions::CREATE, None).await?;
        let resolved = self
            .models
            .resolve_for_create(ctx.subject_id(), model.as_deref())
            .await?;
        let conn = self.db.conn()?;
        let chat = ChatRepo::insert(
            &conn,
            &scope,
            NewChat {
                id: Uuid::new_v4(),
                tenant_id: ctx.subject_tenant_id(),
                user_id: ctx.subject_id(),
                model: resolved.entry.id,
                title,
                now: now_utc(),
            },
        )
        .await?;
        Ok(ChatView {
            chat,
            message_count: 0,
        })
    }

    /// One chat of the caller.
    ///
    /// # Errors
    /// Authorization errors, `ChatNotFound`, database failures.
    pub async fn get(&self, ctx: &SecurityContext, id: Uuid) -> DomainResult<ChatView> {
        let scope = self.authz.chat_scope(ctx, actions::READ, Some(id)).await?;
        let chat = self.load_chat(&scope, id).await?;
        self.view(chat).await
    }

    /// One page of the caller's chats.
    ///
    /// # Errors
    /// `ListError::OData` for filter/order/cursor errors, `ListError::Domain` otherwise.
    pub async fn list(
        &self,
        ctx: &SecurityContext,
        query: &ODataQuery,
    ) -> Result<Page<ChatView>, ListError> {
        let scope = self.authz.chat_scope(ctx, actions::LIST, None).await?;
        let conn = self.db.conn()?;
        let page = ChatRepo::list_page(&conn, &scope, query).await?;
        let ids: Vec<Uuid> = page.items.iter().map(|c| c.id).collect();
        let counts = MessageRepo::count_live_by_chat(&conn, ctx.subject_tenant_id(), &ids).await?;
        let items = page
            .items
            .into_iter()
            .map(|chat| {
                let message_count = counts.get(&chat.id).copied().unwrap_or(0);
                ChatView {
                    chat,
                    message_count,
                }
            })
            .collect();
        Ok(Page {
            items,
            page_info: page.page_info,
        })
    }

    /// Rename a chat (`updated_at = now`); no other field changes.
    ///
    /// # Errors
    /// `InvalidTitle`, authorization errors, `ChatNotFound`, database failures.
    pub async fn update_title(
        &self,
        ctx: &SecurityContext,
        id: Uuid,
        title: String,
    ) -> DomainResult<ChatView> {
        let title = validate_title(&title)?;
        let scope = self
            .authz
            .chat_scope(ctx, actions::UPDATE, Some(id))
            .await?;
        let chat = ChatRepo::update_title(&self.db.conn()?, &scope, id, title, now_utc())
            .await?
            .ok_or(DomainError::ChatNotFound)?;
        self.view(chat).await
    }

    /// Soft-delete a chat, hand its attachments to cleanup and enqueue the
    /// chat-cleanup message, in one transaction.
    ///
    /// # Errors
    /// Authorization errors, `ChatNotFound`, `OutboxPayloadTooLarge`, database failures.
    pub async fn delete(&self, ctx: &SecurityContext, id: Uuid) -> DomainResult<()> {
        let scope = self
            .authz
            .chat_scope(ctx, actions::DELETE, Some(id))
            .await?;
        let outbox = Arc::clone(&self.outbox);
        let wake = with_tx_retry(&self.db, "chat delete", move |tx| {
            let (scope, outbox) = (scope.clone(), Arc::clone(&outbox));
            Box::pin(async move {
                let now = now_utc();
                let chat = ChatRepo::soft_delete(tx, &scope, id, now)
                    .await?
                    .ok_or(DomainError::ChatNotFound)?;
                ChatRepo::mark_attachments_cleanup_pending(tx, chat.tenant_id, chat.id, now)
                    .await?;
                let payload = ChatCleanupPayload::soft_delete(chat.tenant_id, chat.id, now);
                outbox
                    .enqueue_json(
                        tx,
                        QueueKind::ChatCleanup,
                        chat.id,
                        CHAT_CLEANUP_PAYLOAD_TYPE,
                        &payload,
                    )
                    .await
            })
        })
        .await?;
        wake.fire();
        Ok(())
    }

    /// The non-deleted chat `id` visible in `scope`.
    ///
    /// # Errors
    /// `ChatNotFound` (404 masking for missing, deleted and foreign chats).
    pub async fn load_chat(&self, scope: &AccessScope, id: Uuid) -> DomainResult<chat::Model> {
        let conn = self.db.conn()?;
        ChatRepo::find_live(&conn, scope, id)
            .await?
            .ok_or(DomainError::ChatNotFound)
    }

    async fn view(&self, chat: chat::Model) -> DomainResult<ChatView> {
        let conn = self.db.conn()?;
        let message_count = MessageRepo::count_live(&conn, chat.tenant_id, chat.id).await?;
        Ok(ChatView {
            chat,
            message_count,
        })
    }
}

#[cfg(test)]
#[path = "chat_tests.rs"]
mod chat_tests;
