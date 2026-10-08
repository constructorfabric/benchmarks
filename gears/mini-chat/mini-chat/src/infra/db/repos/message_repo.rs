//! `message` repository (chat child: tenant-only scope).

use std::collections::HashMap;
use std::marker::PhantomData;

use sea_orm::sea_query::Expr;
use sea_orm::{
    ColumnTrait, Condition, DbBackend, EntityTrait, ExprTrait, FromQueryResult, Order, QueryFilter,
    QuerySelect,
};
use toolkit_db::odata::{FieldToColumn, LimitCfg, ODataFieldMapping, paginate_odata};
use toolkit_db::secure::{AccessScope, DBRunner, ScopeError, SecureEntityExt, SecureUpdateExt};
use toolkit_odata::filter::{FieldKind, FilterField, FilterOp, ODataValue};
use toolkit_odata::{ODataQuery, Page, SortDir};
use uuid::Uuid;

use super::insert_model;
use crate::api::rest::dto::MiniChatMessageDtoFilterField;
use crate::infra::db::entity::message;
use crate::infra::db::timestamps::{self, Dialect, PgDialect, SqliteDialect};

/// Page size of the message list: default 20, larger requests clamped to 100.
pub const MESSAGE_LIST_LIMITS: LimitCfg = LimitCfg {
    default: 20,
    max: 100,
};

/// `OData` mapping of the message list fields (`created_at`, `id`, `role`)
/// for dialect `D` (timestamp binding, see [`crate::infra::db::timestamps`]).
pub struct MessageODataMapper<D>(PhantomData<D>);

impl<D: Dialect> FieldToColumn<MiniChatMessageDtoFilterField> for MessageODataMapper<D> {
    type Column = message::Column;

    fn map_field(field: MiniChatMessageDtoFilterField) -> message::Column {
        match field {
            MiniChatMessageDtoFilterField::Id => message::Column::Id,
            MiniChatMessageDtoFilterField::Role => message::Column::Role,
            MiniChatMessageDtoFilterField::CreatedAt => message::Column::CreatedAt,
        }
    }

    fn map_value(
        field: MiniChatMessageDtoFilterField,
        _op: FilterOp,
        value: &ODataValue,
    ) -> Result<ODataValue, String> {
        match field {
            MiniChatMessageDtoFilterField::CreatedAt => Ok(timestamps::filter_value::<D>(value)),
            _ => Ok(value.clone()),
        }
    }
}

impl<D: Dialect> ODataFieldMapping<MiniChatMessageDtoFilterField> for MessageODataMapper<D> {
    type Entity = message::Entity;

    fn extract_cursor_value(
        model: &message::Model,
        field: MiniChatMessageDtoFilterField,
    ) -> sea_orm::Value {
        match field {
            MiniChatMessageDtoFilterField::Id => sea_orm::Value::Uuid(Some(model.id)),
            MiniChatMessageDtoFilterField::Role => sea_orm::Value::String(Some(model.role.clone())),
            MiniChatMessageDtoFilterField::CreatedAt => {
                timestamps::cursor_value::<D>(model.created_at)
            }
        }
    }

    fn cursor_kind(field: MiniChatMessageDtoFilterField) -> FieldKind {
        match field {
            MiniChatMessageDtoFilterField::CreatedAt => timestamps::cursor_kind::<D>(),
            other => other.kind(),
        }
    }
}

/// Message order key `(created_at, id)` (strict total order of a chat's
/// messages; also the thread-summary frontier).
pub type OrderKey = (time::OffsetDateTime, Uuid);

/// `messages.created_at` bound for comparisons (fixed-width text on
/// `SQLite`, see [`crate::infra::db::timestamps`]).
fn created_at_value(backend: DbBackend, t: time::OffsetDateTime) -> sea_orm::Value {
    if timestamps::is_sqlite(backend) {
        timestamps::cursor_value::<SqliteDialect>(t)
    } else {
        timestamps::cursor_value::<PgDialect>(t)
    }
}

/// `(created_at, id) > key`.
fn after_key(backend: DbBackend, (t, id): OrderKey) -> Condition {
    let v = created_at_value(backend, t);
    Condition::any()
        .add(message::Column::CreatedAt.gt(v.clone()))
        .add(
            Condition::all()
                .add(message::Column::CreatedAt.eq(v))
                .add(message::Column::Id.gt(id)),
        )
}

