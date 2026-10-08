//! `chat` repository (owner-scoped).

use std::marker::PhantomData;

use sea_orm::sea_query::Expr;
use sea_orm::{ColumnTrait, Condition, DbBackend, EntityTrait, QueryFilter};
use time::OffsetDateTime;
use toolkit_db::odata::{FieldToColumn, LimitCfg, ODataFieldMapping, paginate_odata};
use toolkit_db::secure::{AccessScope, DBRunner, ScopeError, SecureEntityExt, SecureUpdateExt};
use toolkit_odata::filter::{FieldKind, FilterField, FilterOp, ODataValue};
use toolkit_odata::{ODataQuery, Page, SortDir};
use uuid::Uuid;

use super::insert_model;
use crate::api::rest::dto::ChatDetailDtoFilterField;
use crate::infra::db::entity::chat;
use crate::infra::db::timestamps::{self, Dialect, PgDialect, SqliteDialect};

/// Page size of the chat list: default 20, larger requests clamped to 100.
pub const CHAT_LIST_LIMITS: LimitCfg = LimitCfg {
    default: 20,
    max: 100,
};

/// Repository for `chat` rows.
#[derive(Debug, Clone, Copy, Default)]
pub struct ChatRepo;

/// `OData` mapping of the chat list fields (`updated_at`, `id`, `title`) for
/// dialect `D` (timestamp binding, see [`crate::infra::db::timestamps`]).
pub struct ChatODataMapper<D>(PhantomData<D>);

impl<D: Dialect> FieldToColumn<ChatDetailDtoFilterField> for ChatODataMapper<D> {
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
        _op: FilterOp,
        value: &ODataValue,
    ) -> Result<ODataValue, String> {
        match field {
            ChatDetailDtoFilterField::UpdatedAt => Ok(timestamps::filter_value::<D>(value)),
            _ => Ok(value.clone()),
        }
    }
}

impl<D: Dialect> ODataFieldMapping<ChatDetailDtoFilterField> for ChatODataMapper<D> {
    type Entity = chat::Entity;

    fn extract_cursor_value(
        model: &chat::Model,
        field: ChatDetailDtoFilterField,
    ) -> sea_orm::Value {
        match field {
            ChatDetailDtoFilterField::Id => sea_orm::Value::Uuid(Some(model.id)),
            ChatDetailDtoFilterField::Title => sea_orm::Value::String(model.title.clone()),
            ChatDetailDtoFilterField::UpdatedAt => timestamps::cursor_value::<D>(model.updated_at),
        }
    }

    fn cursor_kind(field: ChatDetailDtoFilterField) -> FieldKind {
        match field {
            ChatDetailDtoFilterField::UpdatedAt => timestamps::cursor_kind::<D>(),
            other => other.kind(),
        }
    }
}

fn active(id: Uuid) -> Condition {
    Condition::all()
        .add(chat::Column::Id.eq(id))
        .add(chat::Column::DeletedAt.is_null())
}

