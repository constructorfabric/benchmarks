//! Message repository.

use sea_orm::sea_query::Expr;
use sea_orm::{ActiveValue, ColumnTrait, Condition, EntityTrait, QueryFilter};
use toolkit_db::odata::{FieldToColumn, LimitCfg, ODataFieldMapping, paginate_odata};
use toolkit_db::secure::{AccessScope, DBRunner, SecureEntityExt, SecureUpdateExt};
use toolkit_odata::{ODataQuery, Page, SortDir};
use uuid::Uuid;

use super::chats::odata_err;
use super::{Db, MessageKey, now};
use super::super::entity::messages;
use crate::api::rest::dto::MessageListQueryFilterField;
use crate::error::DomainError;

/// New message row.
pub struct NewMessage {
    /// Message id.
    pub id: Uuid,
    /// Tenant.
    pub tenant_id: Uuid,
    /// Parent chat.
    pub chat_id: Uuid,
    /// Turn correlation key.
    pub request_id: Option<Uuid>,
    /// `user` | `assistant` | `system`.
    pub role: String,
    /// Content.
    pub content: String,
    /// Effective model (assistant messages).
    pub model: Option<String>,
    /// Provider response id (assistant messages).
    pub provider_response_id: Option<String>,
    /// Actual input tokens.
    pub input_tokens: i64,
    /// Actual output tokens.
    pub output_tokens: i64,
    /// Cached input tokens.
    pub cache_read_input_tokens: i64,
    /// Cache-written input tokens.
    pub cache_write_input_tokens: i64,
    /// Reasoning tokens.
    pub reasoning_tokens: i64,
}

/// `OData` field mapping for the message list.
pub struct MessageODataMapping;

impl FieldToColumn<MessageListQueryFilterField> for MessageODataMapping {
    type Column = messages::Column;

    fn map_field(field: MessageListQueryFilterField) -> Self::Column {
        match field {
            MessageListQueryFilterField::CreatedAt => messages::Column::CreatedAt,
            MessageListQueryFilterField::Id => messages::Column::Id,
            MessageListQueryFilterField::Role => messages::Column::Role,
        }
    }
}

impl ODataFieldMapping<MessageListQueryFilterField> for MessageODataMapping {
    type Entity = messages::Entity;

    fn extract_cursor_value(
        model: &messages::Model,
        field: MessageListQueryFilterField,
    ) -> sea_orm::Value {
        match field {
            MessageListQueryFilterField::CreatedAt => {
                sea_orm::Value::TimeDateTimeWithTimeZone(Some(model.created_at))
            }
            MessageListQueryFilterField::Id => sea_orm::Value::Uuid(Some(model.id)),
            MessageListQueryFilterField::Role => model.role.clone().into(),
        }
    }
}

/// Message repository.
#[derive(Clone)]
pub struct MessageRepo {
    #[allow(dead_code)]
    db: Db,
}

impl MessageRepo {
    /// Create a repository.
    #[must_use]
    pub fn new(db: Db) -> Self {
        Self { db }
    }

    fn base_select(
        scope: &AccessScope,
        tenant_id: Uuid,
        chat_id: Uuid,
    ) -> toolkit_db::secure::SecureSelect<messages::Entity, toolkit_db::secure::Scoped> {
        messages::Entity::find()
            .secure()
            .scope_with(&super::tenant_scope(scope))
            .filter(Condition::all().add(messages::Column::TenantId.eq(tenant_id)))
            .filter(Condition::all().add(messages::Column::ChatId.eq(chat_id)))
    }

