//! Chat CRUD (DESIGN §3.3 Create/List/Get/Update/Delete Chat).

use std::collections::HashMap;

use sea_orm::sea_query::Expr;
use sea_orm::{ColumnTrait, Condition, EntityTrait, ExprTrait, FromQueryResult, QueryFilter, QuerySelect, Set};
use serde::{Deserialize, Serialize};
use toolkit_db::odata::{LimitCfg, paginate_with_odata};
use toolkit_db::secure::{
    AccessScope, DBRunner, SecureEntityExt, SecureUpdateExt, secure_insert,
};
use toolkit_odata::{ODataOrderBy, ODataQuery, OrderKey, Page, SortDir};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use super::authz::{actions, child_scope};
use super::clock;
use super::error::DomainError;

use super::service::MiniChat;
use crate::infra::outbox::{OutboxKind, enqueue_json};
use crate::infra::storage::entity::{attachment, chat, message};

/// Chat plus its message count.
#[derive(Debug, Clone)]
pub struct ChatView {
    pub chat: chat::Model,
    pub message_count: i64,
}

/// Chat-cleanup outbox payload (DESIGN §3.6 "Outbox payload and execution semantics").
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatCleanupPayload {
    pub tenant_id: Uuid,
    pub chat_id: Uuid,
    pub system_request_id: Uuid,
    pub reason: String,
    pub chat_deleted_at: crate::domain::clock::Timestamp,
}

/// Validate and trim a chat title (1..=255 characters after trim).
///
/// # Errors
/// `InvalidTitle` when empty / whitespace-only / too long.
pub fn validate_title(title: &str) -> Result<String, DomainError> {
    let t = title.trim();
    if t.is_empty() {
        return Err(DomainError::InvalidTitle("title must not be empty".to_owned()));
    }
    if t.chars().count() > 255 {
        return Err(DomainError::InvalidTitle("title must be at most 255 characters".to_owned()));
    }
    Ok(t.to_owned())
}

#[derive(Debug, FromQueryResult)]
struct ChatCount {
    chat_id: Uuid,
    cnt: i64,
}

/// Count visible messages per chat.
///
/// # Errors
/// DB errors.
pub async fn message_counts(
    runner: &impl DBRunner,
    scope: &AccessScope,
    chat_ids: Vec<Uuid>,
) -> Result<HashMap<Uuid, i64>, DomainError> {
    if chat_ids.is_empty() {
        return Ok(HashMap::new());
    }
    let rows: Vec<ChatCount> = message::Entity::find()
        .filter(message::Column::ChatId.is_in(chat_ids))
        .filter(message::Column::DeletedAt.is_null())
        .filter(message::Column::RequestId.is_not_null())
        .secure()
        .scope_with(scope)
        .project_all(runner, |q| {
            q.select_only()
                .column(message::Column::ChatId)
                .column_as(Expr::col(message::Column::Id).count(), "cnt")
                .group_by(message::Column::ChatId)
                .into_model::<ChatCount>()
        })
        .await?;
    Ok(rows.into_iter().map(|r| (r.chat_id, r.cnt)).collect())
}

impl MiniChat {
    /// `POST /v1/chats`.
    ///
    /// # Errors
    /// `InvalidTitle`, PDP errors, `InvalidModel`, DB errors.
    pub async fn create_chat(
        &self,
        ctx: &SecurityContext,
        title: Option<String>,
        model: Option<String>,
    ) -> Result<ChatView, DomainError> {
        let title = title.as_deref().map(validate_title).transpose()?;
        let scope = self.authz.chat_scope(ctx, actions::CREATE, None).await?;
        let policy = self.policy.current(ctx.subject_id()).await?;
        let model_id = match model {
            Some(m) => policy
                .find_enabled(&m)
                .map(|e| e.id.clone())
                .ok_or(DomainError::InvalidModel(m))?,
            None => policy
                .default_model()
                .map(|e| e.id.clone())
                .ok_or_else(|| DomainError::InvalidModel(String::new()))?,
        };
        let now = clock::now();
        let am = chat::ActiveModel {
            id: Set(Uuid::now_v7()),
            tenant_id: Set(ctx.subject_tenant_id()),
            user_id: Set(ctx.subject_id()),
            model: Set(model_id),
            title: Set(title),
            is_temporary: Set(false),
            created_at: Set(now),
            updated_at: Set(now),
            deleted_at: Set(None),
        };
        let conn = self.db.conn()?;
        let created = secure_insert::<chat::Entity>(am, &scope, &conn).await?;
        Ok(ChatView { chat: created, message_count: 0 })
    }

    /// `GET /v1/chats/{id}`.
    ///
    /// # Errors
    /// PDP errors, `ChatNotFound`, DB errors.
    pub async fn get_chat(&self, ctx: &SecurityContext, chat_id: Uuid) -> Result<ChatView, DomainError> {
        let access = self.load_chat(ctx, actions::READ, chat_id).await?;
        let conn = self.db.conn()?;
        let counts = message_counts(&conn, &access.child_scope, vec![chat_id]).await?;
        let message_count = counts.get(&chat_id).copied().unwrap_or(0);
        Ok(ChatView { chat: access.chat, message_count })
    }

