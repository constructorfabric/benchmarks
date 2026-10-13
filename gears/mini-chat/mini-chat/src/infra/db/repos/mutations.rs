//! Turn mutation persistence: the Secure ORM implementation of the
//! [`MutationRepository`] port
//! (`cpt-cf-mini-chat-feature-turn-mutations`).
//!
//! Child rows carry no owner column: their queries use the tenant part of the
//! scope plus the id of the chat the owner-scoped load already authorized.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use async_trait::async_trait;
use sea_orm::ActiveValue::Set;
use sea_orm::sea_query::Expr;
use sea_orm::{ColumnTrait, Condition, EntityTrait, Order, QueryFilter};
use time::OffsetDateTime;
use toolkit_db::outbox::Wake;
use toolkit_db::secure::{
    ScopeError, SecureDeleteExt, SecureEntityExt, SecureInsertExt, SecureUpdateExt,
};
use toolkit_db::{DBProvider, DbTx};
use toolkit_security::AccessScope;
use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::domain::service::mutations::ports::{DeletePlan, MutationRepository, ReplacementPlan};
use crate::domain::service::mutations::summary::summary_covers;
use crate::domain::service::mutations::types::{LatestTurn, TargetMessage, TargetTurn};
use crate::domain::service::quota::reserve::{CONFLICT_BACKOFF, MAX_TX_ATTEMPTS};
use crate::domain::service::stream::ports::MessageBoundary;
use crate::domain::service::stream::types::{TurnRecord, TurnState};
use crate::infra::db::entities::{
    attachment, chat, chat_turn, message, message_attachment, thread_summary,
};
use crate::infra::db::mapping::scope_error;
use crate::infra::outbox::audit::enqueue_mutation_audit_event;
use crate::infra::outbox::queues::OutboxEnqueuer;
use toolkit_db::contention::is_retryable_contention;

/// The terminal states a mutation may act on.
const TERMINAL: [&str; 3] = ["completed", "failed", "cancelled"];

/// The future of one transaction attempt.
type AttemptFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, DomainError>> + Send + 'a>>;

/// Secure ORM mutation store.
pub struct SeaMutationRepository {
    db: Arc<DBProvider<DomainError>>,
    outbox: Arc<OutboxEnqueuer>,
}

impl SeaMutationRepository {
    /// Creates the store over the database provider and the outbox enqueuer
    /// (the audit queue).
    #[must_use]
    pub fn new(db: Arc<DBProvider<DomainError>>, outbox: Arc<OutboxEnqueuer>) -> Self {
        Self { db, outbox }
    }

    /// Runs `attempt` in a transaction; a lock or serialization conflict
    /// re-runs the whole transaction, at most `MAX_TX_ATTEMPTS` attempts in
    /// total with a short backoff. A unique violation is not a conflict.
    async fn with_retry<T, F>(&self, attempt: F) -> Result<T, DomainError>
    where
        T: Send + 'static,
        F: for<'a> Fn(&'a DbTx<'a>) -> AttemptFuture<'a, T> + Send + Sync + 'static,
    {
        let backend = self.db.db().backend();
        let attempt = Arc::new(attempt);
        let mut number = 0_u32;
        loop {
            number += 1;
            let run = Arc::clone(&attempt);
            let result = self
                .db
                .transaction(move |tx| Box::pin(async move { run(tx).await }))
                .await;
            match result {
                Err(DomainError::Database(message))
                    if number < MAX_TX_ATTEMPTS
                        && is_retryable_contention(
                            backend,
                            &sea_orm::DbErr::Custom(message.clone()),
                        ) =>
                {
                    tracing::warn!(attempt = number, "mutation transaction conflict, retrying");
                    let pause = CONFLICT_BACKOFF
                        .get(usize::try_from(number - 1).unwrap_or(0))
                        .copied()
                        .unwrap_or_default();
                    tokio::time::sleep(pause).await;
                }
                // @cpt-begin:cpt-cf-mini-chat-state-turn-mutations-turn-visibility:p1:inst-stay-live
                other => return other,
                // @cpt-end:cpt-cf-mini-chat-state-turn-mutations-turn-visibility:p1:inst-stay-live
            }
        }
    }
}