    /// Insert a message.
    pub async fn insert(
        &self,
        runner: &impl DBRunner,
        scope: &AccessScope,
        new: NewMessage,
    ) -> Result<messages::Model, DomainError> {
        let am = messages::ActiveModel {
            id: ActiveValue::Set(new.id),
            tenant_id: ActiveValue::Set(new.tenant_id),
            chat_id: ActiveValue::Set(new.chat_id),
            request_id: ActiveValue::Set(new.request_id),
            role: ActiveValue::Set(new.role),
            content: ActiveValue::Set(new.content),
            content_type: ActiveValue::Set("text".to_owned()),
            token_estimate: ActiveValue::Set(0),
            provider_response_id: ActiveValue::Set(new.provider_response_id),
            request_kind: ActiveValue::Set(Some("chat".to_owned())),
            features_used: ActiveValue::Set(serde_json::json!([])),
            input_tokens: ActiveValue::Set(new.input_tokens),
            output_tokens: ActiveValue::Set(new.output_tokens),
            cache_read_input_tokens: ActiveValue::Set(new.cache_read_input_tokens),
            cache_write_input_tokens: ActiveValue::Set(new.cache_write_input_tokens),
            reasoning_tokens: ActiveValue::Set(new.reasoning_tokens),
            model: ActiveValue::Set(new.model),
            is_compressed: ActiveValue::Set(false),
            created_at: ActiveValue::Set(now()),
            deleted_at: ActiveValue::Set(None),
        };
        let model = toolkit_db::secure::secure_insert::<messages::Entity>(am, &super::tenant_scope(scope), runner).await?;
        Ok(model)
    }

    /// List messages of a chat (not deleted), cursor-paginated.
    pub async fn list_page(
        &self,
        runner: &impl DBRunner,
        scope: &AccessScope,
        tenant_id: Uuid,
        chat_id: Uuid,
        query: &ODataQuery,
    ) -> Result<Page<messages::Model>, DomainError> {
        let select = Self::base_select(scope, tenant_id, chat_id)
            .filter(Condition::all().add(messages::Column::DeletedAt.is_null()));
        let page = paginate_odata::<MessageListQueryFilterField, MessageODataMapping, _, _, _, _>(
            select,
            runner,
            query,
            ("created_at", SortDir::Asc),
            LimitCfg {
                default: 20,
                max: 100,
            },
            |m| m,
        )
        .await
        .map_err(|e| {
            tracing::debug!("mini-chat: message list pagination failed: {e}");
            odata_err(e)
        })?;
        Ok(page)
    }

    /// Get one message by id within a chat (not deleted).
    pub async fn get(
        &self,
        runner: &impl DBRunner,
        scope: &AccessScope,
        tenant_id: Uuid,
        chat_id: Uuid,
        id: Uuid,
    ) -> Result<Option<messages::Model>, DomainError> {
        let row = Self::base_select(scope, tenant_id, chat_id)
            .filter(Condition::all().add(messages::Column::Id.eq(id)))
            .filter(Condition::all().add(messages::Column::DeletedAt.is_null()))
            .one(runner)
            .await?;
        Ok(row)
    }

    /// Count non-deleted messages of a chat.
    pub async fn count(
        &self,
        runner: &impl DBRunner,
        scope: &AccessScope,
        tenant_id: Uuid,
        chat_id: Uuid,
    ) -> Result<i64, DomainError> {
        let n = Self::base_select(scope, tenant_id, chat_id)
            .filter(Condition::all().add(messages::Column::DeletedAt.is_null()))
            .count(runner)
            .await?;
        Ok(i64::try_from(n).unwrap_or(i64::MAX))
    }

    /// Latest non-deleted assistant message with non-zero token counts
    /// (prior-context proxy for preflight estimation).
    pub async fn latest_assistant_usage(
        &self,
        runner: &impl DBRunner,
        scope: &AccessScope,
        tenant_id: Uuid,
        chat_id: Uuid,
    ) -> Result<Option<i64>, DomainError> {
        let row = Self::base_select(scope, tenant_id, chat_id)
            .filter(Condition::all().add(messages::Column::DeletedAt.is_null()))
            .filter(Condition::all().add(messages::Column::Role.eq("assistant")))
            .filter(
                Condition::any()
                    .add(messages::Column::InputTokens.ne(0))
                    .add(messages::Column::OutputTokens.ne(0)),
            )
            .order_by(messages::Column::CreatedAt, sea_orm::Order::Desc)
            .order_by(messages::Column::Id, sea_orm::Order::Desc)
            .limit(1)
            .one(runner)
            .await?;
        Ok(row.map(|m| m.input_tokens + m.output_tokens))
    }

