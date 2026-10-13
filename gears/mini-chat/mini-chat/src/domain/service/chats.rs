//! Chat CRUD (DESIGN §3.3 Create/List/Get/Update/Delete Chat).

use std::collections::HashMap;

use sea_orm::sea_query::Expr;
use sea_orm::{ColumnTrait, Condition, EntityTrait, FromQueryResult, QueryFilter, QuerySelect, Set};
use toolkit::{Page, PageInfo};
use toolkit_db::odata::{FieldToColumn, LimitCfg, ODataFieldMapping, paginate_odata};
use toolkit_db::secure::{DBRunner, SecureEntityExt, SecureUpdateExt, secure_insert};
use toolkit_odata::filter::{FieldKind, FilterField};
use toolkit_odata::{ODataOrderBy, ODataQuery, OrderKey, SortDir};
use toolkit_security::{AccessScope, SecurityContext};
use uuid::Uuid;

use super::{MiniChatService, now};
use crate::domain::authz::actions;
use crate::domain::error::{DomainError, DomainResult, Res};
use crate::infra::db::entities::{attachments, chats, messages};
use crate::infra::outbox::{ChatCleanupEvent, fire};

pub const LIMIT_CFG: LimitCfg = LimitCfg { default: 20, max: 100 };

/// Chat with its message count.
#[derive(Debug, Clone)]
pub struct ChatView {
    pub chat: chats::Model,
    pub message_count: i64,
}

/// `OData` fields of `GET /v1/chats`.
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
    type Column = chats::Column;

    fn map_field(field: ChatField) -> chats::Column {
        match field {
            ChatField::UpdatedAt => chats::Column::UpdatedAt,
            ChatField::Id => chats::Column::Id,
            ChatField::Title => chats::Column::Title,
        }
    }
}

impl ODataFieldMapping<ChatField> for ChatMapper {
    type Entity = chats::Entity;

    fn extract_cursor_value(model: &chats::Model, field: ChatField) -> sea_orm::Value {
        match field {
            ChatField::UpdatedAt => sea_orm::Value::TimeDateTimeWithTimeZone(Some(model.updated_at)),
            ChatField::Id => sea_orm::Value::Uuid(Some(model.id)),
            ChatField::Title => sea_orm::Value::String(model.title.clone()),
        }
    }
}

/// Validate and trim a title (1..=255 characters after trim).
///
/// # Errors
/// `INVALID_TITLE`.
pub fn validate_title(title: &str) -> DomainResult<String> {
    let t = title.trim();
    let n = t.chars().count();
    if n == 0 || n > 255 {
        return Err(DomainError::invalid(
            Res::Chat,
            "title",
            "INVALID_TITLE",
            "title must be 1-255 characters after trimming",
        ));
    }
    Ok(t.to_owned())
}

#[derive(Debug, FromQueryResult)]
struct CountRow {
    chat_id: uuid::Uuid,
    c: i64,
}

/// Non-deleted message counts per chat.
pub(crate) async fn message_counts(runner: &impl DBRunner, tenant_scope: &AccessScope, ids: &[Uuid]) -> DomainResult<HashMap<Uuid, i64>> {
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
    let rows: Vec<CountRow> = messages::Entity::find()
        .filter(
            Condition::all()
                .add(messages::Column::ChatId.is_in(ids.to_vec()))
                .add(messages::Column::DeletedAt.is_null()),
        )
        .secure()
        .scope_with(tenant_scope)
        .project_all(runner, |q| {
            q.select_only()
                .column(messages::Column::ChatId)
                .column_as(messages::Column::Id.count(), "c")
                .group_by(messages::Column::ChatId)
                .into_model::<CountRow>()
        })
        .await?;
    Ok(rows.into_iter().map(|r| (r.chat_id, r.c)).collect())
}

impl MiniChatService {
    /// `POST /v1/chats`.
    ///
    /// # Errors
    /// Validation, authorization, model or database errors.
    pub async fn create_chat(&self, ctx: &SecurityContext, title: Option<String>, model: Option<String>) -> DomainResult<ChatView> {
        let title = title.map(|t| validate_title(&t)).transpose()?;
        let scopes = self.scopes(ctx, actions::CREATE, None).await?;
        let snap = self.snapshot(ctx).await?;
        let model = match model {
            Some(m) => snap.find_enabled(&m).ok_or_else(DomainError::invalid_model)?.id.clone(),
            None => snap.default_model().ok_or_else(DomainError::invalid_model)?.id.clone(),
        };
        let ts = now();
        let am = chats::ActiveModel {
            id: Set(Uuid::new_v4()),
            tenant_id: Set(ctx.subject_tenant_id()),
            user_id: Set(ctx.subject_id()),
            model: Set(model),
            title: Set(title),
            is_temporary: Set(false),
            created_at: Set(ts),
            updated_at: Set(ts),
            deleted_at: Set(None),
        };
        let conn = self.db.conn()?;
        let chat = secure_insert::<chats::Entity>(am, &scopes.owner, &conn).await?;
        Ok(ChatView { chat, message_count: 0 })
    }

