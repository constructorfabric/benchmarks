//! Chat CRUD.

#[allow(unused_imports)]
use sea_orm::{EntityTrait as _, QueryFilter as _};
use std::collections::HashMap;

use sea_orm::sea_query::Expr;
use sea_orm::{ColumnTrait, Condition, FromQueryResult, QuerySelect, Set};
use toolkit_db::odata::{FieldToColumn, LimitCfg, ODataFieldMapping, paginate_odata};
use toolkit_db::secure::{DBRunner, SecureEntityExt, SecureUpdateExt, secure_insert};
use toolkit_odata::filter::{FilterOp, ODataValue};
use toolkit_odata::{ODataQuery, OrderKey, Page, SortDir};
use toolkit_odata_macros::ODataFilterable;
use toolkit_security::{AccessScope, SecurityContext};
use uuid::Uuid;

use super::Service;
use crate::domain::authz::{actions, tenant_scope};
use crate::domain::clock;
use crate::domain::error::{DomainError, DomainResult};
use crate::domain::events::{ChatCleanupEvent, PAYLOAD_CHAT_CLEANUP};
use crate::infra::outbox::fire;
use crate::infra::storage::entity::{attachment, chat, message};

/// Maximum title length (characters, after trim).
pub const MAX_TITLE_CHARS: usize = 255;

/// A chat with its live message count.
#[derive(Debug, Clone)]
pub struct ChatView {
    pub chat: chat::Model,
    pub message_count: i64,
}

/// `OData` fields of the chat list.
#[derive(ODataFilterable)]
#[allow(dead_code)]
pub struct ChatQuery {
    #[odata(filter(kind = "DateTimeUtc"))]
    pub updated_at: time::OffsetDateTime,
    #[odata(filter(kind = "Uuid"))]
    pub id: Uuid,
    #[odata(filter(kind = "String"))]
    pub title: String,
}

/// Column mapping of the chat list.
pub struct ChatMapper;

impl FieldToColumn<ChatQueryFilterField> for ChatMapper {
    type Column = chat::Column;

    fn map_field(field: ChatQueryFilterField) -> chat::Column {
        match field {
            ChatQueryFilterField::UpdatedAt => chat::Column::UpdatedAt,
            ChatQueryFilterField::Id => chat::Column::Id,
            ChatQueryFilterField::Title => chat::Column::Title,
        }
    }

    fn map_value(
        _field: ChatQueryFilterField,
        _op: FilterOp,
        value: &ODataValue,
    ) -> Result<ODataValue, String> {
        Ok(normalize_datetime_value(value))
    }
}

impl ODataFieldMapping<ChatQueryFilterField> for ChatMapper {
    type Entity = chat::Entity;

    fn extract_cursor_value(model: &chat::Model, field: ChatQueryFilterField) -> sea_orm::Value {
        match field {
            ChatQueryFilterField::UpdatedAt => {
                sea_orm::Value::TimeDateTimeWithTimeZone(Some(model.updated_at))
            }
            ChatQueryFilterField::Id => sea_orm::Value::Uuid(Some(model.id)),
            ChatQueryFilterField::Title => {
                sea_orm::Value::String(Some(model.title.clone().unwrap_or_default()))
            }
        }
    }
}

/// Map an `OData` datetime literal onto the stored textual form (UTC, nine
/// fractional digits, shifted by the clock's 1 ns marker) so text comparison
/// in `SQLite` matches instant comparison.
#[must_use]
pub fn normalize_datetime_value(value: &ODataValue) -> ODataValue {
    match value {
        ODataValue::DateTime(dt) => {
            let nanos = dt.timestamp_nanos_opt().unwrap_or_default();
            let ts = time::OffsetDateTime::from_unix_timestamp_nanos(i128::from(nanos) + 1)
                .unwrap_or(time::OffsetDateTime::UNIX_EPOCH);
            ODataValue::String(clock::format_stored(ts))
        }
        other => other.clone(),
    }
}

/// Validate and trim a title.
///
/// # Errors
/// `InvalidTitle` when empty after trim or longer than 255 characters.
pub fn validate_title(raw: &str) -> DomainResult<String> {
    let t = raw.trim();
    if t.is_empty() || t.chars().count() > MAX_TITLE_CHARS {
        return Err(DomainError::InvalidTitle);
    }
    Ok(t.to_owned())
}

