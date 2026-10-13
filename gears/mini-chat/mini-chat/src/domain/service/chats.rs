//! Chat CRUD (DESIGN §3.3 "Create/List/Get/Update/Delete Chat").

use crate::infra::db::WriteTransaction;
use std::sync::Arc;

use sea_orm::sea_query::Expr;
use sea_orm::{ActiveValue, ColumnTrait, Condition, EntityTrait, QueryFilter};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use toolkit_db::odata::{FieldToColumn, LimitCfg, ODataFieldMapping, paginate_odata};
use toolkit_db::secure::{SecureEntityExt, SecureInsertExt, SecureUpdateExt};
use toolkit_odata::filter::{FieldKind, FilterField};
use toolkit_odata::{ODataOrderBy, ODataQuery, OrderKey, Page, SortDir};
use toolkit_security::{AccessScope, SecurityContext};
use uuid::Uuid;

use super::{Core, now};
use crate::domain::authz::{self, actions};
use crate::domain::error::{DomainError, Resource};
use crate::infra::db::entities::{attachment, chat, message};
use crate::infra::outbox::QueueKind;

/// Chat projection returned by the API.
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

/// Chat-cleanup outbox payload (DESIGN §3.6 "Outbox payload and execution semantics").
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatCleanupPayload {
    pub tenant_id: Uuid,
    pub chat_id: Uuid,
    pub system_request_id: Uuid,
    pub reason: String,
    #[serde(with = "time::serde::rfc3339")]
    pub chat_deleted_at: OffsetDateTime,
}

/// `$filter` / `$orderby` fields of `GET /v1/chats` (order matters for the `OpenAPI` text).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ChatField {
    UpdatedAt,
    Id,
    Title,
}

impl FilterField for ChatField {
    const FIELDS: &'static [Self] = &[Self::UpdatedAt, Self::Id, Self::Title];

    fn name(&self) -> &'static str {
        match self {
            Self::UpdatedAt => "updated_at",
            Self::Id => "id",
            Self::Title => "title",
        }
    }

    fn kind(&self) -> FieldKind {
        match self {
            Self::UpdatedAt => FieldKind::DateTimeUtc,
            Self::Id => FieldKind::Uuid,
            Self::Title => FieldKind::String,
        }
    }

    fn nullable(&self) -> bool {
        matches!(self, Self::Title)
    }
}

pub struct ChatMapper;

impl FieldToColumn<ChatField> for ChatMapper {
    type Column = chat::Column;

    fn map_field(field: ChatField) -> chat::Column {
        match field {
            ChatField::UpdatedAt => chat::Column::UpdatedAt,
            ChatField::Id => chat::Column::Id,
            ChatField::Title => chat::Column::Title,
        }
    }
}

impl ODataFieldMapping<ChatField> for ChatMapper {
    type Entity = chat::Entity;

    fn extract_cursor_value(m: &chat::Model, field: ChatField) -> sea_orm::Value {
        match field {
            ChatField::UpdatedAt => sea_orm::Value::TimeDateTimeWithTimeZone(Some(m.updated_at)),
            ChatField::Id => sea_orm::Value::Uuid(Some(m.id)),
            ChatField::Title => sea_orm::Value::String(m.title.clone()),
        }
    }
}

pub const PAGE_LIMITS: LimitCfg = LimitCfg {
    default: 20,
    max: 100,
};

/// Validates and trims a chat title (1..=255 characters after trim).
///
/// # Errors
/// 400 `INVALID_TITLE`.
pub fn validate_title(title: &str) -> Result<String, DomainError> {
    let t = title.trim();
    let n = t.chars().count();
    if n == 0 || n > 255 {
        return Err(DomainError::invalid(
            Resource::Chat,
            "title",
            "INVALID_TITLE",
            "title must be 1-255 characters after trimming",
        ));
    }
    Ok(t.to_owned())
}

/// Tenant-only scope used for child tables of an authorized chat.
#[must_use]
pub fn tenant_scope(tenant_id: Uuid) -> AccessScope {
    AccessScope::for_tenant(tenant_id)
}

