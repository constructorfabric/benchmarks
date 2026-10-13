//! Chat CRUD (OWNER: REST CRUD work package).
//!
//! `POST/GET/PATCH/DELETE /v1/chats[/{id}]` and the chat list (DESIGN §3.3).

use std::collections::HashMap;
use std::sync::Arc;

use sea_orm::sea_query::Expr;
use sea_orm::{
    ActiveValue, ColumnTrait, Condition, EntityTrait, FromQueryResult, QueryFilter, QuerySelect,
};
use time::OffsetDateTime;
use toolkit_db::odata::{FieldToColumn, LimitCfg, ODataFieldMapping, paginate_odata};
use toolkit_db::secure::{DBRunner, SecureEntityExt, SecureUpdateExt, secure_insert};
use toolkit_odata::{ODataOrderBy, ODataQuery, OrderKey, Page, SortDir};
use toolkit_security::{AccessScope, SecurityContext};
use uuid::Uuid;

use crate::api::rest::dto::{ChatDetailDto, ChatDetailDtoFilterField, CreateChatReq};
use crate::domain::authz::{self, actions};
use crate::domain::error::{DomainError, reasons, resource_types};
use crate::domain::outbox_payloads::ChatCleanupEvent;
use crate::domain::service::Deps;
use crate::domain::service::chat_access::load_chat;
use crate::infra::db::entity::{attachment, chat, message};

/// Maximum title length (characters, after trimming).
pub const MAX_TITLE_CHARS: usize = 255;

/// Page size configuration of the list endpoints.
pub const LIST_LIMITS: LimitCfg = LimitCfg {
    default: 20,
    max: 100,
};

/// Validates and trims a chat title (1..=255 characters after trim).
///
/// # Errors
/// 400 `invalid_argument` (`title` / `INVALID_TITLE`).
pub fn validate_title(raw: &str) -> Result<String, DomainError> {
    let trimmed = raw.trim();
    let len = trimmed.chars().count();
    if len == 0 {
        return Err(DomainError::invalid(
            resource_types::CHAT,
            "title",
            reasons::INVALID_TITLE,
            "Title must not be empty or whitespace-only",
        ));
    }
    if len > MAX_TITLE_CHARS {
        return Err(DomainError::invalid(
            resource_types::CHAT,
            "title",
            reasons::INVALID_TITLE,
            format!("Title must be at most {MAX_TITLE_CHARS} characters"),
        ));
    }
    Ok(trimmed.to_owned())
}

/// Builds the wire `ChatDetail` of a chat row.
#[must_use]
pub fn chat_to_dto(m: &chat::Model, message_count: i64) -> ChatDetailDto {
    ChatDetailDto {
        id: m.id,
        model: m.model.clone(),
        title: m.title.clone(),
        is_temporary: m.is_temporary,
        message_count,
        created_at: m.created_at,
        updated_at: m.updated_at,
    }
}

/// `$filter` / `$orderby` mapping of the chat list.
pub struct ChatODataMapper;

impl FieldToColumn<ChatDetailDtoFilterField> for ChatODataMapper {
    type Column = chat::Column;

    fn map_field(field: ChatDetailDtoFilterField) -> chat::Column {
        match field {
            ChatDetailDtoFilterField::Id => chat::Column::Id,
            ChatDetailDtoFilterField::Title => chat::Column::Title,
            ChatDetailDtoFilterField::UpdatedAt => chat::Column::UpdatedAt,
        }
    }

    fn map_value(
        field: ChatDetailDtoFilterField,
        _op: toolkit_odata::filter::FilterOp,
        value: &toolkit_odata::filter::ODataValue,
    ) -> Result<toolkit_odata::filter::ODataValue, String> {
        if matches!(field, ChatDetailDtoFilterField::Id) {
            return crate::domain::service::chats::map_uuid_value(value);
        }
        Ok(value.clone())
    }
}

