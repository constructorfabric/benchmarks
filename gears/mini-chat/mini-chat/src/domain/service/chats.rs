//! Chat CRUD (DESIGN §3.3).

use std::sync::{Arc, LazyLock};

use chrono::{DateTime, Utc};
use sea_orm::sea_query::Expr;
use sea_orm::{ColumnTrait, Condition, EntityTrait, QueryFilter};
use serde::{Deserialize, Serialize};
use toolkit_db::odata::{FieldMap, LimitCfg, paginate_with_odata};
use toolkit_odata::filter::FieldKind;
use toolkit_db::secure::{DBRunner, SecureEntityExt, SecureInsertExt, SecureUpdateExt};
use toolkit_odata::{ODataOrderBy, ODataQuery, OrderKey, Page, SortDir};
use toolkit_security::{AccessScope, SecurityContext};
use uuid::Uuid;

use super::{AppServices, now, policy};
use crate::domain::error::{DomainError, NotFoundKind};
use crate::infra::db::entity::{attachments, chats, messages};

/// Chat as returned by the API.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatView {
    pub id: Uuid,
    pub model: String,
    pub title: Option<String>,
    pub is_temporary: bool,
    pub message_count: i64,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Chat-cleanup outbox payload (DESIGN §3.6 "Outbox payload and execution semantics").
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ChatCleanupPayload {
    pub tenant_id: Uuid,
    pub chat_id: Uuid,
    pub system_request_id: Uuid,
    pub reason: String,
    pub chat_deleted_at: DateTime<Utc>,
}

/// Validates and trims a chat title (1..=255 characters after trim).
///
/// # Errors
/// `InvalidTitle`.
pub fn validate_title(title: &str) -> Result<String, DomainError> {
    let t = title.trim();
    let n = t.chars().count();
    if n == 0 || n > 255 {
        return Err(DomainError::InvalidTitle);
    }
    Ok(t.to_owned())
}

static CHAT_FIELDS: LazyLock<FieldMap<chats::Entity>> = LazyLock::new(|| {
    FieldMap::new()
        .insert_with_extractor("updated_at", chats::Column::UpdatedAt, FieldKind::DateTimeUtc, |m: &chats::Model| {
            m.updated_at.to_rfc3339()
        })
        .insert_with_extractor("id", chats::Column::Id, FieldKind::Uuid, |m: &chats::Model| m.id.to_string())
        .insert_with_extractor("title", chats::Column::Title, FieldKind::String, |m: &chats::Model| {
            m.title.clone().unwrap_or_default()
        })
});

/// Loads a non-deleted chat under a scope.
///
/// # Errors
/// `NotFound(Chat)` when missing, deleted or foreign.
pub async fn load_chat(
    runner: &impl DBRunner,
    scope: &AccessScope,
    chat_id: Uuid,
) -> Result<chats::Model, DomainError> {
    chats::Entity::find()
        .secure()
        .scope_with(scope)
        .filter(
            Condition::all()
                .add(chats::Column::Id.eq(chat_id))
                .add(chats::Column::DeletedAt.is_null()),
        )
        .one(runner)
        .await?
        .ok_or(DomainError::NotFound(NotFoundKind::Chat))
}

/// Number of non-deleted messages of a chat.
///
/// # Errors
/// Database failure.
pub async fn message_count(runner: &impl DBRunner, tenant_id: Uuid, chat_id: Uuid) -> Result<i64, DomainError> {
    let n = messages::Entity::find()
        .secure()
        .scope_with(&AccessScope::for_tenant(tenant_id))
        .filter(
            Condition::all()
                .add(messages::Column::ChatId.eq(chat_id))
                .add(messages::Column::DeletedAt.is_null()),
        )
        .count(runner)
        .await?;
    Ok(i64::try_from(n).unwrap_or(i64::MAX))
}

fn view(m: chats::Model, message_count: i64) -> ChatView {
    ChatView {
        id: m.id,
        model: m.model,
        title: m.title,
        is_temporary: m.is_temporary,
        message_count,
        created_at: m.created_at,
        updated_at: m.updated_at,
    }
}