    /// Latest message key (`(created_at, id)`) of non-deleted messages with a
    /// request id — the snapshot boundary for context assembly.
    pub async fn latest_boundary(
        &self,
        runner: &impl DBRunner,
        scope: &AccessScope,
        tenant_id: Uuid,
        chat_id: Uuid,
    ) -> Result<Option<MessageKey>, DomainError> {
        let row = Self::base_select(scope, tenant_id, chat_id)
            .filter(Condition::all().add(messages::Column::DeletedAt.is_null()))
            .filter(Condition::all().add(messages::Column::RequestId.is_not_null()))
            .order_by(messages::Column::CreatedAt, sea_orm::Order::Desc)
            .order_by(messages::Column::Id, sea_orm::Order::Desc)
            .limit(1)
            .one(runner)
            .await?;
        Ok(row.map(|m| (m.created_at, m.id)))
    }

    /// Recent messages for context assembly: non-deleted, non-compressed,
    /// at or below the snapshot boundary, above the summary frontier (when
    /// given), newest first, limited.
    pub async fn recent_for_context(
        &self,
        runner: &impl DBRunner,
        scope: &AccessScope,
        tenant_id: Uuid,
        chat_id: Uuid,
        boundary: MessageKey,
        frontier: Option<MessageKey>,
        limit: u32,
    ) -> Result<Vec<messages::Model>, DomainError> {
        let mut cond = Condition::all()
            .add(messages::Column::DeletedAt.is_null())
            .add(messages::Column::RequestId.is_not_null())
            .add(messages::Column::IsCompressed.eq(false));
        cond = cond.add(le_key(messages::Column::CreatedAt, messages::Column::Id, boundary));
        if let Some(frontier) = frontier {
            cond = cond.add(gt_key(messages::Column::CreatedAt, messages::Column::Id, frontier));
        }
        let rows = Self::base_select(scope, tenant_id, chat_id)
            .filter(cond)
            .order_by(messages::Column::CreatedAt, sea_orm::Order::Desc)
            .order_by(messages::Column::Id, sea_orm::Order::Desc)
            .limit(u64::from(limit))
            .all(runner)
            .await?;
        Ok(rows)
    }

    /// Messages of a chat in the range `(base, target]` (strict total order),
    /// non-deleted and non-compressed, chronological.
    pub async fn summary_range(
        &self,
        runner: &impl DBRunner,
        tenant_id: Uuid,
        chat_id: Uuid,
        base: Option<MessageKey>,
        target: MessageKey,
    ) -> Result<Vec<messages::Model>, DomainError> {
        let mut cond = Condition::all()
            .add(messages::Column::DeletedAt.is_null())
            .add(messages::Column::IsCompressed.eq(false));
        cond = cond.add(le_key(messages::Column::CreatedAt, messages::Column::Id, target));
        if let Some(base) = base {
            cond = cond.add(gt_key(messages::Column::CreatedAt, messages::Column::Id, base));
        }
        let rows = messages::Entity::find()
            .secure()
            .scope_with(&AccessScope::allow_all())
            .filter(Condition::all().add(messages::Column::TenantId.eq(tenant_id)))
            .filter(Condition::all().add(messages::Column::ChatId.eq(chat_id)))
            .filter(cond)
            .order_by(messages::Column::CreatedAt, sea_orm::Order::Asc)
            .order_by(messages::Column::Id, sea_orm::Order::Asc)
            .all(runner)
            .await?;
        Ok(rows)
    }

    /// Mark messages `is_compressed` in the range `(base, target]` (only rows
    /// still non-compressed and non-deleted).
    pub async fn mark_compressed(
        &self,
        runner: &impl DBRunner,
        tenant_id: Uuid,
        chat_id: Uuid,
        base: Option<MessageKey>,
        target: MessageKey,
    ) -> Result<u64, DomainError> {
        let mut cond = Condition::all()
            .add(messages::Column::DeletedAt.is_null())
            .add(messages::Column::IsCompressed.eq(false));
        cond = cond.add(le_key(messages::Column::CreatedAt, messages::Column::Id, target));
        if let Some(base) = base {
            cond = cond.add(gt_key(messages::Column::CreatedAt, messages::Column::Id, base));
        }
        let res = messages::Entity::update_many()
            .col_expr(messages::Column::IsCompressed, Expr::value(true))
            .filter(Condition::all().add(messages::Column::TenantId.eq(tenant_id)))
            .filter(Condition::all().add(messages::Column::ChatId.eq(chat_id)))
            .filter(cond)
            .secure()
            .scope_with(&AccessScope::allow_all())
            .exec(runner)
            .await?;
        Ok(res.rows_affected)
    }