fn turn_record(row: chat_turn::Model) -> Option<TurnRecord> {
    Some(TurnRecord {
        id: row.id,
        tenant_id: row.tenant_id,
        chat_id: row.chat_id,
        request_id: row.request_id,
        requester_user_id: row.requester_user_id,
        state: TurnState::parse(&row.state)?,
        assistant_message_id: row.assistant_message_id,
        error_code: row.error_code,
        effective_model: row.effective_model,
        started_at: row.started_at,
        updated_at: row.updated_at,
        deleted: row.deleted_at.is_some(),
    })
}

/// One Secure ORM INSERT; a unique violation is the typed
/// [`DomainError::UniqueViolation`].
async fn insert<E>(
    tx: &DbTx<'_>,
    scope: &AccessScope,
    model: E::ActiveModel,
) -> Result<(), DomainError>
where
    E: sea_orm::EntityTrait + toolkit_db::secure::ScopableEntity,
    E::ActiveModel: Clone + Send + sea_orm::ActiveModelTrait<Entity = E>,
{
    E::insert(model.clone())
        .secure()
        .scope_with_model(scope, &model)
        .map_err(|error| match error {
            ScopeError::Denied(_) | ScopeError::TenantNotInScope { .. } => DomainError::AuthzDenied,
            other => scope_error(&other),
        })?
        .exec(tx)
        .await
        .map(|_| ())
        .map_err(|error| scope_error(&error))
}

/// `(started_at, id)` strictly after the target's.
fn newer_than(started_at: OffsetDateTime, id: Uuid) -> Condition {
    Condition::any()
        .add(chat_turn::Column::StartedAt.gt(started_at))
        .add(
            Condition::all()
                .add(chat_turn::Column::StartedAt.eq(started_at))
                .add(chat_turn::Column::Id.gt(id)),
        )
}

/// The guarded soft-delete of the target turn; `rows_affected` is 0 when a
/// concurrent mutation committed first or the turn is not terminal.
async fn soft_delete_turn(
    tx: &DbTx<'_>,
    tenant: &AccessScope,
    turn_id: Uuid,
    now: OffsetDateTime,
    replaced_by: Option<Uuid>,
) -> Result<u64, DomainError> {
    use chat_turn::Column as C;
    let mut statement = chat_turn::Entity::update_many()
        .col_expr(C::DeletedAt, Expr::value(now))
        .col_expr(C::UpdatedAt, Expr::value(now));
    if let Some(request) = replaced_by {
        statement = statement.col_expr(C::ReplacedByRequestId, Expr::value(request));
    }
    statement
        .filter(
            Condition::all()
                .add(C::Id.eq(turn_id))
                .add(C::DeletedAt.is_null())
                .add(C::State.is_in(TERMINAL)),
        )
        .secure()
        .scope_with(tenant)
        .exec(tx)
        .await
        .map(|result| result.rows_affected)
        .map_err(|error| scope_error(&error))
}

/// Whether a live turn of the chat is newer than the target (the re-check).
async fn newer_turn_exists(
    tx: &DbTx<'_>,
    tenant: &AccessScope,
    chat_id: Uuid,
    exclude: Option<Uuid>,
    target_started_at: OffsetDateTime,
    target_id: Uuid,
) -> Result<bool, DomainError> {
    use chat_turn::Column as C;
    let mut filter = Condition::all()
        .add(C::ChatId.eq(chat_id))
        .add(C::DeletedAt.is_null())
        .add(newer_than(target_started_at, target_id));
    if let Some(id) = exclude {
        filter = filter.add(C::Id.ne(id));
    }
    let row = chat_turn::Entity::find()
        .filter(filter)
        .secure()
        .scope_with(tenant)
        .limit(1)
        .one(tx)
        .await
        .map_err(|error| scope_error(&error))?;
    Ok(row.is_some())
}

/// Soft-deletes the live messages of `request_id`.
async fn soft_delete_messages(
    tx: &DbTx<'_>,
    tenant: &AccessScope,
    chat_id: Uuid,
    request_id: Uuid,
    now: OffsetDateTime,
) -> Result<(), DomainError> {
    message::Entity::update_many()
        .col_expr(message::Column::DeletedAt, Expr::value(now))
        .filter(
            Condition::all()
                .add(message::Column::ChatId.eq(chat_id))
                .add(message::Column::RequestId.eq(request_id))
                .add(message::Column::DeletedAt.is_null()),
        )
        .secure()
        .scope_with(tenant)
        .exec(tx)
        .await
        .map(|_| ())
        .map_err(|error| scope_error(&error))
}

