//! `thread_summaries` (one committed summary per chat): the read of the send pipeline, the
//! delete of a turn mutation and the compare-and-set commit of the summary handler.

use sea_orm::sea_query::Expr;
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};
use time::OffsetDateTime;
use toolkit_db::secure::{
    AccessScope, DBRunner, SecureDeleteExt as _, SecureEntityExt, SecureUpdateExt as _,
    secure_insert,
};
use uuid::Uuid;

use super::messages::Position;
use crate::domain::error::{DomainError, map_scope_err};
use crate::infra::db::entity::thread_summaries::{self, Column};
use crate::infra::db::ts;

/// A summary to commit.
#[derive(Debug, Clone)]
pub struct NewSummary {
    pub tenant_id: Uuid,
    pub chat_id: Uuid,
    /// The new inclusive frontier.
    pub frontier: Position,
    pub token_estimate: i32,
    pub text: String,
    pub now: OffsetDateTime,
}

/// Compare-and-set commit of `summary`: inserts the chat's first row when `base` is `None`,
/// otherwise advances the row whose frontier is still `base`. `false` when the precondition
/// does not hold (a row exists / the frontier moved or the row is gone); a lost insert race on
/// the unique `chat_id` is a `Conflict` error.
///
/// # Errors
/// `Conflict` on a unique violation, `Internal` on a database error.
pub async fn compare_and_set(
    tx: &impl DBRunner,
    scope: &AccessScope,
    base: Option<Position>,
    summary: NewSummary,
) -> Result<bool, DomainError> {
    let now = ts::normalize(summary.now);
    let frontier_at = ts::normalize(summary.frontier.created_at);
    let Some(base) = base else {
        if find_for_chat(tx, scope, summary.chat_id).await?.is_some() {
            return Ok(false);
        }
        let row = thread_summaries::ActiveModel {
            id: sea_orm::Set(Uuid::new_v4()),
            tenant_id: sea_orm::Set(summary.tenant_id),
            chat_id: sea_orm::Set(summary.chat_id),
            summary_text: sea_orm::Set(summary.text),
            summarized_up_to_created_at: sea_orm::Set(frontier_at),
            summarized_up_to_message_id: sea_orm::Set(summary.frontier.id),
            token_estimate: sea_orm::Set(summary.token_estimate),
            created_at: sea_orm::Set(now),
            updated_at: sea_orm::Set(now),
        };
        secure_insert::<thread_summaries::Entity>(row, scope, tx)
            .await
            .map_err(map_scope_err)?;
        return Ok(true);
    };
    let res = thread_summaries::Entity::update_many()
        .col_expr(Column::SummaryText, Expr::value(summary.text))
        .col_expr(Column::SummarizedUpToCreatedAt, Expr::value(frontier_at))
        .col_expr(
            Column::SummarizedUpToMessageId,
            Expr::value(summary.frontier.id),
        )
        .col_expr(Column::TokenEstimate, Expr::value(summary.token_estimate))
        .col_expr(Column::UpdatedAt, Expr::value(now))
        .filter(Column::ChatId.eq(summary.chat_id))
        .filter(Column::SummarizedUpToCreatedAt.eq(ts::normalize(base.created_at)))
        .filter(Column::SummarizedUpToMessageId.eq(base.id))
        .secure()
        .scope_with(scope)
        .exec(tx)
        .await
        .map_err(map_scope_err)?;
    Ok(res.rows_affected == 1)
}

/// The chat's summary row, if any.
///
/// # Errors
/// `Internal` on a database error.
pub async fn find_for_chat(
    conn: &impl DBRunner,
    scope: &AccessScope,
    chat_id: Uuid,
) -> Result<Option<thread_summaries::Model>, DomainError> {
    thread_summaries::Entity::find()
        .filter(Column::ChatId.eq(chat_id))
        .secure()
        .scope_with(scope)
        .one(conn)
        .await
        .map_err(map_scope_err)
}

/// Deletes the chat's summary row (a turn mutation invalidated it).
///
/// # Errors
/// `Internal` on a database error.
pub async fn delete_for_chat(
    tx: &impl DBRunner,
    scope: &AccessScope,
    chat_id: Uuid,
) -> Result<(), DomainError> {
    thread_summaries::Entity::delete_many()
        .filter(Column::ChatId.eq(chat_id))
        .secure()
        .scope_with(scope)
        .exec(tx)
        .await
        .map_err(map_scope_err)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use time::Duration;

    use super::*;
    use crate::infra::db::ts::db_now;
    use crate::infra::db::tx::write_tx;
    use crate::test_support::app::{TestApp, ctx};
    use crate::test_support::stream::{create_chat, thread_summary_of};

    async fn cas(
        app: &TestApp,
        tenant: Uuid,
        base: Option<Position>,
        summary: NewSummary,
    ) -> Result<bool, DomainError> {
        write_tx(&app.services.db, move |tx| {
            let summary = summary.clone();
            Box::pin(async move {
                compare_and_set(tx, &AccessScope::for_tenant(tenant), base, summary).await
            })
        })
        .await
    }

    #[tokio::test]
    async fn compare_and_set_requires_the_stored_frontier() {
        let app = TestApp::builder().build().await;
        let (tenant, user) = (Uuid::new_v4(), Uuid::new_v4());
        let chat = create_chat(&app, &ctx(tenant, user), None).await;
        let now = db_now();
        let first = Position {
            created_at: now - Duration::seconds(10),
            id: Uuid::new_v4(),
        };
        let second = Position {
            created_at: now - Duration::seconds(5),
            id: Uuid::new_v4(),
        };
        let summary = |text: &str, frontier: Position| NewSummary {
            tenant_id: tenant,
            chat_id: chat,
            frontier,
            token_estimate: 7,
            text: text.to_owned(),
            now,
        };

        // No row yet: the first summary is inserted.
        assert!(
            cas(&app, tenant, None, summary("one", first))
                .await
                .unwrap()
        );
        // A second "first summary" finds the row: not committed.
        assert!(
            !cas(&app, tenant, None, summary("dup", second))
                .await
                .unwrap()
        );
        // An UPDATE based on a frontier that is not the stored one: not committed.
        assert!(
            !cas(&app, tenant, Some(second), summary("stale", second))
                .await
                .unwrap()
        );
        let row = thread_summary_of(&app, chat).await.unwrap();
        assert_eq!(
            (row.summary_text.as_str(), row.summarized_up_to_message_id),
            ("one", first.id)
        );

        // Based on the stored frontier: advanced.
        assert!(
            cas(&app, tenant, Some(first), summary("two", second))
                .await
                .unwrap()
        );
        let row = thread_summary_of(&app, chat).await.unwrap();
        assert_eq!(
            (row.summary_text.as_str(), row.summarized_up_to_message_id),
            ("two", second.id)
        );
        assert_eq!(row.summarized_up_to_created_at, second.created_at);
        // The old base no longer matches.
        assert!(
            !cas(&app, tenant, Some(first), summary("three", second))
                .await
                .unwrap()
        );
    }
}