impl ODataFieldMapping<ChatDetailDtoFilterField> for ChatODataMapper {
    type Entity = chat::Entity;

    fn cursor_kind(field: ChatDetailDtoFilterField) -> toolkit_odata::filter::FieldKind {
        match field {
            ChatDetailDtoFilterField::Id => toolkit_odata::filter::FieldKind::Uuid,
            other => toolkit_odata::filter::FilterField::kind(&other),
        }
    }

    fn extract_cursor_value(m: &chat::Model, field: ChatDetailDtoFilterField) -> sea_orm::Value {
        match field {
            ChatDetailDtoFilterField::Id => sea_orm::Value::Uuid(Some(m.id)),
            ChatDetailDtoFilterField::Title => sea_orm::Value::String(m.title.clone()),
            ChatDetailDtoFilterField::UpdatedAt => {
                sea_orm::Value::TimeDateTimeWithTimeZone(Some(m.updated_at))
            }
        }
    }
}

#[derive(Debug, FromQueryResult)]
struct ChatCount {
    chat_id: Uuid,
    cnt: i64,
}

/// Number of non-deleted messages per chat (missing chats have 0).
///
/// `child_scope` must be the tenant-only scope of chats that were already authorized.
///
/// # Errors
/// Database errors.
pub async fn message_counts(
    runner: &impl DBRunner,
    child_scope: &AccessScope,
    chat_ids: &[Uuid],
) -> Result<HashMap<Uuid, i64>, DomainError> {
    if chat_ids.is_empty() {
        return Ok(HashMap::new());
    }
    let rows = message::Entity::find()
        .filter(message::Column::ChatId.is_in(chat_ids.iter().copied()))
        .filter(message::Column::DeletedAt.is_null())
        .secure()
        .scope_with(child_scope)
        .project_all(runner, |q| {
            q.select_only()
                .column(message::Column::ChatId)
                .column_as(message::Column::Id.count(), "cnt")
                .group_by(message::Column::ChatId)
                .into_model::<ChatCount>()
        })
        .await?;
    Ok(rows.into_iter().map(|r| (r.chat_id, r.cnt)).collect())
}

/// Number of non-deleted messages of one chat.
///
/// # Errors
/// Database errors.
pub async fn message_count(
    runner: &impl DBRunner,
    child_scope: &AccessScope,
    chat_id: Uuid,
) -> Result<i64, DomainError> {
    let n = message::Entity::find()
        .filter(message::Column::ChatId.eq(chat_id))
        .filter(message::Column::DeletedAt.is_null())
        .secure()
        .scope_with(child_scope)
        .count(runner)
        .await?;
    Ok(i64::try_from(n).unwrap_or(i64::MAX))
}

pub struct ChatService {
    deps: Arc<Deps>,
}

impl ChatService {
    #[must_use]
    pub fn new(deps: Arc<Deps>) -> Self {
        Self { deps }
    }

    /// `POST /v1/chats`.
    ///
    /// # Errors
    /// 400 `INVALID_TITLE` / `INVALID_MODEL`, 403/503 from the PEP, 500 on plugin/DB failure.
    pub async fn create(
        &self,
        ctx: &SecurityContext,
        req: CreateChatReq,
    ) -> Result<ChatDetailDto, DomainError> {
        // 1. Title first (before the PEP and the model lookup).
        let title = req.title.as_deref().map(validate_title).transpose()?;

        // 2. PEP.
        let scope = authz::chat_scope(&self.deps.enforcer, ctx, actions::CREATE, None).await?;

        // 3. Model.
        let snapshot = self.deps.policy.current_snapshot(ctx.subject_id()).await?;
        let entry = match req.model.as_deref() {
            Some(id) => snapshot.find_enabled(id),
            None => snapshot.default_model(),
        }
        .ok_or_else(DomainError::invalid_model)?;

        // 4. Insert.
        let now = OffsetDateTime::now_utc();
        let am = chat::ActiveModel {
            id: ActiveValue::Set(Uuid::new_v4()),
            tenant_id: ActiveValue::Set(ctx.subject_tenant_id()),
            user_id: ActiveValue::Set(ctx.subject_id()),
            model: ActiveValue::Set(entry.id.clone()),
            title: ActiveValue::Set(title),
            is_temporary: ActiveValue::Set(false),
            created_at: ActiveValue::Set(now),
            updated_at: ActiveValue::Set(now),
            deleted_at: ActiveValue::Set(None),
        };
        let conn = self.deps.db.conn()?;
        let model = secure_insert::<chat::Entity>(am, &scope, &conn).await?;
        Ok(chat_to_dto(&model, 0))
    }

