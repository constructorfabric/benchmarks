//! `chats` repository. Every query is owner-scoped by the compiled PEP scope.

use sea_orm::sea_query::Expr;
use sea_orm::{ActiveValue::Set, ColumnTrait, Condition, EntityTrait, QueryFilter};
use time::OffsetDateTime;
use toolkit_db::secure::{DBRunner, SecureEntityExt, SecureInsertExt, SecureUpdateExt};
use toolkit_security::AccessScope;
use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::infra::db::entities::{chat, message};

pub async fn insert(
    runner: &impl DBRunner,
    scope: &AccessScope,
    model: chat::Model,
) -> Result<chat::Model, DomainError> {
    let am = chat::ActiveModel {
        id: Set(model.id),
        tenant_id: Set(model.tenant_id),
        user_id: Set(model.user_id),
        model: Set(model.model.clone()),
        title: Set(model.title.clone()),
        is_temporary: Set(model.is_temporary),
        created_at: Set(model.created_at),
        updated_at: Set(model.updated_at),
        deleted_at: Set(None),
    };
    chat::Entity::insert(am.clone())
        .secure()
        .scope_with_model(scope, &am)?
        .exec(runner)
        .await?;
    Ok(model)
}

/// Non-deleted chat by id within the scope.
pub async fn find(
    runner: &impl DBRunner,
    scope: &AccessScope,
    id: Uuid,
) -> Result<Option<chat::Model>, DomainError> {
    Ok(chat::Entity::find()
        .filter(
            Condition::all()
                .add(chat::Column::Id.eq(id))
                .add(chat::Column::DeletedAt.is_null()),
        )
        .secure()
        .scope_with(scope)
        .one(runner)
        .await?)
}

/// Chat by id including soft-deleted rows (system workers).
pub async fn find_any(
    runner: &impl DBRunner,
    scope: &AccessScope,
    id: Uuid,
) -> Result<Option<chat::Model>, DomainError> {
    Ok(chat::Entity::find()
        .filter(Condition::all().add(chat::Column::Id.eq(id)))
        .secure()
        .scope_with(scope)
        .one(runner)
        .await?)
}

pub async fn update_title(
    runner: &impl DBRunner,
    scope: &AccessScope,
    id: Uuid,
    title: &str,
    now: OffsetDateTime,
) -> Result<bool, DomainError> {
    let res = chat::Entity::update_many()
        .secure()
        .col_expr(chat::Column::Title, Expr::value(title.to_owned()))
        .col_expr(chat::Column::UpdatedAt, Expr::value(now))
        .filter(
            Condition::all()
                .add(chat::Column::Id.eq(id))
                .add(chat::Column::DeletedAt.is_null()),
        )
        .scope_with(scope)
        .exec(runner)
        .await?;
    Ok(res.rows_affected == 1)
}

/// Soft-delete; `false` when the chat is missing or already deleted.
pub async fn soft_delete(
    runner: &impl DBRunner,
    scope: &AccessScope,
    id: Uuid,
    now: OffsetDateTime,
) -> Result<bool, DomainError> {
    let res = chat::Entity::update_many()
        .secure()
        .col_expr(chat::Column::DeletedAt, Expr::value(now))
        .col_expr(chat::Column::UpdatedAt, Expr::value(now))
        .filter(
            Condition::all()
                .add(chat::Column::Id.eq(id))
                .add(chat::Column::DeletedAt.is_null()),
        )
        .scope_with(scope)
        .exec(runner)
        .await?;
    Ok(res.rows_affected == 1)
}

/// Bump `updated_at` (sent message, retry, edit).
pub async fn touch(
    runner: &impl DBRunner,
    scope: &AccessScope,
    id: Uuid,
    now: OffsetDateTime,
) -> Result<(), DomainError> {
    chat::Entity::update_many()
        .secure()
        .col_expr(chat::Column::UpdatedAt, Expr::value(now))
        .filter(Condition::all().add(chat::Column::Id.eq(id)))
        .scope_with(scope)
        .exec(runner)
        .await?;
    Ok(())
}

/// Number of non-deleted messages of a chat.
pub async fn message_count(
    runner: &impl DBRunner,
    tenant_scope: &AccessScope,
    chat_id: Uuid,
) -> Result<i64, DomainError> {
    let n = message::Entity::find()
        .filter(
            Condition::all()
                .add(message::Column::ChatId.eq(chat_id))
                .add(message::Column::DeletedAt.is_null()),
        )
        .secure()
        .scope_with(tenant_scope)
        .count(runner)
        .await?;
    Ok(i64::try_from(n).unwrap_or(i64::MAX))
}

/// Message counts for several chats (list endpoint).
pub async fn message_counts(
    runner: &impl DBRunner,
    tenant_scope: &AccessScope,
    chat_ids: &[Uuid],
) -> Result<std::collections::HashMap<Uuid, i64>, DomainError> {
    #[derive(sea_orm::FromQueryResult)]
    struct Row {
        chat_id: Uuid,
        cnt: i64,
    }
    let mut out = std::collections::HashMap::new();
    if chat_ids.is_empty() {
        return Ok(out);
    }
    let rows: Vec<Row> = message::Entity::find()
        .filter(
            Condition::all()
                .add(message::Column::ChatId.is_in(chat_ids.iter().copied()))
                .add(message::Column::DeletedAt.is_null()),
        )
        .secure()
        .scope_with(tenant_scope)
        .project_all(runner, |q| {
            use sea_orm::QuerySelect;
            q.select_only()
                .column(message::Column::ChatId)
                .column_as(
                    Expr::from(sea_orm::sea_query::Func::count(Expr::col(
                        message::Column::Id,
                    ))),
                    "cnt",
                )
                .group_by(message::Column::ChatId)
                .into_model::<Row>()
        })
        .await?;
    for row in rows {
        out.insert(row.chat_id, row.cnt);
    }
    Ok(out)
}