#[derive(Debug, FromQueryResult)]
struct ChatCount {
    chat_id: Uuid,
    cnt: i64,
}

impl Service {
    /// Live message counts of chats.
    pub(crate) async fn message_counts<R: DBRunner>(
        runner: &R,
        tenant_id: Uuid,
        chat_ids: &[Uuid],
    ) -> DomainResult<HashMap<Uuid, i64>> {
        if chat_ids.is_empty() {
            return Ok(HashMap::new());
        }
        let rows = message::Entity::find()
            .secure()
            .scope_with(&tenant_scope(tenant_id))
            .filter(
                Condition::all()
                    .add(message::Column::ChatId.is_in(chat_ids.to_vec()))
                    .add(message::Column::DeletedAt.is_null()),
            )
            .project_all(runner, |q| {
                q.select_only()
                    .column(message::Column::ChatId)
                    .column_as(
                        sea_orm::ExprTrait::count(Expr::col(message::Column::Id)),
                        "cnt",
                    )
                    .group_by(message::Column::ChatId)
                    .into_model::<ChatCount>()
            })
            .await?;
        Ok(rows.into_iter().map(|r| (r.chat_id, r.cnt)).collect())
    }

    async fn view(&self, chat: chat::Model) -> DomainResult<ChatView> {
        let conn = self.db.conn()?;
        let counts = Self::message_counts(&conn, chat.tenant_id, &[chat.id]).await?;
        let message_count = counts.get(&chat.id).copied().unwrap_or(0);
        Ok(ChatView {
            chat,
            message_count,
        })
    }