// @cpt-dod:cpt-cf-mini-chat-dod-turn-mutations-summary-invalidation:p1
/// The summary invalidation: a covering summary is deleted and every
/// `is_compressed` flag of the chat cleared (soft-deleted rows included).
async fn invalidate_summary(
    tx: &DbTx<'_>,
    tenant: &AccessScope,
    chat_id: Uuid,
    user_message: MessageBoundary,
) -> Result<(), DomainError> {
    // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-invalidate-summary:p1:inst-select-summary-tx
    // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-invalidate-summary:p1:inst-if-no-summary
    // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-invalidate-summary:p1:inst-return-no-summary
    let Some(row) = thread_summary::Entity::find()
        .filter(Condition::all().add(thread_summary::Column::ChatId.eq(chat_id)))
        .secure()
        .scope_with(tenant)
        .one(tx)
        .await
        .map_err(|error| scope_error(&error))?
    else {
        return Ok(());
    };
    // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-invalidate-summary:p1:inst-return-no-summary
    // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-invalidate-summary:p1:inst-if-no-summary
    // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-invalidate-summary:p1:inst-select-summary-tx
    let frontier = MessageBoundary {
        created_at: row.summarized_up_to_created_at,
        id: row.summarized_up_to_message_id,
    };
    // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-invalidate-summary:p1:inst-return-kept
    if !summary_covers(frontier, user_message) {
        return Ok(());
    }
    // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-invalidate-summary:p1:inst-return-kept
    // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-invalidate-summary:p1:inst-if-summary-covers
    // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-invalidate-summary:p1:inst-delete-summary
    // @cpt-begin:cpt-cf-mini-chat-state-background-workers-summary-frontier:p1:inst-invalidated-by-mutation
    thread_summary::Entity::delete_many()
        .filter(Condition::all().add(thread_summary::Column::ChatId.eq(chat_id)))
        .secure()
        .scope_with(tenant)
        .exec(tx)
        .await
        .map_err(|error| scope_error(&error))?;
    // @cpt-end:cpt-cf-mini-chat-state-background-workers-summary-frontier:p1:inst-invalidated-by-mutation
    // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-invalidate-summary:p1:inst-delete-summary
    // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-invalidate-summary:p1:inst-clear-compressed
    // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-invalidate-summary:p1:inst-return-invalidated
    message::Entity::update_many()
        .col_expr(message::Column::IsCompressed, Expr::value(false))
        .filter(
            Condition::all()
                .add(message::Column::ChatId.eq(chat_id))
                .add(message::Column::IsCompressed.eq(true)),
        )
        .secure()
        .scope_with(tenant)
        .exec(tx)
        .await
        .map(|_| ())
        .map_err(|error| scope_error(&error))
    // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-invalidate-summary:p1:inst-return-invalidated
    // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-invalidate-summary:p1:inst-clear-compressed
    // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-invalidate-summary:p1:inst-if-summary-covers
}

#[async_trait]
impl MutationRepository for SeaMutationRepository {
    async fn find_target(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
        request_id: Uuid,
    ) -> Result<Option<TargetTurn>, DomainError> {
        let conn = self.db.conn()?;
        // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-preview:p1:inst-select-target
        let row = chat_turn::Entity::find()
            .filter(
                Condition::all()
                    .add(chat_turn::Column::ChatId.eq(chat_id))
                    .add(chat_turn::Column::RequestId.eq(request_id)),
            )
            .secure()
            .scope_with(&scope.tenant_only())
            .one(&conn)
            .await
            .map_err(|error| scope_error(&error))?;
        // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-preview:p1:inst-select-target
        Ok(row.and_then(|row| {
            let web_search_enabled = row.web_search_enabled;
            turn_record(row).map(|turn| TargetTurn {
                turn,
                web_search_enabled,
            })
        }))
    }