    /// `GET /v1/chats/{id}`.
    ///
    /// # Errors
    /// 404 when missing/deleted/foreign, 403/503 from the PEP.
    pub async fn get(
        &self,
        ctx: &SecurityContext,
        chat_id: Uuid,
    ) -> Result<ChatDetailDto, DomainError> {
        let ac = load_chat(&self.deps, ctx, chat_id, actions::READ).await?;
        let conn = self.deps.db.conn()?;
        let count = message_count(&conn, &ac.child_scope, ac.chat.id).await?;
        Ok(chat_to_dto(&ac.chat, count))
    }

    /// `PATCH /v1/chats/{id}` (title only).
    ///
    /// # Errors
    /// 400 `INVALID_TITLE` (before the PEP), 404, 403/503.
    pub async fn update_title(
        &self,
        ctx: &SecurityContext,
        chat_id: Uuid,
        raw_title: &str,
    ) -> Result<ChatDetailDto, DomainError> {
        let title = validate_title(raw_title)?;
        let ac = load_chat(&self.deps, ctx, chat_id, actions::UPDATE).await?;
        let now = OffsetDateTime::now_utc();
        let conn = self.deps.db.conn()?;
        let rows = chat::Entity::update_many()
            .col_expr(chat::Column::Title, Expr::value(title.clone()))
            .col_expr(chat::Column::UpdatedAt, Expr::value(now))
            .filter(
                Condition::all()
                    .add(chat::Column::Id.eq(ac.chat.id))
                    .add(chat::Column::DeletedAt.is_null()),
            )
            .secure()
            .scope_with(&ac.scope)
            .exec(&conn)
            .await?
            .rows_affected;
        if rows == 0 {
            return Err(DomainError::chat_not_found());
        }
        let mut updated = ac.chat;
        updated.title = Some(title);
        updated.updated_at = now;
        let count = message_count(&conn, &ac.child_scope, updated.id).await?;
        Ok(chat_to_dto(&updated, count))
    }