    /// `GET /v1/chats` (cursor pagination + `OData`).
    ///
    /// # Errors
    /// PDP errors, `OData` errors, DB errors.
    pub async fn list_chats(
        &self,
        ctx: &SecurityContext,
        mut query: ODataQuery,
    ) -> Result<Page<ChatView>, DomainError> {
        let scope = self.authz.chat_scope(ctx, actions::LIST, None).await?;
        if query.order.is_empty() && query.cursor.is_none() {
            query.order = ODataOrderBy(vec![OrderKey { field: "updated_at".to_owned(), dir: SortDir::Desc }]);
        }
        let conn = self.db.conn()?;
        let select = chat::Entity::find()
            .filter(chat::Column::DeletedAt.is_null())
            .secure()
            .scope_with(&scope)
            .into_inner();
        let page = paginate_with_odata(
            select,
            &conn,
            &query,
            &super::odata_fields::chat_field_map(),
            ("id", SortDir::Desc),
            LimitCfg { default: 20, max: 100 },
            |m| m,
        )
        .await?;
        let ids: Vec<Uuid> = page.items.iter().map(|c| c.id).collect();
        let child = child_scope(&scope, ctx.subject_tenant_id());
        let counts = message_counts(&conn, &child, ids).await?;
        Ok(page.map_items(|c| {
            let message_count = counts.get(&c.id).copied().unwrap_or(0);
            ChatView { chat: c, message_count }
        }))
    }

    /// `PATCH /v1/chats/{id}` (title only).
    ///
    /// # Errors
    /// `InvalidTitle`, PDP errors, `ChatNotFound`, DB errors.
    pub async fn update_chat_title(
        &self,
        ctx: &SecurityContext,
        chat_id: Uuid,
        title: &str,
    ) -> Result<ChatView, DomainError> {
        let title = validate_title(title)?;
        let access = self.load_chat(ctx, actions::UPDATE, chat_id).await?;
        let conn = self.db.conn()?;
        let now = clock::now();
        let res = chat::Entity::update_many()
            .secure()
            .col_expr(chat::Column::Title, Expr::value(title))
            .col_expr(chat::Column::UpdatedAt, Expr::value(now))
            .filter(
                Condition::all()
                    .add(chat::Column::Id.eq(chat_id))
                    .add(chat::Column::DeletedAt.is_null()),
            )
            .scope_with(&access.scope)
            .exec(&conn)
            .await?;
        if res.rows_affected == 0 {
            return Err(DomainError::ChatNotFound(chat_id));
        }
        self.get_chat_unchecked(&access.scope, &access.child_scope, chat_id).await
    }

    async fn get_chat_unchecked(
        &self,
        scope: &AccessScope,
        child: &AccessScope,
        chat_id: Uuid,
    ) -> Result<ChatView, DomainError> {
        let conn = self.db.conn()?;
        let chat = chat::Entity::find()
            .filter(chat::Column::Id.eq(chat_id))
            .filter(chat::Column::DeletedAt.is_null())
            .secure()
            .scope_with(scope)
            .one(&conn)
            .await?
            .ok_or(DomainError::ChatNotFound(chat_id))?;
        let counts = message_counts(&conn, child, vec![chat_id]).await?;
        let message_count = counts.get(&chat_id).copied().unwrap_or(0);
        Ok(ChatView { chat, message_count })
    }

    /// `DELETE /v1/chats/{id}`: soft-delete, mark attachments for cleanup and
    /// enqueue the chat-cleanup outbox message in one transaction.
    ///
    /// # Errors
    /// PDP errors, `ChatNotFound`, `ChatCleanupPayloadTooLarge`, DB errors.
    pub async fn delete_chat(&self, ctx: &SecurityContext, chat_id: Uuid) -> Result<(), DomainError> {
        let access = self.load_chat(ctx, actions::DELETE, chat_id).await?;
        let outbox = self.outbox.clone();
        let tenant_id = access.chat.tenant_id;
        let scope = access.scope.clone();
        let child = access.child_scope.clone();
        let now = clock::now();
        let wake = self
            .tx(move |tx| {
                Box::pin(async move {
                    let res = chat::Entity::update_many()
                        .secure()
                        .col_expr(chat::Column::DeletedAt, Expr::value(Some(now)))
                        .col_expr(chat::Column::UpdatedAt, Expr::value(now))
                        .filter(
                            Condition::all()
                                .add(chat::Column::Id.eq(chat_id))
                                .add(chat::Column::DeletedAt.is_null()),
                        )
                        .scope_with(&scope)
                        .exec(tx)
                        .await?;
                    if res.rows_affected == 0 {
                        return Err(DomainError::ChatNotFound(chat_id));
                    }
                    attachment::Entity::update_many()
                        .secure()
                        .col_expr(attachment::Column::CleanupStatus, Expr::value(Some("pending")))
                        .col_expr(attachment::Column::CleanupUpdatedAt, Expr::value(Some(now)))
                        .filter(
                            Condition::all()
                                .add(attachment::Column::ChatId.eq(chat_id))
                                .add(attachment::Column::CleanupStatus.is_null()),
                        )
                        .scope_with(&child)
                        .exec(tx)
                        .await?;
                    let payload = ChatCleanupPayload {
                        tenant_id,
                        chat_id,
                        system_request_id: Uuid::new_v4(),
                        reason: "chat_soft_delete".to_owned(),
                        chat_deleted_at: now,
                    };
                    let wake = enqueue_json(&outbox, tx, OutboxKind::ChatCleanup, chat_id, &payload)
                        .await
                        .map_err(|e| match e {
                            DomainError::Internal(msg) if msg.contains("payload too large") => {
                                DomainError::ChatCleanupPayloadTooLarge(msg)
                            }
                            other => other,
                        })?;
                    Ok(wake)
                })
            })
            .await?;
        wake.fire();
        Ok(())
    }
}