    async fn latest_turn(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
    ) -> Result<Option<LatestTurn>, DomainError> {
        let conn = self.db.conn()?;
        // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-preview:p1:inst-select-latest
        let row = chat_turn::Entity::find()
            .filter(
                Condition::all()
                    .add(chat_turn::Column::ChatId.eq(chat_id))
                    .add(chat_turn::Column::DeletedAt.is_null()),
            )
            .secure()
            .scope_with(&scope.tenant_only())
            .order_by(chat_turn::Column::StartedAt, Order::Desc)
            .order_by(chat_turn::Column::Id, Order::Desc)
            .limit(1)
            .one(&conn)
            .await
            .map_err(|error| scope_error(&error))?;
        // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-preview:p1:inst-select-latest
        Ok(row.map(|row| LatestTurn {
            id: row.id,
            started_at: row.started_at,
        }))
    }

    async fn user_message(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
        request_id: Uuid,
    ) -> Result<Option<TargetMessage>, DomainError> {
        let conn = self.db.conn()?;
        // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-preview:p1:inst-select-user-message
        let row = message::Entity::find()
            .filter(
                Condition::all()
                    .add(message::Column::ChatId.eq(chat_id))
                    .add(message::Column::RequestId.eq(request_id))
                    .add(message::Column::Role.eq("user"))
                    .add(message::Column::DeletedAt.is_null()),
            )
            .secure()
            .scope_with(&scope.tenant_only())
            .one(&conn)
            .await
            .map_err(|error| scope_error(&error))?;
        // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-preview:p1:inst-select-user-message
        Ok(row.map(|row| TargetMessage {
            id: row.id,
            content: row.content,
            created_at: row.created_at,
        }))
    }

    async fn carried_attachment_ids(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
        user_message_id: Uuid,
    ) -> Result<Vec<Uuid>, DomainError> {
        let conn = self.db.conn()?;
        let tenant = scope.tenant_only();
        carried_ids(&conn, &tenant, chat_id, user_message_id).await
    }

    async fn history_boundary(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
        new_request_id: Uuid,
    ) -> Result<Option<MessageBoundary>, DomainError> {
        let conn = self.db.conn()?;
        // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-load-replacement-history:p1:inst-replacement-boundary
        let row = message::Entity::find()
            .filter(
                Condition::all()
                    .add(message::Column::ChatId.eq(chat_id))
                    .add(message::Column::RequestId.is_not_null())
                    .add(message::Column::RequestId.ne(new_request_id))
                    .add(message::Column::DeletedAt.is_null()),
            )
            .secure()
            .scope_with(&scope.tenant_only())
            .order_by(message::Column::CreatedAt, Order::Desc)
            .order_by(message::Column::Id, Order::Desc)
            .limit(1)
            .one(&conn)
            .await
            .map_err(|error| scope_error(&error))?;
        // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-load-replacement-history:p1:inst-replacement-boundary
        Ok(row.map(|row| MessageBoundary {
            created_at: row.created_at,
            id: row.id,
        }))
    }

    async fn replace_turn(&self, plan: &ReplacementPlan) -> Result<(), DomainError> {
        let plan = Arc::new(plan.clone());
        let outbox = Arc::clone(&self.outbox);
        // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-replace-turn:p1:inst-return-replacement-error
        let wake = self
            .with_retry(move |tx| {
                let (plan, outbox) = (Arc::clone(&plan), Arc::clone(&outbox));
                Box::pin(async move { replace_in_tx(tx, &plan, &outbox).await })
            })
            .await?;
        // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-replace-turn:p1:inst-return-replacement-error
        // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-replace-turn:p1:inst-commit-replacement
        wake.fire();
        // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-replace-turn:p1:inst-commit-replacement
        // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-replace-turn:p1:inst-return-new-turn
        Ok(())
        // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-replace-turn:p1:inst-return-new-turn
    }

    async fn delete_turn(&self, plan: &DeletePlan) -> Result<(), DomainError> {
        let plan = Arc::new(plan.clone());
        let outbox = Arc::clone(&self.outbox);
        // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-remove-turn:p1:inst-return-delete-error
        let wake = self
            .with_retry(move |tx| {
                let (plan, outbox) = (Arc::clone(&plan), Arc::clone(&outbox));
                Box::pin(async move { delete_in_tx(tx, &plan, &outbox).await })
            })
            .await?;
        // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-remove-turn:p1:inst-return-delete-error
        // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-remove-turn:p1:inst-commit-delete
        wake.fire();
        // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-remove-turn:p1:inst-commit-delete
        // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-remove-turn:p1:inst-return-deleted
        Ok(())
        // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-remove-turn:p1:inst-return-deleted
    }

