//! Chat CRUD (DESIGN section 3.3: Create/List/Get/Update/Delete Chat).

use std::sync::Arc;

use sea_orm::ActiveValue::Set;
use time::OffsetDateTime;
use toolkit_db::DBProvider;
use toolkit_db::secure::DBRunner;
use toolkit_odata::{ODataQuery, Page};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::domain::error::{DomainError, ResourceKind};
use crate::domain::ports::{AuthzPort, ChatAction};
use crate::domain::services::model_service::ModelService;
use crate::domain::time::db_now;
use crate::infra::db::entities::chat;
use crate::infra::db::repos::{attachment_repo, chat_repo, message_repo};
use crate::infra::outbox::MiniChatOutbox;
use crate::infra::outbox::payloads::ChatCleanupPayload;

/// Maximum chat title length (characters, after trimming).
pub const MAX_TITLE_CHARS: usize = 255;

/// `reason` of the chat-cleanup outbox message.
const CHAT_CLEANUP_REASON: &str = "chat_soft_delete";

/// Input of [`ChatService::create`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CreateChat {
    pub title: Option<String>,
    pub model: Option<String>,
}

/// A chat as the API shows it (`ChatDetail`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChatView {
    pub id: Uuid,
    pub model: String,
    pub title: Option<String>,
    pub is_temporary: bool,
    pub message_count: i64,
    pub created_at: OffsetDateTime,
    pub updated_at: OffsetDateTime,
}

impl ChatView {
    fn new(row: chat::Model, message_count: i64) -> Self {
        Self {
            id: row.id,
            model: row.model,
            title: row.title,
            is_temporary: row.is_temporary,
            message_count,
            created_at: row.created_at,
            updated_at: row.updated_at,
        }
    }
}

/// Trimmed title, 1..=255 characters, else `InvalidTitle`.
///
/// # Errors
/// `InvalidTitle`.
pub fn validate_title(raw: &str) -> Result<String, DomainError> {
    let title = raw.trim();
    let len = title.chars().count();
    if (1..=MAX_TITLE_CHARS).contains(&len) {
        Ok(title.to_owned())
    } else {
        Err(DomainError::InvalidTitle)
    }
}

fn chat_not_found() -> DomainError {
    DomainError::NotFound {
        resource: ResourceKind::Chat,
    }
}

pub struct ChatService {
    db: Arc<DBProvider<DomainError>>,
    authz: Arc<dyn AuthzPort>,
    models: Arc<ModelService>,
    outbox: Arc<MiniChatOutbox>,
}

impl ChatService {
    #[must_use]
    pub fn new(
        db: Arc<DBProvider<DomainError>>,
        authz: Arc<dyn AuthzPort>,
        models: Arc<ModelService>,
        outbox: Arc<MiniChatOutbox>,
    ) -> Self {
        Self {
            db,
            authz,
            models,
            outbox,
        }
    }

    /// Creates a chat owned by the caller. The title is validated before the
    /// authorization check and the model lookup.
    ///
    /// # Errors
    /// `InvalidTitle`, authorization failure, `InvalidModel` (unknown or
    /// disabled model, or no enabled model), policy plugin or database failure.
    pub async fn create(
        &self,
        ctx: &SecurityContext,
        req: CreateChat,
    ) -> Result<ChatView, DomainError> {
        let title = req.title.as_deref().map(validate_title).transpose()?;
        let scope = self.authz.chat_scope(ctx, ChatAction::Create, None).await?;
        let model = match req.model.as_deref() {
            Some(id) => {
                self.models
                    .resolve_for_chat(ctx.subject_id(), id, true)
                    .await?
                    .1
            }
            None => self.models.default_for_chat(ctx.subject_id()).await?,
        };
        let now = db_now();
        let am = chat::ActiveModel {
            id: Set(Uuid::new_v4()),
            tenant_id: Set(ctx.subject_tenant_id()),
            user_id: Set(ctx.subject_id()),
            model: Set(model.id),
            title: Set(title),
            is_temporary: Set(false),
            created_at: Set(now),
            updated_at: Set(now),
            deleted_at: Set(None),
        };
        let conn = self.db.conn()?;
        let row = chat_repo::insert(&conn, &scope, am).await?;
        Ok(ChatView::new(row, 0))
    }

    /// One live chat of the caller with its message count.
    ///
    /// # Errors
    /// Authorization failure, `NotFound` (chat), database failure.
    pub async fn get(&self, ctx: &SecurityContext, id: Uuid) -> Result<ChatView, DomainError> {
        let scope = self
            .authz
            .chat_scope(ctx, ChatAction::Read, Some(id))
            .await?;
        let conn = self.db.conn()?;
        let row = chat_repo::find_scoped(&conn, &scope, id)
            .await?
            .ok_or_else(chat_not_found)?;
        with_count(&conn, row).await
    }