/// Repository for `message` rows.
#[derive(Debug, Clone, Copy, Default)]
pub struct MessageRepo;

impl MessageRepo {
    /// Insert a complete row.
    ///
    /// # Errors
    ///
    /// `ScopeError` on scope denial or a database error (unique/CHECK
    /// violations included).
    pub async fn insert(
        &self,
        runner: &impl DBRunner,
        scope: &AccessScope,
        row: message::Model,
    ) -> Result<message::Model, ScopeError> {
        insert_model::<message::Entity>(runner, &scope.tenant_only(), row).await
    }

    /// Load a row by id within the scope.
    ///
    /// # Errors
    ///
    /// `ScopeError` on a database error.
    pub async fn find_by_id(
        &self,
        runner: &impl DBRunner,
        scope: &AccessScope,
        id: Uuid,
    ) -> Result<Option<message::Model>, ScopeError> {
        message::Entity::find_by_id(id)
            .secure()
            .scope_with(&scope.tenant_only())
            .one(runner)
            .await
    }

    /// Number of non-deleted messages per chat, for the given chats (chats
    /// without messages are absent from the map).
    ///
    /// # Errors
    ///
    /// `ScopeError` on a database error.
    pub async fn count_active_by_chats(
        &self,
        runner: &impl DBRunner,
        scope: &AccessScope,
        chat_ids: &[Uuid],
    ) -> Result<HashMap<Uuid, i64>, ScopeError> {
        #[derive(FromQueryResult)]
        struct ChatCount {
            chat_id: Uuid,
            cnt: i64,
        }
        if chat_ids.is_empty() {
            return Ok(HashMap::new());
        }
        let rows: Vec<ChatCount> = message::Entity::find()
            .filter(message::Column::ChatId.is_in(chat_ids.iter().copied()))
            .filter(message::Column::DeletedAt.is_null())
            .secure()
            .scope_with(&scope.tenant_only())
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

    /// Recent-history window of the chat for context assembly: messages
    /// with a request id, not deleted, not compressed, after the summary
    /// frontier `after` (order key `(created_at, id)`) when there is one,
    /// newest first. `backend` selects the timestamp binding.
    ///
    /// # Errors
    ///
    /// `ScopeError` on a database error.
    pub async fn recent_for_context(
        &self,
        runner: &impl DBRunner,
        scope: &AccessScope,
        chat_id: Uuid,
        limit: u64,
        after: Option<OrderKey>,
        backend: DbBackend,
    ) -> Result<Vec<message::Model>, ScopeError> {
        let mut cond = Condition::all()
            .add(message::Column::ChatId.eq(chat_id))
            .add(message::Column::RequestId.is_not_null())
            .add(message::Column::DeletedAt.is_null())
            .add(message::Column::IsCompressed.eq(false));
        if let Some(key) = after {
            cond = cond.add(after_key(backend, key));
        }
        message::Entity::find()
            .secure()
            .scope_with(&scope.tenant_only())
            .filter(cond)
            .order_by(message::Column::CreatedAt, Order::Desc)
            .order_by(message::Column::Id, Order::Desc)
            .limit(limit)
            .all(runner)
            .await
    }

    /// The latest non-deleted message of the chat that does not belong to
    /// turn `request_id` (the frozen target of a thread summary).
    ///
    /// # Errors
    ///
    /// `ScopeError` on a database error.
    pub async fn latest_outside_turn(
        &self,
        runner: &impl DBRunner,
        scope: &AccessScope,
        chat_id: Uuid,
        request_id: Uuid,
    ) -> Result<Option<message::Model>, ScopeError> {
        message::Entity::find()
            .secure()
            .scope_with(&scope.tenant_only())
            .filter(
                Condition::all()
                    .add(message::Column::ChatId.eq(chat_id))
                    .add(message::Column::DeletedAt.is_null())
                    .add(
                        Condition::any()
                            .add(message::Column::RequestId.ne(request_id))
                            .add(message::Column::RequestId.is_null()),
                    ),
            )
            .order_by(message::Column::CreatedAt, Order::Desc)
            .order_by(message::Column::Id, Order::Desc)
            .limit(1)
            .one(runner)
            .await
    }

    /// The thread-summary range of the chat: non-deleted, non-compressed
    /// messages with order key in `(base, target]` (`base = None`: from the
    /// start), ordered by `(created_at, id)` ascending.
    ///
    /// # Errors
    ///
    /// `ScopeError` on a database error.
    pub async fn summary_range(
        &self,
        runner: &impl DBRunner,
        scope: &AccessScope,
        chat_id: Uuid,
        base: Option<OrderKey>,
        target: OrderKey,
        backend: DbBackend,
    ) -> Result<Vec<message::Model>, ScopeError> {
        let mut cond = Condition::all()
            .add(message::Column::ChatId.eq(chat_id))
            .add(message::Column::DeletedAt.is_null())
            .add(message::Column::IsCompressed.eq(false))
            .add(after_key(backend, target).not());
        if let Some(key) = base {
            cond = cond.add(after_key(backend, key));
        }
        message::Entity::find()
            .secure()
            .scope_with(&scope.tenant_only())
            .filter(cond)
            .order_by(message::Column::CreatedAt, Order::Asc)
            .order_by(message::Column::Id, Order::Asc)
            .all(runner)
            .await
    }

    /// Check that message `id` of the chat is not deleted and lock its row
    /// for the rest of the transaction (a no-op update; on `PostgreSQL` it
    /// takes the row lock). Returns 1 when the message is live, else 0.
    ///
    /// # Errors
    ///
    /// `ScopeError` on a database error.
    pub async fn lock_live(
        &self,
        runner: &impl DBRunner,
        scope: &AccessScope,
        chat_id: Uuid,
        id: Uuid,
    ) -> Result<u64, ScopeError> {
        let res = message::Entity::update_many()
            .filter(
                Condition::all()
                    .add(message::Column::ChatId.eq(chat_id))
                    .add(message::Column::Id.eq(id))
                    .add(message::Column::DeletedAt.is_null()),
            )
            .secure()
            .scope_with(&scope.tenant_only())
            .col_expr(
                message::Column::DeletedAt,
                Expr::col(message::Column::DeletedAt),
            )
            .exec(runner)
            .await?;
        Ok(res.rows_affected)
    }

    /// Set `is_compressed` on the non-deleted messages `ids` of the chat.
    /// Returns the rows updated.
    ///
    /// # Errors
    ///
    /// `ScopeError` on a database error.
    pub async fn mark_compressed(
        &self,
        runner: &impl DBRunner,
        scope: &AccessScope,
        chat_id: Uuid,
        ids: &[Uuid],
    ) -> Result<u64, ScopeError> {
        if ids.is_empty() {
            return Ok(0);
        }
        let res = message::Entity::update_many()
            .filter(
                Condition::all()
                    .add(message::Column::ChatId.eq(chat_id))
                    .add(message::Column::Id.is_in(ids.iter().copied()))
                    .add(message::Column::DeletedAt.is_null()),
            )
            .secure()
            .scope_with(&scope.tenant_only())
            .col_expr(message::Column::IsCompressed, Expr::value(true))
            .exec(runner)
            .await?;
        Ok(res.rows_affected)
    }

    /// `(input_tokens, output_tokens)` of the latest non-deleted assistant
    /// message with non-zero token counts (prior context of the reserve),
    /// ignoring the messages of turn `exclude_request` (the turn a retry /
    /// edit replaces).
    ///
    /// # Errors
    ///
    /// `ScopeError` on a database error.
    pub async fn latest_assistant_usage(
        &self,
        runner: &impl DBRunner,
        scope: &AccessScope,
        chat_id: Uuid,
        exclude_request: Option<Uuid>,
    ) -> Result<Option<(i64, i64)>, ScopeError> {
        let mut cond = Condition::all().add(message::Column::ChatId.eq(chat_id));
        if let Some(rid) = exclude_request {
            cond = cond.add(
                Condition::any()
                    .add(message::Column::RequestId.ne(rid))
                    .add(message::Column::RequestId.is_null()),
            );
        }
        let row = message::Entity::find()
            .secure()
            .scope_with(&scope.tenant_only())
            .filter(
                cond.add(message::Column::Role.eq("assistant"))
                    .add(message::Column::DeletedAt.is_null())
                    .add(
                        Condition::any()
                            .add(message::Column::InputTokens.gt(0))
                            .add(message::Column::OutputTokens.gt(0)),
                    ),
            )
            .order_by(message::Column::CreatedAt, Order::Desc)
            .order_by(message::Column::Id, Order::Desc)
            .limit(1)
            .one(runner)
            .await?;
        Ok(row.map(|m| (m.input_tokens, m.output_tokens)))
    }

    /// The non-deleted message of turn `request_id` with `role`.
    ///
    /// # Errors
    ///
    /// `ScopeError` on a database error.
    pub async fn find_turn_message(
        &self,
        runner: &impl DBRunner,
        scope: &AccessScope,
        chat_id: Uuid,
        request_id: Uuid,
        role: &str,
    ) -> Result<Option<message::Model>, ScopeError> {
        message::Entity::find()
            .secure()
            .scope_with(&scope.tenant_only())
            .filter(
                Condition::all()
                    .add(message::Column::ChatId.eq(chat_id))
                    .add(message::Column::RequestId.eq(request_id))
                    .add(message::Column::Role.eq(role))
                    .add(message::Column::DeletedAt.is_null()),
            )
            .one(runner)
            .await
    }

    /// Soft-delete the non-deleted messages of turn `request_id`. Returns
    /// the rows updated.
    ///
    /// # Errors
    ///
    /// `ScopeError` on a database error.
    pub async fn soft_delete_turn(
        &self,
        runner: &impl DBRunner,
        scope: &AccessScope,
        chat_id: Uuid,
        request_id: Uuid,
        now: time::OffsetDateTime,
    ) -> Result<u64, ScopeError> {
        let res = message::Entity::update_many()
            .filter(
                Condition::all()
                    .add(message::Column::ChatId.eq(chat_id))
                    .add(message::Column::RequestId.eq(request_id))
                    .add(message::Column::DeletedAt.is_null()),
            )
            .secure()
            .scope_with(&scope.tenant_only())
            .col_expr(message::Column::DeletedAt, Expr::value(now))
            .exec(runner)
            .await?;
        Ok(res.rows_affected)
    }

    /// Clear `is_compressed` on every message of the chat (the summary was
    /// dropped). Returns the rows updated.
    ///
    /// # Errors
    ///
    /// `ScopeError` on a database error.
    pub async fn clear_compressed(
        &self,
        runner: &impl DBRunner,
        scope: &AccessScope,
        chat_id: Uuid,
    ) -> Result<u64, ScopeError> {
        let res = message::Entity::update_many()
            .filter(
                Condition::all()
                    .add(message::Column::ChatId.eq(chat_id))
                    .add(message::Column::IsCompressed.eq(true)),
            )
            .secure()
            .scope_with(&scope.tenant_only())
            .col_expr(message::Column::IsCompressed, Expr::value(false))
            .exec(runner)
            .await?;
        Ok(res.rows_affected)
    }

    /// One page of the chat's non-deleted messages (`backend` selects the
    /// timestamp binding dialect). Without `$orderby` (and without a cursor,
    /// which carries its own order) the order is `created_at asc`; `id asc`
    /// is always the tiebreaker.
    ///
    /// # Errors
    ///
    /// `toolkit_odata::Error` for invalid filters / order fields / cursors
    /// and database failures (`Db`).
    pub async fn list_page(
        &self,
        runner: &impl DBRunner,
        scope: &AccessScope,
        chat_id: Uuid,
        query: &ODataQuery,
        backend: DbBackend,
    ) -> Result<Page<message::Model>, toolkit_odata::Error> {
        let mut query = query.clone();
        if query.cursor.is_none() && query.order.0.is_empty() {
            query.order = toolkit_odata::ODataOrderBy(vec![toolkit_odata::OrderKey {
                field: "created_at".to_owned(),
                dir: SortDir::Asc,
            }]);
        }
        let select = message::Entity::find()
            .filter(message::Column::ChatId.eq(chat_id))
            .filter(message::Column::DeletedAt.is_null())
            .secure()
            .scope_with(&scope.tenant_only());
        let tiebreaker = ("id", SortDir::Asc);
        if timestamps::is_sqlite(backend) {
            paginate_odata::<_, MessageODataMapper<SqliteDialect>, _, _, _, _>(
                select,
                runner,
                &query,
                tiebreaker,
                MESSAGE_LIST_LIMITS,
                |m| m,
            )
            .await
        } else {
            paginate_odata::<_, MessageODataMapper<PgDialect>, _, _, _, _>(
                select,
                runner,
                &query,
                tiebreaker,
                MESSAGE_LIST_LIMITS,
                |m| m,
            )
            .await
        }
    }
}