    async fn mark_setup_failed(
        &self,
        tenant_id: Uuid,
        turn_id: Uuid,
        error_code: &str,
        at: OffsetDateTime,
    ) -> Result<u64, DomainError> {
        use chat_turn::Column as C;
        let conn = self.db.conn()?;
        // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-fail-setup:p1:inst-mark-failed
        chat_turn::Entity::update_many()
            .col_expr(C::State, Expr::value("failed"))
            .col_expr(C::ErrorCode, Expr::value(error_code))
            .col_expr(C::CompletedAt, Expr::value(at))
            .col_expr(C::UpdatedAt, Expr::value(at))
            .filter(
                Condition::all()
                    .add(C::Id.eq(turn_id))
                    .add(C::State.eq("running"))
                    .add(C::ReserveTokens.is_null()),
            )
            .secure()
            .scope_with(&AccessScope::for_tenant(tenant_id))
            .exec(&conn)
            .await
            .map(|result| result.rows_affected)
            .map_err(|error| scope_error(&error))
        // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-fail-setup:p1:inst-mark-failed
    }
}

/// The attachment ids linked to `user_message_id` whose attachment row is not
/// deleted, in link order.
async fn carried_ids<R>(
    runner: &R,
    tenant: &AccessScope,
    chat_id: Uuid,
    user_message_id: Uuid,
) -> Result<Vec<Uuid>, DomainError>
where
    R: toolkit_db::secure::DBRunner,
{
    // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-load-mutation-inputs:p1:inst-select-old-links
    let links = message_attachment::Entity::find()
        .filter(
            Condition::all()
                .add(message_attachment::Column::ChatId.eq(chat_id))
                .add(message_attachment::Column::MessageId.eq(user_message_id)),
        )
        .secure()
        .scope_with(tenant)
        .order_by(message_attachment::Column::CreatedAt, Order::Asc)
        .order_by(message_attachment::Column::AttachmentId, Order::Asc)
        .all(runner)
        .await
        .map_err(|error| scope_error(&error))?;
    // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-load-mutation-inputs:p1:inst-select-old-links
    if links.is_empty() {
        return Ok(Vec::new());
    }
    // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-load-mutation-inputs:p1:inst-carried-ids
    let live: Vec<Uuid> = attachment::Entity::find()
        .filter(
            Condition::all()
                .add(attachment::Column::ChatId.eq(chat_id))
                .add(attachment::Column::Id.is_in(links.iter().map(|link| link.attachment_id)))
                .add(attachment::Column::DeletedAt.is_null()),
        )
        .secure()
        .scope_with(tenant)
        .all(runner)
        .await
        .map_err(|error| scope_error(&error))?
        .into_iter()
        .map(|row| row.id)
        .collect();
    Ok(links
        .into_iter()
        .map(|link| link.attachment_id)
        .filter(|id| live.contains(id))
        .collect())
    // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-load-mutation-inputs:p1:inst-carried-ids
}