/// Loads a non-deleted chat owned by the caller under the PEP scope (404 otherwise).
///
/// # Errors
/// 404 chat, or DB errors.
pub async fn load_owned_chat(
    db: &impl toolkit_db::secure::DBRunner,
    scope: &AccessScope,
    ctx: &SecurityContext,
    chat_id: Uuid,
) -> Result<chat::Model, DomainError> {
    chat::Entity::find()
        .filter(
            Condition::all()
                .add(chat::Column::Id.eq(chat_id))
                .add(chat::Column::UserId.eq(ctx.subject_id()))
                .add(chat::Column::TenantId.eq(ctx.subject_tenant_id()))
                .add(chat::Column::DeletedAt.is_null()),
        )
        .secure()
        .scope_with(scope)
        .one(db)
        .await?
        .ok_or_else(|| DomainError::not_found(Resource::Chat, &chat_id))
}

/// Number of non-deleted messages of a chat.
///
/// # Errors
/// DB errors.
pub async fn message_count(
    db: &impl toolkit_db::secure::DBRunner,
    tenant_id: Uuid,
    chat_id: Uuid,
) -> Result<i64, DomainError> {
    let n = message::Entity::find()
        .filter(message::Column::ChatId.eq(chat_id))
        .filter(message::Column::DeletedAt.is_null())
        .secure()
        .scope_with(&tenant_scope(tenant_id))
        .count(db)
        .await?;
    Ok(i64::try_from(n).unwrap_or(i64::MAX))
}

impl Core {
    /// Authorizes a chat action and loads the caller's chat.
    ///
    /// # Errors
    /// 403/503 from the PEP, 404 when the chat is hidden.
    pub async fn authorize_chat(
        &self,
        ctx: &SecurityContext,
        action: &str,
        chat_id: Uuid,
    ) -> Result<chat::Model, DomainError> {
        let scope = authz::chat_scope(&self.enforcer, ctx, action, Some(chat_id)).await?;
        let conn = self.db.conn()?;
        load_owned_chat(&conn, &scope, ctx, chat_id).await
    }

    /// `POST /v1/chats`.
    ///
    /// # Errors
    /// 400 title/model, 403/503 PEP, 500 policy.
    pub async fn create_chat(
        &self,
        ctx: &SecurityContext,
        title: Option<String>,
        model: Option<String>,
    ) -> Result<ChatView, DomainError> {
        let title = match title {
            Some(t) => Some(validate_title(&t)?),
            None => None,
        };
        let scope = authz::chat_scope(&self.enforcer, ctx, actions::CREATE, None).await?;
        let snapshot = self.policy.current_snapshot(ctx.subject_id()).await?;
        let model_id = match model {
            Some(m) => snapshot
                .find_enabled_model(&m)
                .map(|e| e.id.clone())
                .ok_or_else(DomainError::invalid_model)?,
            None => snapshot
                .default_model()
                .map(|e| e.id.clone())
                .ok_or_else(DomainError::invalid_model)?,
        };
        let ts = now();
        let row = chat::Model {
            id: Uuid::new_v4(),
            tenant_id: ctx.subject_tenant_id(),
            user_id: ctx.subject_id(),
            model: model_id,
            title,
            is_temporary: false,
            created_at: ts,
            updated_at: ts,
            deleted_at: None,
        };
        let am = chat::ActiveModel {
            id: ActiveValue::Set(row.id),
            tenant_id: ActiveValue::Set(row.tenant_id),
            user_id: ActiveValue::Set(row.user_id),
            model: ActiveValue::Set(row.model.clone()),
            title: ActiveValue::Set(row.title.clone()),
            is_temporary: ActiveValue::Set(false),
            created_at: ActiveValue::Set(ts),
            updated_at: ActiveValue::Set(ts),
            deleted_at: ActiveValue::Set(None),
        };
        let conn = self.db.conn()?;
        chat::Entity::insert(am)
            .secure()
            .scope_unchecked(&scope)?
            .exec(&conn)
            .await?;
        Ok(ChatView::from_model(row, 0))
    }

    /// `GET /v1/chats/{id}`.
    ///
    /// # Errors
    /// 404 / PEP errors.
    pub async fn get_chat(
        &self,
        ctx: &SecurityContext,
        chat_id: Uuid,
    ) -> Result<ChatView, DomainError> {
        let c = self.authorize_chat(ctx, actions::READ, chat_id).await?;
        let conn = self.db.conn()?;
        let n = message_count(&conn, c.tenant_id, c.id).await?;
        Ok(ChatView::from_model(c, n))
    }