    /// `POST /v1/chats`.
    ///
    /// # Errors
    /// `InvalidTitle`, authz errors, `InvalidModel`.
    pub async fn create_chat(
        &self,
        ctx: &SecurityContext,
        title: Option<String>,
        model: Option<String>,
    ) -> DomainResult<ChatView> {
        let title = match title {
            Some(t) => Some(validate_title(&t)?),
            None => None,
        };
        let scope = self.authz.chat_scope(ctx, actions::CREATE, None).await?;
        let snapshot = self.snapshot(ctx.subject_id()).await?;
        let model_id = match model {
            Some(m) => snapshot
                .find_enabled_model(&m)
                .map(|e| e.id.clone())
                .ok_or_else(|| DomainError::invalid_model(format!("model '{m}' is not enabled")))?,
            None => snapshot
                .default_model()
                .map(|e| e.id.clone())
                .ok_or_else(|| DomainError::invalid_model("no enabled model in the catalog"))?,
        };
        let now = clock::now();
        let am = chat::ActiveModel {
            id: Set(Uuid::new_v4()),
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
        let chat = secure_insert::<chat::Entity>(am, &scope, &conn).await?;
        Ok(ChatView {
            chat,
            message_count: 0,
        })
    }

    /// `GET /v1/chats/{id}`.
    ///
    /// # Errors
    /// Authz errors, `ChatNotFound`.
    pub async fn get_chat(&self, ctx: &SecurityContext, chat_id: Uuid) -> DomainResult<ChatView> {
        let (chat, _) = self.authorized_chat(ctx, actions::READ, chat_id).await?;
        self.view(chat).await
    }

    /// `GET /v1/chats`.
    ///
    /// # Errors
    /// Authz errors, `OData` errors.
    pub async fn list_chats(
        &self,
        ctx: &SecurityContext,
        query: &ODataQuery,
    ) -> DomainResult<Page<ChatView>> {
        let scope = self.authz.chat_scope(ctx, actions::LIST, None).await?;
        let mut query = query.clone();
        if query.cursor.is_none() && query.order.0.is_empty() {
            query.order.0.push(OrderKey {
                field: "updated_at".to_owned(),
                dir: SortDir::Desc,
            });
        }
        let conn = self.db.conn()?;
        let select = chat::Entity::find()
            .filter(chat::Column::DeletedAt.is_null())
            .secure()
            .scope_with(&scope);
        let page = paginate_odata::<ChatQueryFilterField, ChatMapper, _, _, _, _>(
            select,
            &conn,
            &query,
            ("id", SortDir::Desc),
            LimitCfg {
                default: 20,
                max: 100,
            },
            |m| m,
        )
        .await?;
        let ids: Vec<Uuid> = page.items.iter().map(|c| c.id).collect();
        let counts = Self::message_counts(&conn, ctx.subject_tenant_id(), &ids).await?;
        Ok(Page {
            items: page
                .items
                .into_iter()
                .map(|chat| {
                    let message_count = counts.get(&chat.id).copied().unwrap_or(0);
                    ChatView {
                        chat,
                        message_count,
                    }
                })
                .collect(),
            page_info: page.page_info,
        })
    }

    /// `PATCH /v1/chats/{id}` (title only).
    ///
    /// # Errors
    /// `InvalidTitle`, authz errors, `ChatNotFound`.
    pub async fn update_chat_title(
        &self,
        ctx: &SecurityContext,
        chat_id: Uuid,
        title: &str,
    ) -> DomainResult<ChatView> {
        let title = validate_title(title)?;
        let (chat, scope) = self.authorized_chat(ctx, actions::UPDATE, chat_id).await?;
        let now = clock::now();
        let conn = self.db.conn()?;
        let res = chat::Entity::update_many()
            .col_expr(chat::Column::Title, Expr::value(title.clone()))
            .col_expr(chat::Column::UpdatedAt, Expr::value(now))
            .filter(
                Condition::all()
                    .add(chat::Column::Id.eq(chat_id))
                    .add(chat::Column::DeletedAt.is_null()),
            )
            .secure()
            .scope_with(&scope)
            .exec(&conn)
            .await?;
        if res.rows_affected == 0 {
            return Err(DomainError::ChatNotFound { id: chat_id });
        }
        let mut chat = chat;
        chat.title = Some(title);
        chat.updated_at = now;
        self.view(chat).await
    }

    /// `DELETE /v1/chats/{id}`: soft-delete, mark attachments for cleanup and
    /// enqueue the chat cleanup in one transaction.
    ///
    /// # Errors
    /// Authz errors, `ChatNotFound`, `OutboxPayloadTooLarge`.
    pub async fn delete_chat(&self, ctx: &SecurityContext, chat_id: Uuid) -> DomainResult<()> {
        let scope = self
            .authz
            .chat_scope(ctx, actions::DELETE, Some(chat_id))
            .await?;
        let outbox = std::sync::Arc::clone(&self.outbox);
        let wakes = self
            .tx(move |tx| {
                let scope = scope.clone();
                let outbox = std::sync::Arc::clone(&outbox);
                Box::pin(async move {
                    let now = clock::now();
                    let res = chat::Entity::update_many()
                        .col_expr(chat::Column::DeletedAt, Expr::value(Some(now)))
                        .col_expr(chat::Column::UpdatedAt, Expr::value(now))
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
                        return Err(DomainError::ChatNotFound { id: chat_id });
                    }
                    let chat = chat::Entity::find()
                        .secure()
                        .scope_with(&scope)
                        .filter(Condition::all().add(chat::Column::Id.eq(chat_id)))
                        .one(tx)
                        .await?
                        .ok_or(DomainError::ChatNotFound { id: chat_id })?;
                    let child = tenant_scope(chat.tenant_id);
                    attachment::Entity::update_many()
                        .col_expr(
                            attachment::Column::CleanupStatus,
                            Expr::value(Some("pending")),
                        )
                        .col_expr(attachment::Column::CleanupUpdatedAt, Expr::value(Some(now)))
                        .filter(
                            Condition::all()
                                .add(attachment::Column::ChatId.eq(chat_id))
                                .add(attachment::Column::DeletedAt.is_null())
                                .add(attachment::Column::CleanupStatus.is_null()),
                        )
                        .secure()
                        .scope_with(&child)
                        .exec(tx)
                        .await?;
                    let event = ChatCleanupEvent {
                        tenant_id: chat.tenant_id,
                        chat_id,
                        system_request_id: Uuid::new_v4(),
                        reason: "chat_soft_delete".to_owned(),
                        chat_deleted_at: now,
                    };
                    let wake = outbox
                        .enqueue_json(
                            tx,
                            &outbox.queues.chat_cleanup_queue_name,
                            chat_id,
                            PAYLOAD_CHAT_CLEANUP,
                            &event,
                        )
                        .await?;
                    Ok(vec![wake])
                })
            })
            .await?;
        fire(wakes);
        Ok(())
    }
}

/// Scope of a single chat owned by the caller (used by child services).
#[must_use]
pub fn owner_scope(ctx: &SecurityContext) -> AccessScope {
    crate::domain::authz::with_owner(&AccessScope::for_tenant(ctx.subject_tenant_id()), ctx)
}