impl AppServices {
    /// `POST /v1/chats`.
    ///
    /// # Errors
    /// `InvalidTitle`, `InvalidModel`, authorization and database errors.
    pub async fn create_chat(
        &self,
        ctx: &SecurityContext,
        title: Option<String>,
        model: Option<String>,
    ) -> Result<ChatView, DomainError> {
        let title = title.as_deref().map(validate_title).transpose()?;
        let scope = self.authz.chat_scope(ctx, "create", None).await?;
        let snapshot = policy::current_snapshot(self.policy.as_ref(), ctx.subject_id()).await?;
        let model_id = match model {
            Some(m) => snapshot
                .enabled_model(&m)
                .map(|e| e.id.clone())
                .ok_or(DomainError::InvalidModel)?,
            None => snapshot
                .default_model()
                .map(|e| e.id.clone())
                .ok_or(DomainError::InvalidModel)?,
        };
        let ts = now();
        let am = chats::ActiveModel {
            id: sea_orm::Set(Uuid::new_v4()),
            tenant_id: sea_orm::Set(ctx.subject_tenant_id()),
            user_id: sea_orm::Set(ctx.subject_id()),
            model: sea_orm::Set(model_id),
            title: sea_orm::Set(title),
            is_temporary: sea_orm::Set(false),
            created_at: sea_orm::Set(ts),
            updated_at: sea_orm::Set(ts),
            deleted_at: sea_orm::Set(None),
        };
        let conn = self.conn()?;
        let id = am.id.clone().unwrap();
        chats::Entity::insert(am)
            .secure()
            .scope_unchecked(&scope)?
            .exec(&conn)
            .await?;
        let m = load_chat(&conn, &scope, id).await?;
        Ok(view(m, 0))
    }

    /// `GET /v1/chats/{id}`.
    ///
    /// # Errors
    /// `NotFound(Chat)`, authorization and database errors.
    pub async fn get_chat(&self, ctx: &SecurityContext, chat_id: Uuid) -> Result<ChatView, DomainError> {
        let scope = self.authz.chat_scope(ctx, "read", Some(chat_id)).await?;
        let conn = self.conn()?;
        let m = load_chat(&conn, &scope, chat_id).await?;
        let count = message_count(&conn, m.tenant_id, m.id).await?;
        Ok(view(m, count))
    }

    /// `PATCH /v1/chats/{id}` (title only).
    ///
    /// # Errors
    /// `InvalidTitle`, `NotFound(Chat)`, authorization and database errors.
    pub async fn update_chat_title(
        &self,
        ctx: &SecurityContext,
        chat_id: Uuid,
        title: &str,
    ) -> Result<ChatView, DomainError> {
        let title = validate_title(title)?;
        let scope = self.authz.chat_scope(ctx, "update", Some(chat_id)).await?;
        let conn = self.conn()?;
        let rows = chats::Entity::update_many()
            .col_expr(chats::Column::Title, Expr::value(Some(title)))
            .col_expr(chats::Column::UpdatedAt, Expr::value(now()))
            .filter(
                Condition::all()
                    .add(chats::Column::Id.eq(chat_id))
                    .add(chats::Column::DeletedAt.is_null()),
            )
            .secure()
            .scope_with(&scope)
            .exec(&conn)
            .await?
            .rows_affected;
        if rows == 0 {
            return Err(DomainError::NotFound(NotFoundKind::Chat));
        }
        let m = load_chat(&conn, &scope, chat_id).await?;
        let count = message_count(&conn, m.tenant_id, m.id).await?;
        Ok(view(m, count))
    }