    /// `GET /v1/chats`.
    ///
    /// # Errors
    /// Authorization, `OData` or database errors.
    pub async fn list_chats(&self, ctx: &SecurityContext, mut query: ODataQuery) -> DomainResult<Page<ChatView>> {
        let scopes = self.scopes(ctx, actions::LIST, None).await?;
        if query.order.is_empty() && query.cursor.is_none() {
            query.order = ODataOrderBy(vec![OrderKey { field: "updated_at".into(), dir: SortDir::Desc }]);
        }
        let conn = self.db.conn()?;
        let select = chats::Entity::find()
            .filter(chats::Column::DeletedAt.is_null())
            .secure()
            .scope_with(&scopes.owner);
        let page: Page<chats::Model> =
            paginate_odata::<ChatField, ChatMapper, _, _, _, _>(select, &conn, &query, ("id", SortDir::Desc), LIMIT_CFG, |m| m)
                .await?;
        let ids: Vec<Uuid> = page.items.iter().map(|c| c.id).collect();
        let counts = message_counts(&conn, &scopes.tenant, &ids).await?;
        let PageInfo { next_cursor, prev_cursor, limit } = page.page_info;
        Ok(Page {
            items: page
                .items
                .into_iter()
                .map(|chat| {
                    let message_count = counts.get(&chat.id).copied().unwrap_or(0);
                    ChatView { chat, message_count }
                })
                .collect(),
            page_info: PageInfo { next_cursor, prev_cursor, limit },
        })
    }

    /// `GET /v1/chats/{id}`.
    ///
    /// # Errors
    /// Authorization, not found or database errors.
    pub async fn get_chat(&self, ctx: &SecurityContext, chat_id: Uuid) -> DomainResult<ChatView> {
        let scopes = self.scopes(ctx, actions::READ, Some(chat_id)).await?;
        let conn = self.db.conn()?;
        let chat = self.load_chat(&conn, &scopes, chat_id).await?;
        let counts = message_counts(&conn, &scopes.tenant, &[chat_id]).await?;
        Ok(ChatView { chat, message_count: counts.get(&chat_id).copied().unwrap_or(0) })
    }

    /// `PATCH /v1/chats/{id}` (title only).
    ///
    /// # Errors
    /// Validation, authorization, not found or database errors.
    pub async fn update_chat_title(&self, ctx: &SecurityContext, chat_id: Uuid, title: &str) -> DomainResult<ChatView> {
        let title = validate_title(title)?;
        let scopes = self.scopes(ctx, actions::UPDATE, Some(chat_id)).await?;
        let conn = self.db.conn()?;
        self.load_chat(&conn, &scopes, chat_id).await?;
        let ts = now();
        chats::Entity::update_many()
            .col_expr(chats::Column::Title, Expr::value(title))
            .col_expr(chats::Column::UpdatedAt, Expr::value(ts))
            .filter(Condition::all().add(chats::Column::Id.eq(chat_id)).add(chats::Column::DeletedAt.is_null()))
            .secure()
            .scope_with(&scopes.owner)
            .exec(&conn)
            .await?;
        let chat = self.load_chat(&conn, &scopes, chat_id).await?;
        let counts = message_counts(&conn, &scopes.tenant, &[chat_id]).await?;
        Ok(ChatView { chat, message_count: counts.get(&chat_id).copied().unwrap_or(0) })
    }

    /// `DELETE /v1/chats/{id}`: soft delete, mark attachments for cleanup and
    /// enqueue the chat-cleanup message in one transaction.
    ///
    /// # Errors
    /// Authorization, not found or database errors.
    pub async fn delete_chat(&self, ctx: &SecurityContext, chat_id: Uuid) -> DomainResult<()> {
        let scopes = self.scopes(ctx, actions::DELETE, Some(chat_id)).await?;
        let outbox = self.outbox.clone();
        let wakes = self
            .tx(move |tx| {
                let scopes = scopes.clone();
                let outbox = outbox.clone();
                Box::pin(async move {
                    let chat = chats::Entity::find()
                        .filter(Condition::all().add(chats::Column::Id.eq(chat_id)).add(chats::Column::DeletedAt.is_null()))
                        .secure()
                        .scope_with(&scopes.owner)
                        .one(tx)
                        .await?
                        .ok_or(DomainError::NotFound(Res::Chat))?;
                    let ts = now();
                    let res = chats::Entity::update_many()
                        .col_expr(chats::Column::DeletedAt, Expr::value(ts))
                        .col_expr(chats::Column::UpdatedAt, Expr::value(ts))
                        .filter(Condition::all().add(chats::Column::Id.eq(chat_id)).add(chats::Column::DeletedAt.is_null()))
                        .secure()
                        .scope_with(&scopes.owner)
                        .exec(tx)
                        .await?;
                    if res.rows_affected == 0 {
                        return Err(DomainError::NotFound(Res::Chat));
                    }
                    attachments::Entity::update_many()
                        .col_expr(attachments::Column::CleanupStatus, Expr::value("pending"))
                        .col_expr(attachments::Column::CleanupUpdatedAt, Expr::value(ts))
                        .filter(
                            Condition::all()
                                .add(attachments::Column::ChatId.eq(chat_id))
                                .add(attachments::Column::CleanupStatus.is_null()),
                        )
                        .secure()
                        .scope_with(&scopes.tenant)
                        .exec(tx)
                        .await?;
                    let ev = ChatCleanupEvent {
                        tenant_id: chat.tenant_id,
                        chat_id,
                        system_request_id: Uuid::new_v4(),
                        reason: "chat_soft_delete".to_owned(),
                        chat_deleted_at: ts,
                    };
                    let wake = outbox.chat_cleanup(tx, &ev).await?;
                    Ok(vec![wake])
                })
            })
            .await?;
        fire(wakes);
        Ok(())
    }
}
