//! Chat CRUD (DESIGN section 3.3 "Create Chat" .. "Delete Chat").

use std::sync::Arc;

use time::OffsetDateTime;
use toolkit_db::DBProvider;
use toolkit_odata::{ODataQuery, Page};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::domain::authz::{Authz, ChatAction};
use crate::domain::error::DomainError;
use crate::domain::model_service::ModelService;
use crate::infra::db::entity::chats;
use crate::infra::db::repo;
use crate::infra::db::ts::db_now;
use crate::infra::db::tx::{write_tx, write_tx_with_wakes};
use crate::infra::outbox::{ChatCleanupEvent, OutboxEnqueuer, OutboxRecord};

/// Longest accepted chat title, in characters.
const MAX_TITLE_CHARS: usize = 255;
/// `ChatCleanupEvent::reason` of a chat deletion.
const CHAT_SOFT_DELETE: &str = "chat_soft_delete";

/// Input of [`ChatService::create`].
#[derive(Debug, Clone, Default)]
pub struct CreateChat {
    pub title: Option<String>,
    pub model: Option<String>,
}

/// A chat with its message count.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatDetail {
    pub id: Uuid,
    pub model: String,
    pub title: Option<String>,
    pub is_temporary: bool,
    pub message_count: i64,
    pub created_at: OffsetDateTime,
    pub updated_at: OffsetDateTime,
}

impl ChatDetail {
    fn new(chat: chats::Model, message_count: i64) -> Self {
        Self {
            id: chat.id,
            model: chat.model,
            title: chat.title,
            is_temporary: chat.is_temporary,
            message_count,
            created_at: chat.created_at,
            updated_at: chat.updated_at,
        }
    }
}

/// Trims `raw` and checks that the result has 1 to 255 characters.
///
/// # Errors
/// `InvalidTitle` otherwise.
fn validate_title(raw: &str) -> Result<String, DomainError> {
    let title = raw.trim();
    let chars = title.chars().count();
    if chars == 0 || chars > MAX_TITLE_CHARS {
        return Err(DomainError::InvalidTitle);
    }
    Ok(title.to_owned())
}

fn not_found(id: Uuid) -> DomainError {
    DomainError::ChatNotFound { id: id.to_string() }
}

/// Chat lifecycle: create, list, read, rename, delete.
pub struct ChatService {
    db: Arc<DBProvider<DomainError>>,
    authz: Arc<Authz>,
    models: Arc<ModelService>,
    outbox: Arc<OutboxEnqueuer>,
}

impl ChatService {
    #[must_use]
    pub fn new(
        db: Arc<DBProvider<DomainError>>,
        authz: Arc<Authz>,
        models: Arc<ModelService>,
        outbox: Arc<OutboxEnqueuer>,
    ) -> Self {
        Self {
            db,
            authz,
            models,
            outbox,
        }
    }

    /// Creates a chat for the caller. The title is validated first, then the PDP is asked, then
    /// the model is resolved (the default model when none is requested).
    ///
    /// # Errors
    /// `InvalidTitle`, `AccessDenied` / `AuthzUnavailable`, `InvalidModel`, plugin and database
    /// failures.
    pub async fn create(
        &self,
        ctx: &SecurityContext,
        req: CreateChat,
    ) -> Result<ChatDetail, DomainError> {
        let title = req.title.as_deref().map(validate_title).transpose()?;
        let scope = self.authz.create_scope(ctx).await?;
        let model = self
            .models
            .resolve_for_create(ctx, req.model.as_deref())
            .await?;

        let now = db_now();
        let row = chats::ActiveModel {
            id: sea_orm::Set(Uuid::new_v4()),
            tenant_id: sea_orm::Set(ctx.subject_tenant_id()),
            user_id: sea_orm::Set(ctx.subject_id()),
            model: sea_orm::Set(model.id),
            title: sea_orm::Set(title),
            is_temporary: sea_orm::Set(false),
            created_at: sea_orm::Set(now),
            updated_at: sea_orm::Set(now),
            deleted_at: sea_orm::Set(None),
        };
        let conn = self.db.conn()?;
        let chat = repo::chats::insert(&conn, &scope, row).await?;
        Ok(ChatDetail::new(chat, 0))
    }

    /// One page of the caller's chats, most recent activity first unless the query orders them.
    ///
    /// # Errors
    /// `AccessDenied` / `AuthzUnavailable`, `OData` for an invalid filter, order or cursor,
    /// database failures.
    pub async fn list(
        &self,
        ctx: &SecurityContext,
        query: &ODataQuery,
    ) -> Result<Page<ChatDetail>, DomainError> {
        let scope = self.authz.chat_scope(ctx, ChatAction::List, None).await?;
        let conn = self.db.conn()?;
        let page = repo::chats::list(&conn, &scope, query).await?;
        let ids: Vec<Uuid> = page.items.iter().map(|c| c.id).collect();
        let counts = repo::chats::message_counts(&conn, &scope.tenant_only(), &ids).await?;
        Ok(page.map_items(|chat| {
            let count = counts.get(&chat.id).copied().unwrap_or(0);
            ChatDetail::new(chat, count)
        }))
    }