    /// `GET /v1/chats`.
    ///
    /// # Errors
    /// 400 `OData`, PEP errors.
    pub async fn list_chats(
        &self,
        ctx: &SecurityContext,
        mut query: ODataQuery,
    ) -> Result<Page<ChatView>, DomainError> {
        let scope = authz::chat_scope(&self.enforcer, ctx, actions::LIST, None).await?;
        if query.cursor.is_none() && query.order.is_empty() {
            query.order = ODataOrderBy(vec![OrderKey {
                field: "updated_at".to_owned(),
                dir: SortDir::Desc,
            }]);
        }
        let conn = self.db.conn()?;
        let base = chat::Entity::find()
            .filter(chat::Column::UserId.eq(ctx.subject_id()))
            .filter(chat::Column::TenantId.eq(ctx.subject_tenant_id()))
            .filter(chat::Column::DeletedAt.is_null())
            .secure()
            .scope_with(&scope);
        let page = paginate_odata::<ChatField, ChatMapper, chat::Entity, chat::Model, _, _>(
            base,
            &conn,
            &query,
            ("id", SortDir::Desc),
            PAGE_LIMITS,
            |m| m,
        )
        .await?;
        let mut items = Vec::with_capacity(page.items.len());
        for m in page.items {
            let n = message_count(&conn, m.tenant_id, m.id).await?;
            items.push(ChatView::from_model(m, n));
        }
        Ok(Page {
            items,
            page_info: page.page_info,
        })
    }

    /// `PATCH /v1/chats/{id}` (title only).
    ///
    /// # Errors
    /// 400 title, 404, PEP errors.
    pub async fn rename_chat(
        &self,
        ctx: &SecurityContext,
        chat_id: Uuid,
        title: &str,
    ) -> Result<ChatView, DomainError> {
        let title = validate_title(title)?;
        let c = self.authorize_chat(ctx, actions::UPDATE, chat_id).await?;
        let ts = now();
        let conn = self.db.conn()?;
        chat::Entity::update_many()
            .col_expr(chat::Column::Title, Expr::value(title.clone()))
            .col_expr(chat::Column::UpdatedAt, Expr::value(ts))
            .filter(
                Condition::all()
                    .add(chat::Column::Id.eq(c.id))
                    .add(chat::Column::DeletedAt.is_null()),
            )
            .secure()
            .scope_with(&tenant_scope(c.tenant_id))
            .exec(&conn)
            .await?;
        let n = message_count(&conn, c.tenant_id, c.id).await?;
        let mut m = c;
        m.title = Some(title);
        m.updated_at = ts;
        Ok(ChatView::from_model(m, n))
    }

    /// `DELETE /v1/chats/{id}`: soft delete + chat-cleanup outbox in one transaction.
    ///
    /// # Errors
    /// 404, PEP errors.
    pub async fn delete_chat(
        self: &Arc<Self>,
        ctx: &SecurityContext,
        chat_id: Uuid,
    ) -> Result<(), DomainError> {
        let c = self.authorize_chat(ctx, actions::DELETE, chat_id).await?;
        let core = Arc::clone(self);
        let tenant = c.tenant_id;
        let wake = self
            .db
            .write_transaction(move |tx| {
                Box::pin(async move {
                    let ts = now();
                    let scope = tenant_scope(tenant);
                    let res = chat::Entity::update_many()
                        .col_expr(chat::Column::DeletedAt, Expr::value(Some(ts)))
                        .col_expr(chat::Column::UpdatedAt, Expr::value(ts))
                        .filter(
                            Condition::all()
                                .add(chat::Column::Id.eq(chat_id))
                                .add(chat::Column::DeletedAt.is_null()),
                        )
                        .secure()
                        .scope_with(&scope)
                        .exec(tx)
                        .await?;
                    if res.rows_affected == 0 {
                        return Err(DomainError::not_found(Resource::Chat, &chat_id));
                    }
                    attachment::Entity::update_many()
                        .col_expr(
                            attachment::Column::CleanupStatus,
                            Expr::value(Some("pending".to_owned())),
                        )
                        .col_expr(attachment::Column::CleanupUpdatedAt, Expr::value(Some(ts)))
                        .filter(
                            Condition::all()
                                .add(attachment::Column::ChatId.eq(chat_id))
                                .add(attachment::Column::CleanupStatus.is_null()),
                        )
                        .secure()
                        .scope_with(&scope)
                        .exec(tx)
                        .await?;
                    let payload = ChatCleanupPayload {
                        tenant_id: tenant,
                        chat_id,
                        system_request_id: Uuid::new_v4(),
                        reason: "chat_soft_delete".to_owned(),
                        chat_deleted_at: ts,
                    };
                    core.outbox
                        .enqueue(tx, QueueKind::ChatCleanup, chat_id, &payload)
                        .await
                })
            })
            .await?;
        wake.fire();
        Ok(())
    }
}