/// The replacement transaction
/// (`cpt-cf-mini-chat-algo-turn-mutations-replace-turn`): write-first.
// @cpt-dod:cpt-cf-mini-chat-dod-turn-mutations-replacement-transaction:p1
async fn replace_in_tx(
    tx: &DbTx<'_>,
    plan: &ReplacementPlan,
    outbox: &OutboxEnqueuer,
) -> Result<Wake, DomainError> {
    let tenant = AccessScope::for_tenant(plan.tenant_id);
    // 1. The new running turn: the one-running-turn index decides a race.
    // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-replace-turn:p1:inst-insert-new-turn
    let turn = chat_turn::ActiveModel {
        id: Set(plan.new_turn_id),
        tenant_id: Set(plan.tenant_id),
        chat_id: Set(plan.chat_id),
        request_id: Set(plan.new_request_id),
        requester_type: Set("user".to_owned()),
        requester_user_id: Set(Some(plan.user_id)),
        state: Set("running".to_owned()),
        provider_name: Set(None),
        provider_response_id: Set(None),
        assistant_message_id: Set(None),
        error_code: Set(None),
        reserve_tokens: Set(None),
        max_output_tokens_applied: Set(None),
        reserved_credits_micro: Set(None),
        policy_version_applied: Set(None),
        effective_model: Set(None),
        minimal_generation_floor_applied: Set(None),
        error_detail: Set(None),
        deleted_at: Set(None),
        replaced_by_request_id: Set(None),
        started_at: Set(plan.now),
        last_progress_at: Set(Some(plan.now)),
        web_search_enabled: Set(plan.web_search_enabled),
        web_search_completed_count: Set(0),
        code_interpreter_completed_count: Set(0),
        file_search_completed_count: Set(0),
        completed_at: Set(None),
        updated_at: Set(plan.now),
    };
    insert::<chat_turn::Entity>(tx, &tenant, turn).await?;
    // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-replace-turn:p1:inst-insert-new-turn
    // 2. The guarded soft-delete of the old turn.
    // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-replace-turn:p1:inst-soft-delete-old-turn
    // @cpt-begin:cpt-cf-mini-chat-state-turn-mutations-turn-visibility:p1:inst-to-replaced
    // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-replace-turn:p1:inst-if-old-turn-gone
    // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-replace-turn:p1:inst-return-old-turn-gone
    if soft_delete_turn(
        tx,
        &tenant,
        plan.target_turn_id,
        plan.now,
        Some(plan.new_request_id),
    )
    .await?
        == 0
    {
        return Err(DomainError::NotLatestTurn);
    }
    // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-replace-turn:p1:inst-return-old-turn-gone
    // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-replace-turn:p1:inst-if-old-turn-gone
    // @cpt-end:cpt-cf-mini-chat-state-turn-mutations-turn-visibility:p1:inst-to-replaced
    // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-replace-turn:p1:inst-soft-delete-old-turn
    // 3. The latest-turn re-check.
    // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-replace-turn:p1:inst-recheck-latest
    // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-replace-turn:p1:inst-if-newer-turn
    // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-replace-turn:p1:inst-return-newer-turn
    if newer_turn_exists(
        tx,
        &tenant,
        plan.chat_id,
        Some(plan.new_turn_id),
        plan.target_started_at,
        plan.target_turn_id,
    )
    .await?
    {
        return Err(DomainError::NotLatestTurn);
    }
    // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-replace-turn:p1:inst-return-newer-turn
    // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-replace-turn:p1:inst-if-newer-turn
    // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-replace-turn:p1:inst-recheck-latest
    // 4. The old messages.
    // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-replace-turn:p1:inst-soft-delete-old-messages
    soft_delete_messages(tx, &tenant, plan.chat_id, plan.target_request_id, plan.now).await?;
    // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-replace-turn:p1:inst-soft-delete-old-messages
    // 5. The new user message.
    // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-replace-turn:p1:inst-insert-new-user-message
    let user_message = message::ActiveModel {
        id: Set(plan.new_user_message_id),
        tenant_id: Set(plan.tenant_id),
        chat_id: Set(plan.chat_id),
        request_id: Set(Some(plan.new_request_id)),
        role: Set("user".to_owned()),
        content: Set(plan.content.clone()),
        content_type: Set("text".to_owned()),
        token_estimate: Set(0),
        provider_response_id: Set(None),
        request_kind: Set("chat".to_owned()),
        features_used: Set(serde_json::json!([])),
        input_tokens: Set(0),
        output_tokens: Set(0),
        cache_read_input_tokens: Set(0),
        cache_write_input_tokens: Set(0),
        reasoning_tokens: Set(0),
        model: Set(None),
        is_compressed: Set(false),
        created_at: Set(plan.now),
        deleted_at: Set(None),
    };
    insert::<message::Entity>(tx, &tenant, user_message).await?;
    // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-replace-turn:p1:inst-insert-new-user-message
    // 6. The carried links: the old message's links whose attachment is live.
    // The old message was soft-deleted above, but its links are not touched.
    let old_message = plan.target_user_message.id;
    // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-replace-turn:p1:inst-select-carried-links
    let carried = carried_ids(tx, &tenant, plan.chat_id, old_message).await?;
    // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-replace-turn:p1:inst-select-carried-links
    // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-replace-turn:p1:inst-for-each-carried-link
    for attachment_id in carried {
        // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-replace-turn:p1:inst-insert-carried-link
        let link = message_attachment::ActiveModel {
            tenant_id: Set(plan.tenant_id),
            chat_id: Set(plan.chat_id),
            message_id: Set(plan.new_user_message_id),
            attachment_id: Set(attachment_id),
            created_at: Set(plan.now),
        };
        insert::<message_attachment::Entity>(tx, &tenant, link).await?;
        // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-replace-turn:p1:inst-insert-carried-link
    }
    // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-replace-turn:p1:inst-for-each-carried-link
    // 7. The summary.
    // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-replace-turn:p1:inst-replace-invalidate-summary
    invalidate_summary(tx, &tenant, plan.chat_id, plan.target_user_message).await?;
    // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-replace-turn:p1:inst-replace-invalidate-summary
    // 8. The chat bump.
    // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-replace-turn:p1:inst-touch-chat-mutation
    chat::Entity::update_many()
        .col_expr(chat::Column::UpdatedAt, Expr::value(plan.now))
        .filter(Condition::all().add(chat::Column::Id.eq(plan.chat_id)))
        .secure()
        .scope_with(&AccessScope::for_tenant(plan.tenant_id).ensure_owner(plan.user_id))
        .exec(tx)
        .await
        .map_err(|error| scope_error(&error))?;
    // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-replace-turn:p1:inst-touch-chat-mutation
    // 9. The audit event.
    // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-replace-turn:p1:inst-enqueue-replacement-audit
    enqueue_mutation_audit_event(tx, outbox, &plan.audit).await
    // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-replace-turn:p1:inst-enqueue-replacement-audit
}

