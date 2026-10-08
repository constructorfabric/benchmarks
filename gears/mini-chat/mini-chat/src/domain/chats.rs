//! Chat CRUD use cases.

use time::OffsetDateTime;
use toolkit_db::odata::{LimitCfg, paginate_odata};
use toolkit_db::secure::SecureEntityExt;
use toolkit_odata::{ODataOrderBy, ODataQuery, OrderKey, Page, SortDir};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use sea_orm::{ColumnTrait, Condition, EntityTrait, QueryFilter};

use super::error::{DomainError, Res};
use super::service::Services;
use crate::infra::db::entities::chat;
use crate::infra::db::odata::{ChatMapper, ChatQueryFilterField};
use crate::infra::db::repo::{attachments, chats};
use crate::infra::db::{now_ts, tenant_scope};
use crate::infra::outbox::{self, ChatCleanupMsg, Queue};

/// Chat metadata with its message count.
#[derive(Debug, Clone, PartialEq, Eq)]
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
    fn from_model(m: chat::Model, message_count: i64) -> Self {
        Self {
            id: m.id,
            model: m.model,
            title: m.title,
            is_temporary: m.is_temporary,
            message_count,
            created_at: m.created_at,
            updated_at: m.updated_at,
        }
    }
}

pub const LIST_LIMITS: LimitCfg = LimitCfg {
    default: 20,
    max: 100,
};

/// Validate and normalize a chat title (trimmed, 1–255 characters).
pub fn validate_title(title: &str) -> Result<String, DomainError> {
    let t = title.trim();
    let n = t.chars().count();
    if n == 0 || n > 255 {
        return Err(DomainError::invalid(
            Res::Chat,
            "title",
            "INVALID_TITLE",
            "Title must be 1-255 characters after trimming",
        ));
    }
    Ok(t.to_owned())
}

/// Default ordering when the query has none and no cursor.
pub fn with_default_order(mut q: ODataQuery, field: &str, dir: SortDir) -> ODataQuery {
    if q.order.is_empty() && q.cursor.is_none() {
        q.order = ODataOrderBy(vec![OrderKey {
            field: field.to_owned(),
            dir,
        }]);
    }
    q
}

impl Services {
    pub async fn create_chat(
        &self,
        ctx: &SecurityContext,
        title: Option<String>,
        model: Option<String>,
    ) -> Result<ChatView, DomainError> {
        let title = title.map(|t| validate_title(&t)).transpose()?;
        let scope = self.chat_scope(ctx, "create", None).await?;
        let snapshot = self.policy.current_snapshot(ctx.subject_id()).await?;
        let model_id = match model {
            Some(m) => snapshot
                .find_enabled_model(&m)
                .map(|e| e.id.clone())
                .ok_or_else(|| DomainError::invalid_model(Res::Chat))?,
            None => snapshot
                .default_model()
                .map(|e| e.id.clone())
                .ok_or_else(|| DomainError::invalid_model(Res::Chat))?,
        };
        let now = now_ts();
        let m = chat::Model {
            id: Uuid::new_v4(),
            tenant_id: ctx.subject_tenant_id(),
            user_id: ctx.subject_id(),
            model: model_id,
            title,
            is_temporary: false,
            created_at: now,
            updated_at: now,
            deleted_at: None,
        };
        let conn = self.db.conn()?;
        let m = chats::insert(&conn, &scope, m).await?;
        Ok(ChatView::from_model(m, 0))
    }

    /// Owner-scoped chat lookup (404 when missing, deleted or foreign).
    pub async fn load_chat(
        &self,
        ctx: &SecurityContext,
        action: &str,
        chat_id: Uuid,
    ) -> Result<chat::Model, DomainError> {
        let scope = self.chat_scope(ctx, action, Some(chat_id)).await?;
        let conn = self.db.conn()?;
        chats::find(&conn, &scope, chat_id)
            .await?
            .ok_or_else(|| DomainError::not_found(Res::Chat, chat_id.to_string()))
    }