    /// Page of the caller's live chats (default `updated_at desc`, `id desc`).
    ///
    /// # Errors
    /// Authorization failure, `Query` (bad `OData` query), database failure.
    pub async fn list(
        &self,
        ctx: &SecurityContext,
        query: ODataQuery,
    ) -> Result<Page<ChatView>, DomainError> {
        let scope = self.authz.chat_scope(ctx, ChatAction::List, None).await?;
        let conn = self.db.conn()?;
        let page = chat_repo::list_page(&conn, &scope, query).await?;
        let ids: Vec<Uuid> = page.items.iter().map(|c| c.id).collect();
        let counts = message_repo::count_live_by_chat(&conn, ctx.subject_tenant_id(), &ids).await?;
        Ok(page.map_items(|row| {
            let n = counts.get(&row.id).copied().unwrap_or(0);
            ChatView::new(row, n)
        }))
    }

    /// Renames a live chat and bumps `updated_at`; nothing else changes.
    ///
    /// # Errors
    /// `InvalidTitle`, authorization failure, `NotFound` (chat), database
    /// failure.
    pub async fn update_title(
        &self,
        ctx: &SecurityContext,
        id: Uuid,
        title: &str,
    ) -> Result<ChatView, DomainError> {
        let title = validate_title(title)?;
        let scope = self
            .authz
            .chat_scope(ctx, ChatAction::Update, Some(id))
            .await?;
        let conn = self.db.conn()?;
        if chat_repo::update_title(&conn, &scope, id, &title, db_now()).await? == 0 {
            return Err(chat_not_found());
        }
        let row = chat_repo::find_scoped(&conn, &scope, id)
            .await?
            .ok_or_else(chat_not_found)?;
        with_count(&conn, row).await
    }

    /// Soft-deletes a live chat in one transaction: `deleted_at` and
    /// `updated_at` are set, the chat's live attachments without a cleanup
    /// state become `cleanup_status = 'pending'`, and a chat-cleanup outbox
    /// message is enqueued (woken after commit).
    ///
    /// # Errors
    /// Authorization failure, `NotFound` (chat, also on a second delete),
    /// `OutboxPayloadTooLarge { during_chat_delete: true }`, database failure.
    pub async fn delete(&self, ctx: &SecurityContext, id: Uuid) -> Result<(), DomainError> {
        let scope = self
            .authz
            .chat_scope(ctx, ChatAction::Delete, Some(id))
            .await?;
        let outbox = Arc::clone(&self.outbox);
        let wake = self
            .db
            .transaction(move |tx| {
                Box::pin(async move {
                    // Write first: on SQLite a read-then-write transaction can
                    // fail with BUSY_SNAPSHOT when another connection commits
                    // in between; the guarded UPDATE is also the existence
                    // check. Reads after it run under the write lock.
                    let now = db_now();
                    if chat_repo::soft_delete(tx, &scope, id, now).await? == 0 {
                        return Err(chat_not_found());
                    }
                    let chat = chat_repo::find_any_scoped(tx, &scope, id)
                        .await?
                        .ok_or_else(chat_not_found)?;
                    let tenant_id = chat.tenant_id;
                    attachment_repo::mark_chat_cleanup_pending(tx, tenant_id, id, now).await?;
                    let payload = ChatCleanupPayload {
                        tenant_id,
                        chat_id: id,
                        system_request_id: Uuid::new_v4(),
                        reason: CHAT_CLEANUP_REASON.to_owned(),
                        chat_deleted_at: now,
                    };
                    outbox
                        .enqueue_chat_cleanup(tx, &payload)
                        .await
                        .map_err(|e| match e {
                            DomainError::OutboxPayloadTooLarge { .. } => {
                                DomainError::OutboxPayloadTooLarge {
                                    during_chat_delete: true,
                                }
                            }
                            other => other,
                        })
                })
            })
            .await?;
        wake.fire();
        Ok(())
    }
}

async fn with_count(conn: &impl DBRunner, row: chat::Model) -> Result<ChatView, DomainError> {
    let counts = message_repo::count_live_by_chat(conn, row.tenant_id, &[row.id]).await?;
    let n = counts.get(&row.id).copied().unwrap_or(0);
    Ok(ChatView::new(row, n))
}

#[cfg(test)]
#[path = "chat_service_tests.rs"]
mod chat_service_tests;