impl ChatRepo {
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
        row: chat::Model,
    ) -> Result<chat::Model, ScopeError> {
        insert_model::<chat::Entity>(runner, scope, row).await
    }

    /// Load a row by id within the scope (soft-deleted rows included).
    ///
    /// # Errors
    ///
    /// `ScopeError` on a database error.
    pub async fn find_by_id(
        &self,
        runner: &impl DBRunner,
        scope: &AccessScope,
        id: Uuid,
    ) -> Result<Option<chat::Model>, ScopeError> {
        chat::Entity::find_by_id(id)
            .secure()
            .scope_with(scope)
            .one(runner)
            .await
    }

    /// Load a non-deleted row by id within the scope.
    ///
    /// # Errors
    ///
    /// `ScopeError` on a database error.
    pub async fn find_active(
        &self,
        runner: &impl DBRunner,
        scope: &AccessScope,
        id: Uuid,
    ) -> Result<Option<chat::Model>, ScopeError> {
        chat::Entity::find()
            .filter(active(id))
            .secure()
            .scope_with(scope)
            .one(runner)
            .await
    }

    /// Set the title of a non-deleted chat and bump `updated_at`; returns
    /// the updated row (`None` when no such chat is in scope).
    ///
    /// # Errors
    ///
    /// `ScopeError` on a database error.
    pub async fn rename(
        &self,
        runner: &impl DBRunner,
        scope: &AccessScope,
        id: Uuid,
        title: &str,
        now: OffsetDateTime,
    ) -> Result<Option<chat::Model>, ScopeError> {
        let rows = chat::Entity::update_many()
            .filter(active(id))
            .secure()
            .scope_with(scope)
            .col_expr(chat::Column::Title, Expr::value(title))
            .col_expr(chat::Column::UpdatedAt, Expr::value(now))
            .exec_with_returning(runner)
            .await?;
        Ok(rows.into_iter().next())
    }

    /// Bump `updated_at` of a non-deleted chat (send / retry / edit).
    /// Returns the number of rows updated.
    ///
    /// # Errors
    ///
    /// `ScopeError` on a database error.
    pub async fn touch(
        &self,
        runner: &impl DBRunner,
        scope: &AccessScope,
        id: Uuid,
        now: OffsetDateTime,
    ) -> Result<u64, ScopeError> {
        let res = chat::Entity::update_many()
            .filter(active(id))
            .secure()
            .scope_with(scope)
            .col_expr(chat::Column::UpdatedAt, Expr::value(now))
            .exec(runner)
            .await?;
        Ok(res.rows_affected)
    }

    /// Soft-delete a non-deleted chat (`deleted_at = updated_at = now`);
    /// returns the deleted row (`None` when no such chat is in scope).
    ///
    /// # Errors
    ///
    /// `ScopeError` on a database error.
    pub async fn soft_delete(
        &self,
        runner: &impl DBRunner,
        scope: &AccessScope,
        id: Uuid,
        now: OffsetDateTime,
    ) -> Result<Option<chat::Model>, ScopeError> {
        let rows = chat::Entity::update_many()
            .filter(active(id))
            .secure()
            .scope_with(scope)
            .col_expr(chat::Column::DeletedAt, Expr::value(now))
            .col_expr(chat::Column::UpdatedAt, Expr::value(now))
            .exec_with_returning(runner)
            .await?;
        Ok(rows.into_iter().next())
    }

    /// One page of the non-deleted chats in scope (`backend` selects the
    /// timestamp binding dialect). Without `$orderby` (and
    /// without a cursor, which carries its own order) the order is
    /// `updated_at desc`; `id desc` is always the tiebreaker.
    ///
    /// # Errors
    ///
    /// `toolkit_odata::Error` for invalid filters / order fields / cursors
    /// and database failures (`Db`).
    pub async fn list_page(
        &self,
        runner: &impl DBRunner,
        scope: &AccessScope,
        query: &ODataQuery,
        backend: DbBackend,
    ) -> Result<Page<chat::Model>, toolkit_odata::Error> {
        let mut query = query.clone();
        if query.cursor.is_none() && query.order.0.is_empty() {
            query.order = toolkit_odata::ODataOrderBy(vec![toolkit_odata::OrderKey {
                field: "updated_at".to_owned(),
                dir: SortDir::Desc,
            }]);
        }
        let select = chat::Entity::find()
            .filter(chat::Column::DeletedAt.is_null())
            .secure()
            .scope_with(scope);
        let tiebreaker = ("id", SortDir::Desc);
        if timestamps::is_sqlite(backend) {
            paginate_odata::<_, ChatODataMapper<SqliteDialect>, _, _, _, _>(
                select,
                runner,
                &query,
                tiebreaker,
                CHAT_LIST_LIMITS,
                |m| m,
            )
            .await
        } else {
            paginate_odata::<_, ChatODataMapper<PgDialect>, _, _, _, _>(
                select,
                runner,
                &query,
                tiebreaker,
                CHAT_LIST_LIMITS,
                |m| m,
            )
            .await
        }
    }
}