    pub async fn get_chat(
        &self,
        ctx: &SecurityContext,
        chat_id: Uuid,
    ) -> Result<ChatView, DomainError> {
        let chat = self.load_chat(ctx, "read", chat_id).await?;
        let conn = self.db.conn()?;
        let n = chats::message_count(&conn, &tenant_scope(chat.tenant_id), chat.id).await?;
        Ok(ChatView::from_model(chat, n))
    }

    pub async fn list_chats(
        &self,
        ctx: &SecurityContext,
        query: ODataQuery,
    ) -> Result<Page<ChatView>, DomainError> {
        let scope = self.chat_scope(ctx, "list", None).await?;
        let query = with_default_order(query, "updated_at", SortDir::Desc);
        let conn = self.db.conn()?;
        let select = chat::Entity::find()
            .filter(Condition::all().add(chat::Column::DeletedAt.is_null()))
            .secure()
            .scope_with(&scope);
        let page =
            paginate_odata::<ChatQueryFilterField, ChatMapper, chat::Entity, chat::Model, _, _>(
                select,
                &conn,
                &query,
                ("id", SortDir::Desc),
                LIST_LIMITS,
                |m| m,
            )
            .await?;
        let ids: Vec<Uuid> = page.items.iter().map(|c| c.id).collect();
        let counts =
            chats::message_counts(&conn, &tenant_scope(ctx.subject_tenant_id()), &ids).await?;
        Ok(page.map_items(|c| {
            let n = counts.get(&c.id).copied().unwrap_or(0);
            ChatView::from_model(c, n)
        }))
    }

    pub async fn update_chat_title(
        &self,
        ctx: &SecurityContext,
        chat_id: Uuid,
        title: &str,
    ) -> Result<ChatView, DomainError> {
        let title = validate_title(title)?;
        let scope = self.chat_scope(ctx, "update", Some(chat_id)).await?;
        let conn = self.db.conn()?;
        if !chats::update_title(&conn, &scope, chat_id, &title, now_ts()).await? {
            return Err(DomainError::not_found(Res::Chat, chat_id.to_string()));
        }
        let chat = chats::find(&conn, &scope, chat_id)
            .await?
            .ok_or_else(|| DomainError::not_found(Res::Chat, chat_id.to_string()))?;
        let n = chats::message_count(&conn, &tenant_scope(chat.tenant_id), chat.id).await?;
        Ok(ChatView::from_model(chat, n))
    }

    /// Soft-delete the chat, hand its attachments to cleanup and enqueue the
    /// chat cleanup message, in one transaction.
    pub async fn delete_chat(
        &self,
        ctx: &SecurityContext,
        chat_id: Uuid,
    ) -> Result<(), DomainError> {
        let scope = self.chat_scope(ctx, "delete", Some(chat_id)).await?;
        let tenant = ctx.subject_tenant_id();
        let ob = self.outbox.clone();
        let wakes = self
            .db
            .transaction(move |tx| {
                Box::pin(async move {
                    let now = now_ts();
                    if !chats::soft_delete(tx, &scope, chat_id, now).await? {
                        return Err(DomainError::not_found(Res::Chat, chat_id.to_string()));
                    }
                    let ts = tenant_scope(tenant);
                    attachments::mark_chat_cleanup_pending(tx, &ts, chat_id, now).await?;
                    let msg = ChatCleanupMsg {
                        tenant_id: tenant,
                        chat_id,
                        system_request_id: Uuid::new_v4(),
                        reason: "chat_soft_delete".to_owned(),
                        chat_deleted_at: now,
                    };
                    let wake = ob.enqueue(tx, Queue::ChatCleanup, chat_id, &msg).await?;
                    Ok(vec![wake])
                })
            })
            .await?;
        outbox::fire(wakes);
        Ok(())
    }
}