    /// `DELETE /v1/chats/{id}`: soft delete + attachment cleanup marking +
    /// chat-cleanup outbox message in one transaction.
    ///
    /// # Errors
    /// `NotFound(Chat)`, `PayloadTooLarge`, authorization and database errors.
    pub async fn delete_chat(&self, ctx: &SecurityContext, chat_id: Uuid) -> Result<(), DomainError> {
        let scope = self.authz.chat_scope(ctx, "delete", Some(chat_id)).await?;
        let tenant_id = ctx.subject_tenant_id();
        let outbox = Arc::clone(&self.outbox);
        let wake = self
            .db
            .transaction_ref_mapped(move |tx| {
                Box::pin(async move {
                    crate::domain::service::lock_for_write(tx).await?;
                    let ts = now();
                    let chat = load_chat(tx, &scope, chat_id).await?;
                    let rows = chats::Entity::update_many()
                        .col_expr(chats::Column::DeletedAt, Expr::value(Some(ts)))
                        .col_expr(chats::Column::UpdatedAt, Expr::value(ts))
                        .filter(
                            Condition::all()
                                .add(chats::Column::Id.eq(chat_id))
                                .add(chats::Column::DeletedAt.is_null()),
                        )
                        .secure()
                        .scope_with(&scope)
                        .exec(tx)
                        .await?
                        .rows_affected;
                    if rows == 0 {
                        return Err(DomainError::NotFound(NotFoundKind::Chat));
                    }
                    attachments::Entity::update_many()
                        .col_expr(attachments::Column::CleanupStatus, Expr::value(Some("pending")))
                        .col_expr(attachments::Column::CleanupUpdatedAt, Expr::value(Some(ts)))
                        .filter(
                            Condition::all()
                                .add(attachments::Column::ChatId.eq(chat_id))
                                .add(attachments::Column::DeletedAt.is_null())
                                .add(attachments::Column::CleanupStatus.is_null()),
                        )
                        .secure()
                        .scope_with(&AccessScope::for_tenant(chat.tenant_id))
                        .exec(tx)
                        .await?;
                    let payload = ChatCleanupPayload {
                        tenant_id: chat.tenant_id,
                        chat_id,
                        system_request_id: Uuid::new_v4(),
                        reason: "chat_soft_delete".to_owned(),
                        chat_deleted_at: ts,
                    };
                    outbox.chat_cleanup(tx, &payload).await
                })
            })
            .await?;
        let _ = tenant_id;
        wake.fire();
        Ok(())
    }

    /// `GET /v1/chats`.
    ///
    /// # Errors
    /// OData, authorization and database errors.
    pub async fn list_chats(
        &self,
        ctx: &SecurityContext,
        query: &ODataQuery,
    ) -> Result<Page<ChatView>, DomainError> {
        let scope = self.authz.chat_scope(ctx, "list", None).await?;
        let conn = self.conn()?;
        let mut q = query.clone();
        if q.cursor.is_none() && q.order.is_empty() {
            q.order = ODataOrderBy(vec![
                OrderKey {
                    field: "updated_at".into(),
                    dir: SortDir::Desc,
                },
                OrderKey {
                    field: "id".into(),
                    dir: SortDir::Desc,
                },
            ]);
        }
        let select = chats::Entity::find()
            .secure()
            .scope_with(&scope)
            .filter(Condition::all().add(chats::Column::DeletedAt.is_null()))
            .into_inner();
        let page = paginate_with_odata::<chats::Entity, chats::Model, _, _>(
            select,
            &conn,
            &q,
            &CHAT_FIELDS,
            ("id", SortDir::Desc),
            LimitCfg { default: 20, max: 100 },
            |m| m,
        )
        .await?;
        let mut items = Vec::with_capacity(page.items.len());
        for m in page.items {
            let count = message_count(&conn, m.tenant_id, m.id).await?;
            items.push(view(m, count));
        }
        Ok(Page {
            items,
            page_info: page.page_info,
        })
    }
}

/// Bumps `chats.updated_at` inside a transaction.
///
/// # Errors
/// Database failure.
pub async fn touch_chat(runner: &impl DBRunner, tenant_id: Uuid, chat_id: Uuid) -> Result<(), DomainError> {
    chats::Entity::update_many()
        .col_expr(chats::Column::UpdatedAt, Expr::value(now()))
        .filter(Condition::all().add(chats::Column::Id.eq(chat_id)))
        .secure()
        .scope_with(&AccessScope::for_tenant(tenant_id))
        .exec(runner)
        .await?;
    Ok(())
}