/// The delete transaction (`cpt-cf-mini-chat-algo-turn-mutations-remove-turn`):
/// write-first with the guarded soft-delete of the turn.
async fn delete_in_tx(
    tx: &DbTx<'_>,
    plan: &DeletePlan,
    outbox: &OutboxEnqueuer,
) -> Result<Wake, DomainError> {
    let tenant = AccessScope::for_tenant(plan.tenant_id);
    // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-remove-turn:p1:inst-soft-delete-turn
    // @cpt-begin:cpt-cf-mini-chat-state-turn-mutations-turn-visibility:p1:inst-to-deleted
    // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-remove-turn:p1:inst-if-delete-missed
    // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-remove-turn:p1:inst-return-delete-missed
    if soft_delete_turn(tx, &tenant, plan.target_turn_id, plan.now, None).await? == 0 {
        return Err(DomainError::NotLatestTurn);
    }
    // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-remove-turn:p1:inst-return-delete-missed
    // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-remove-turn:p1:inst-if-delete-missed
    // @cpt-end:cpt-cf-mini-chat-state-turn-mutations-turn-visibility:p1:inst-to-deleted
    // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-remove-turn:p1:inst-soft-delete-turn
    // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-remove-turn:p1:inst-recheck-latest-delete
    // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-remove-turn:p1:inst-if-newer-delete
    // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-remove-turn:p1:inst-return-newer-delete
    if newer_turn_exists(
        tx,
        &tenant,
        plan.chat_id,
        None,
        plan.target_started_at,
        plan.target_turn_id,
    )
    .await?
    {
        return Err(DomainError::NotLatestTurn);
    }
    // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-remove-turn:p1:inst-return-newer-delete
    // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-remove-turn:p1:inst-if-newer-delete
    // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-remove-turn:p1:inst-recheck-latest-delete
    // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-remove-turn:p1:inst-soft-delete-messages
    soft_delete_messages(tx, &tenant, plan.chat_id, plan.target_request_id, plan.now).await?;
    // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-remove-turn:p1:inst-soft-delete-messages
    // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-remove-turn:p1:inst-delete-invalidate-summary
    invalidate_summary(tx, &tenant, plan.chat_id, plan.target_user_message).await?;
    // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-remove-turn:p1:inst-delete-invalidate-summary
    // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-remove-turn:p1:inst-enqueue-delete-audit
    enqueue_mutation_audit_event(tx, outbox, &plan.audit).await
    // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-remove-turn:p1:inst-enqueue-delete-audit
}

#[cfg(test)]
#[path = "mutations_tests.rs"]
mod reads_tests;

#[cfg(test)]
#[path = "mutations_replace_tests.rs"]
mod replace_tests;

#[cfg(test)]
#[path = "mutations_delete_tests.rs"]
mod delete_tests;

#[cfg(test)]
#[path = "mutations_concurrency_tests.rs"]
mod concurrency_tests;