    /// `DELETE /v1/chats/{id}`: soft delete + attachment cleanup marking + chat-cleanup
    /// outbox message, in one transaction. Running turns are not cancelled.
    ///
    /// # Errors
    /// 404 (also on a concurrent delete), 403/503, 400 when the outbox payload is too large.
    pub async fn delete(&self, ctx: &SecurityContext, chat_id: Uuid) -> Result<(), DomainError> {
        let ac = load_chat(&self.deps, ctx, chat_id, actions::DELETE).await?;
        let scope = ac.scope;
        let child_scope = ac.child_scope;
        let tenant_id = ac.chat.tenant_id;
        let outbox = Arc::clone(&self.deps.outbox);
        let wake = self
            .deps
            .db
            .transaction(move |tx| {
                Box::pin(async move {
                    let now = OffsetDateTime::now_utc();
                    let rows = chat::Entity::update_many()
                        .col_expr(chat::Column::DeletedAt, Expr::value(now))
                        .col_expr(chat::Column::UpdatedAt, Expr::value(now))
                        .filter(
                            Condition::all()
                                .add(chat::Column::Id.eq(chat_id))
                                .add(chat::Column::DeletedAt.is_null()),
                        )
                        .secure()
                        .scope_with(&scope)
                        .exec(tx)
                        .await?
                        .rows_affected;
                    if rows == 0 {
                        return Err(DomainError::chat_not_found());
                    }
                    attachment::Entity::update_many()
                        .col_expr(attachment::Column::CleanupStatus, Expr::value("pending"))
                        .col_expr(attachment::Column::CleanupUpdatedAt, Expr::value(now))
                        .filter(
                            Condition::all()
                                .add(attachment::Column::ChatId.eq(chat_id))
                                .add(attachment::Column::DeletedAt.is_null())
                                .add(attachment::Column::CleanupStatus.is_null()),
                        )
                        .secure()
                        .scope_with(&child_scope)
                        .exec(tx)
                        .await?;
                    let ev = ChatCleanupEvent {
                        tenant_id,
                        chat_id,
                        system_request_id: Uuid::new_v4(),
                        reason: "chat_soft_delete".to_owned(),
                        chat_deleted_at: now,
                    };
                    outbox.enqueue_chat_cleanup(tx, &ev).await
                })
            })
            .await?;
        wake.fire();
        Ok(())
    }

    /// `GET /v1/chats`: the caller's non-deleted chats, default order `updated_at desc, id desc`.
    ///
    /// # Errors
    /// 400 OData errors (`DomainError::OData`), 403/503 from the PEP.
    pub async fn list(
        &self,
        ctx: &SecurityContext,
        mut query: ODataQuery,
    ) -> Result<Page<ChatDetailDto>, DomainError> {
        let scope = authz::chat_scope(&self.deps.enforcer, ctx, actions::LIST, None).await?;
        if query.cursor.is_none() && query.order.is_empty() {
            query.order = ODataOrderBy(vec![OrderKey {
                field: "updated_at".to_owned(),
                dir: SortDir::Desc,
            }]);
        }
        let conn = self.deps.db.conn()?;
        let select = chat::Entity::find()
            .filter(chat::Column::DeletedAt.is_null())
            .filter(chat::Column::UserId.eq(ctx.subject_id()))
            .secure()
            .scope_with(&scope);
        let order = title_order::effective_order(&query, ("id", SortDir::Desc))?;
        let page: Page<chat::Model> = if title_order::orders_by_title(&order) {
            // Nullable title: ordered / paginated on COALESCE(title, '').
            title_order::paginate(select, &conn, &query, &order, LIST_LIMITS).await?
        } else {
            paginate_odata::<ChatDetailDtoFilterField, ChatODataMapper, _, _, _, _>(
                select,
                &conn,
                &query,
                ("id", SortDir::Desc),
                LIST_LIMITS,
                |m| m,
            )
            .await?
        };
        let ids: Vec<Uuid> = page.items.iter().map(|c| c.id).collect();
        let counts = message_counts(&conn, &scope.tenant_only(), &ids).await?;
        Ok(page.map_items(|c| {
            let n = counts.get(&c.id).copied().unwrap_or(0);
            chat_to_dto(&c, n)
        }))
    }
}

pub mod title_order;

#[cfg(test)]
#[path = "chats_test_rows.rs"]
pub(crate) mod test_rows;

#[cfg(test)]
#[path = "chats_tests.rs"]
mod chats_tests;


/// Accepts `id eq '<uuid>'` (quoted, DESIGN §3.3) by converting the string literal to a UUID.
pub(crate) fn map_uuid_value(
    value: &toolkit_odata::filter::ODataValue,
) -> Result<toolkit_odata::filter::ODataValue, String> {
    match value {
        toolkit_odata::filter::ODataValue::String(s) => uuid::Uuid::parse_str(s.trim())
            .map(toolkit_odata::filter::ODataValue::Uuid)
            .map_err(|_| format!("invalid UUID '{s}' for field id")),
        other => Ok(other.clone()),
    }
}