    /// Clear `is_compressed` on all messages of a chat.
    pub async fn clear_compressed(
        &self,
        runner: &impl DBRunner,
        tenant_id: Uuid,
        chat_id: Uuid,
    ) -> Result<(), DomainError> {
        messages::Entity::update_many()
            .col_expr(messages::Column::IsCompressed, Expr::value(false))
            .filter(Condition::all().add(messages::Column::TenantId.eq(tenant_id)))
            .filter(Condition::all().add(messages::Column::ChatId.eq(chat_id)))
            .filter(Condition::all().add(messages::Column::IsCompressed.eq(true)))
            .secure()
            .scope_with(&AccessScope::allow_all())
            .exec(runner)
            .await?;
        Ok(())
    }

    /// The user message of a turn (by request id, not deleted).
    pub async fn get_turn_user_message(
        &self,
        runner: &impl DBRunner,
        tenant_id: Uuid,
        chat_id: Uuid,
        request_id: Uuid,
    ) -> Result<Option<messages::Model>, DomainError> {
        let row = messages::Entity::find()
            .secure()
            .scope_with(&AccessScope::allow_all())
            .filter(Condition::all().add(messages::Column::TenantId.eq(tenant_id)))
            .filter(Condition::all().add(messages::Column::ChatId.eq(chat_id)))
            .filter(Condition::all().add(messages::Column::RequestId.eq(request_id)))
            .filter(Condition::all().add(messages::Column::Role.eq("user")))
            .filter(Condition::all().add(messages::Column::DeletedAt.is_null()))
            .one(runner)
            .await?;
        Ok(row)
    }

    /// Latest non-deleted message not belonging to a turn (thread summary
    /// frozen target frontier).
    pub async fn latest_excluding_turn(
        &self,
        runner: &impl DBRunner,
        tenant_id: Uuid,
        chat_id: Uuid,
        request_id: Uuid,
    ) -> Result<Option<MessageKey>, DomainError> {
        let row = messages::Entity::find()
            .secure()
            .scope_with(&AccessScope::allow_all())
            .filter(Condition::all().add(messages::Column::TenantId.eq(tenant_id)))
            .filter(Condition::all().add(messages::Column::ChatId.eq(chat_id)))
            .filter(Condition::all().add(messages::Column::DeletedAt.is_null()))
            .filter(
                Condition::any()
                    .add(messages::Column::RequestId.is_null())
                    .add(messages::Column::RequestId.ne(request_id)),
            )
            .order_by(messages::Column::CreatedAt, sea_orm::Order::Desc)
            .order_by(messages::Column::Id, sea_orm::Order::Desc)
            .limit(1)
            .one(runner)
            .await?;
        Ok(row.map(|m| (m.created_at, m.id)))
    }

    /// Soft-delete all non-deleted messages of a turn (by request id).
    pub async fn soft_delete_turn(
        &self,
        runner: &impl DBRunner,
        tenant_id: Uuid,
        chat_id: Uuid,
        request_id: Uuid,
    ) -> Result<(), DomainError> {
        let now = now();
        messages::Entity::update_many()
            .col_expr(messages::Column::DeletedAt, Expr::value(Some(now)))
            .filter(Condition::all().add(messages::Column::TenantId.eq(tenant_id)))
            .filter(Condition::all().add(messages::Column::ChatId.eq(chat_id)))
            .filter(Condition::all().add(messages::Column::RequestId.eq(request_id)))
            .filter(Condition::all().add(messages::Column::DeletedAt.is_null()))
            .secure()
            .scope_with(&AccessScope::allow_all())
            .exec(runner)
            .await?;
        Ok(())
    }
}

/// `(created_at, id) <= key` row-value comparison.
fn le_key(
    created: messages::Column,
    id: messages::Column,
    key: MessageKey,
) -> sea_orm::Condition {
    Condition::any()
        .add(created.lt(key.0))
        .add(Condition::all().add(created.eq(key.0)).add(id.lte(key.1)))
}

/// `(created_at, id) > key` row-value comparison.
fn gt_key(
    created: messages::Column,
    id: messages::Column,
    key: MessageKey,
) -> sea_orm::Condition {
    Condition::any()
        .add(created.gt(key.0))
        .add(Condition::all().add(created.eq(key.0)).add(id.gt(key.1)))
}