    /// One of the caller's chats.
    ///
    /// # Errors
    /// `ChatNotFound` (also for a deleted or foreign chat), `AccessDenied` / `AuthzUnavailable`,
    /// database failures.
    pub async fn get(&self, ctx: &SecurityContext, id: Uuid) -> Result<ChatDetail, DomainError> {
        let scope = self
            .authz
            .chat_scope(ctx, ChatAction::Read, Some(id))
            .await?;
        let conn = self.db.conn()?;
        let chat = repo::chats::load_scoped(&conn, &scope, id)
            .await?
            .ok_or_else(|| not_found(id))?;
        let count = repo::chats::message_count(&conn, &scope.tenant_only(), id).await?;
        Ok(ChatDetail::new(chat, count))
    }

    /// Sets the title of one of the caller's chats (and `updated_at`). The title is validated
    /// before the PDP is asked.
    ///
    /// # Errors
    /// `InvalidTitle`, `ChatNotFound`, `AccessDenied` / `AuthzUnavailable`, database failures.
    pub async fn rename(
        &self,
        ctx: &SecurityContext,
        id: Uuid,
        title: String,
    ) -> Result<ChatDetail, DomainError> {
        let title = validate_title(&title)?;
        let scope = self
            .authz
            .chat_scope(ctx, ChatAction::Update, Some(id))
            .await?;
        let tenant_scope = scope.tenant_only();
        write_tx(&self.db, move |tx| {
            let (scope, tenant_scope, title) = (scope.clone(), tenant_scope.clone(), title.clone());
            Box::pin(async move {
                if !repo::chats::rename(tx, &scope, id, &title, db_now()).await? {
                    return Err(not_found(id));
                }
                let chat = repo::chats::load_scoped(tx, &scope, id)
                    .await?
                    .ok_or_else(|| not_found(id))?;
                let count = repo::chats::message_count(tx, &tenant_scope, id).await?;
                Ok(ChatDetail::new(chat, count))
            })
        })
        .await
    }

    /// Soft-deletes one of the caller's chats. In one transaction: the chat is marked deleted,
    /// its attachments without cleanup state become `pending`, and a chat cleanup event is
    /// enqueued for the provider-side cleanup.
    ///
    /// # Errors
    /// `ChatNotFound` (also when already deleted), `AccessDenied` / `AuthzUnavailable`,
    /// `ChatCleanupPayloadTooLarge`, database failures.
    pub async fn delete(&self, ctx: &SecurityContext, id: Uuid) -> Result<(), DomainError> {
        let scope = self
            .authz
            .chat_scope(ctx, ChatAction::Delete, Some(id))
            .await?;
        let tenant_scope = scope.tenant_only();
        let outbox = Arc::clone(&self.outbox);
        write_tx_with_wakes(&self.db, move |tx, wakes| {
            let (scope, tenant_scope, outbox) =
                (scope.clone(), tenant_scope.clone(), Arc::clone(&outbox));
            Box::pin(async move {
                let chat = repo::chats::load_scoped(tx, &scope, id)
                    .await?
                    .ok_or_else(|| not_found(id))?;
                let now = db_now();
                if !repo::chats::soft_delete(tx, &scope, id, now).await? {
                    return Err(not_found(id));
                }
                repo::chats::mark_attachments_cleanup_pending(tx, &tenant_scope, id, now).await?;
                let record = OutboxRecord::chat_cleanup(&ChatCleanupEvent {
                    tenant_id: chat.tenant_id,
                    chat_id: id,
                    system_request_id: Uuid::new_v4(),
                    reason: CHAT_SOFT_DELETE.to_owned(),
                    chat_deleted_at: now,
                })?;
                wakes.add(outbox.enqueue(tx, record).await?);
                Ok(())
            })
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn title_is_trimmed_and_counted_in_chars() {
        assert_eq!(validate_title("  hi  ").unwrap(), "hi");
        assert_eq!(
            validate_title(&"\u{e9}".repeat(255))
                .unwrap()
                .chars()
                .count(),
            255
        );
        for bad in ["", "   ", "\n\t"] {
            assert!(matches!(
                validate_title(bad),
                Err(DomainError::InvalidTitle)
            ));
        }
        assert!(matches!(
            validate_title(&"\u{e9}".repeat(256)),
            Err(DomainError::InvalidTitle)
        ));
    }
}
