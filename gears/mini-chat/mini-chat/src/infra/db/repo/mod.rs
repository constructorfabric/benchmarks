//! Repositories over the Secure ORM. Owner-scoped tables are queried with the
//! PEP scope; child tables of a chat with a tenant scope plus the `chat_id`
//! of an already authorized chat. Background workers use `allow_all`.

pub mod attachments;
pub mod chats;
pub mod messages;
pub mod quota;
pub mod reactions;
pub mod summaries;
pub mod turns;
pub mod vector_stores;

use chrono::{DateTime, Utc};
use sea_orm::{ColumnTrait, Condition};
use uuid::Uuid;

/// `(created_at, id) > (ts, id)` for a pair of columns.
pub(crate) fn tuple_gt<C: ColumnTrait>(
    ts_col: C,
    id_col: C,
    ts: DateTime<Utc>,
    id: Uuid,
) -> Condition {
    Condition::any()
        .add(ts_col.gt(ts))
        .add(Condition::all().add(ts_col.eq(ts)).add(id_col.gt(id)))
}

/// `(created_at, id) <= (ts, id)` for a pair of columns.
pub(crate) fn tuple_lte<C: ColumnTrait>(
    ts_col: C,
    id_col: C,
    ts: DateTime<Utc>,
    id: Uuid,
) -> Condition {
    Condition::any()
        .add(ts_col.lt(ts))
        .add(Condition::all().add(ts_col.eq(ts)).add(id_col.lte(id)))
}

/// Takes the SQLite write lock at the start of a transaction (a no-op
/// `UPDATE`), so a concurrent writer makes the busy handler wait instead of
/// failing a later read→write upgrade with `SQLITE_BUSY`.
///
/// # Errors
/// Database errors.
pub async fn acquire_write_lock(runner: &impl toolkit_db::secure::DBRunner) -> Result<(), crate::domain::error::DomainError> {
    use sea_orm::sea_query::Expr;
    use sea_orm::{EntityTrait, QueryFilter};
    use toolkit_db::secure::{AccessScope, SecureUpdateExt};
    use crate::infra::db::entities::chats;
    chats::Entity::update_many()
        .col_expr(chats::Column::Id, Expr::col(chats::Column::Id))
        .filter(sea_orm::sea_query::ExprTrait::eq(Expr::val(1), 0))
        .secure()
        .scope_with(&AccessScope::allow_all())
        .exec(runner)
        .await?;
    Ok(())
}
