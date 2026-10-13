//! `SeaORM` implementation of the turn-mutation repository over Secure ORM (DESIGN section 3.9):
//! the mutation commit of retry, edit and delete, and the reserve of an unstarted retry/edit turn.
//!
//! The mutation commit is one transaction. The conditional update that soft-deletes the target
//! (`deleted_at IS NULL`, terminal state, requester) is the row lock and the compare-and-set of the
//! atomicity invariant: of two concurrent mutations of the same turn exactly one affects the row,
//! the other one is refused. The one-running-turn unique index decides a lost insert race. The
//! first statement of the transaction writes the chat row, which serializes the mutation with a
//! concurrent send before the latest-turn check.

use std::sync::Arc;

use async_trait::async_trait;
use mini_chat_sdk::{TurnDeleteAudit, TurnMutationAuditEvent, TurnReplacementAudit};
use sea_orm::sea_query::Expr;
use sea_orm::{ActiveValue, ColumnTrait, Condition, EntityTrait, QueryFilter};
use time::OffsetDateTime;
use toolkit_db::DBProvider;
use toolkit_db::DbTx;
use toolkit_db::outbox::{Wake, in_transaction};
use toolkit_db::secure::{
    DBRunner, SecureDeleteExt, SecureEntityExt, SecureUpdateExt, secure_insert,
};
use toolkit_security::AccessScope;
use uuid::Uuid;

use super::entity::{
    attachments, chat_turns, chats, message_attachments, messages, thread_summaries,
};
use super::map_scope_err;
use super::quota_repo;
use super::rows;
use crate::domain::error::{DomainError, ProtectedResource};
use crate::domain::mutation::{
    DeleteCommand, MutationOp, MutationRepository, OriginalMessage, ReplaceCommand,
};
use crate::domain::stream::{AttachmentRef, MutationReserveCommand};
use crate::domain::turn::TurnState;
use crate::infra::outbox::OutboxEnqueuer;

const RUNNING: &str = "running";

/// Mutation repository over the gear's database.
pub struct MutationRepositoryImpl {
    db: Arc<DBProvider<DomainError>>,
    outbox: Arc<OutboxEnqueuer>,
}

impl MutationRepositoryImpl {
    /// Creates the repository.
    #[must_use]
    pub fn new(db: Arc<DBProvider<DomainError>>, outbox: Arc<OutboxEnqueuer>) -> Self {
        Self { db, outbox }
    }
}

/// `(started_at, id)` strictly after the target's: a newer turn.
fn started_after(started_at: OffsetDateTime, id: Uuid) -> Condition {
    Condition::any()
        .add(chat_turns::Column::StartedAt.gt(started_at))
        .add(
            Condition::all()
                .add(chat_turns::Column::StartedAt.eq(started_at))
                .add(chat_turns::Column::Id.gt(id)),
        )
}

/// The turn row of a request, soft-deleted ones included.
async fn find_turn(
    tx: &DbTx<'_>,
    tenant_scope: &AccessScope,
    chat_id: Uuid,
    request_id: Uuid,
) -> Result<Option<chat_turns::Model>, DomainError> {
    chat_turns::Entity::find()
        .filter(chat_turns::Column::ChatId.eq(chat_id))
        .filter(chat_turns::Column::RequestId.eq(request_id))
        .secure()
        .scope_with(tenant_scope)
        .one(tx)
        .await
        .map_err(map_scope_err)
}

/// Why the target of a mutation can no longer be mutated.
fn refusal(turn: Option<&chat_turns::Model>, request_id: Uuid, user_id: Uuid) -> DomainError {
    let Some(turn) = turn else {
        return DomainError::TurnNotFound { request_id };
    };
    if turn.requester_user_id != Some(user_id) {
        return DomainError::AuthzDenied {
            resource: ProtectedResource::Chat,
        };
    }
    if turn.state == RUNNING && turn.deleted_at.is_none() {
        return DomainError::InvalidTurnState;
    }
    DomainError::NotLatestTurn
}

/// The target of a mutation commit.
struct Target {
    user_id: Uuid,
    chat_id: Uuid,
    request_id: Uuid,
    replaced_by: Option<Uuid>,
    now: OffsetDateTime,
}

/// Takes the write lock of the chat row, the first statement of a mutation transaction.
///
/// A send commits its user message, its quota rows and its new turn in a transaction that also
/// writes the chat row (`updated_at` bump). Writing the chat row first serializes a mutation with
/// such a send, so the latest-turn check that follows (a fresh READ COMMITTED statement) sees every
/// turn a concurrent send has committed: a deleted or replaced turn is never left behind a live
/// newer one. The lock order is quota rows, chat row, turn rows; a mutation touches no quota rows
/// before this statement, so the order cannot cycle.
///
/// `bump` is the new `updated_at` of a mutation that moves the chat's activity (retry, edit); a
/// delete passes `None` and writes the unchanged value, which takes the same lock.
///
/// The same lock serializes the attachment transactions with a chat soft delete and with each other
/// (an upload's insert, an attachment delete), and the reserve of a send with an attachment delete.
/// The update affects no row once the chat is soft-deleted: `ChatNotFound`.
pub(super) async fn lock_chat(
    tx: &DbTx<'_>,
    scope: &AccessScope,
    chat_id: Uuid,
    bump: Option<OffsetDateTime>,
) -> Result<(), DomainError> {
    let updated_at = match bump {
        Some(now) => Expr::value(now),
        None => Expr::col(chats::Column::UpdatedAt),
    };
    let touched = chats::Entity::update_many()
        .col_expr(chats::Column::UpdatedAt, updated_at)
        .filter(chats::Column::Id.eq(chat_id))
        .filter(chats::Column::DeletedAt.is_null())
        .secure()
        .scope_with(scope)
        .exec(tx)
        .await
        .map_err(map_scope_err)?
        .rows_affected;
    if touched == 0 {
        return Err(DomainError::ChatNotFound { chat_id });
    }
    Ok(())
}

/// Re-validates the target inside the transaction and soft-deletes the turn and its messages.
///
/// The update is the compare-and-set: it affects the row only while the turn is not deleted, is in
/// a terminal state and belongs to the requester. The latest-turn check follows: no non-deleted
/// turn may start after the target.
async fn retire_turn(
    tx: &DbTx<'_>,
    tenant_scope: &AccessScope,
    target: &Target,
) -> Result<(), DomainError> {
    let affected = chat_turns::Entity::update_many()
        .col_expr(chat_turns::Column::DeletedAt, Expr::value(target.now))
        .col_expr(
            chat_turns::Column::ReplacedByRequestId,
            Expr::value(target.replaced_by),
        )
        .col_expr(chat_turns::Column::UpdatedAt, Expr::value(target.now))
        .filter(chat_turns::Column::ChatId.eq(target.chat_id))
        .filter(chat_turns::Column::RequestId.eq(target.request_id))
        .filter(chat_turns::Column::DeletedAt.is_null())
        .filter(chat_turns::Column::State.ne(RUNNING))
        .filter(chat_turns::Column::RequesterUserId.eq(target.user_id))
        .secure()
        .scope_with(tenant_scope)
        .exec(tx)
        .await
        .map_err(map_scope_err)?
        .rows_affected;
    let turn = find_turn(tx, tenant_scope, target.chat_id, target.request_id).await?;
    if affected == 0 {
        return Err(refusal(turn.as_ref(), target.request_id, target.user_id));
    }
    let turn = turn.ok_or(DomainError::TurnNotFound {
        request_id: target.request_id,
    })?;
    let newer = chat_turns::Entity::find()
        .filter(chat_turns::Column::ChatId.eq(target.chat_id))
        .filter(chat_turns::Column::DeletedAt.is_null())
        .filter(started_after(turn.started_at, turn.id))
        .secure()
        .scope_with(tenant_scope)
        .count(tx)
        .await
        .map_err(map_scope_err)?;
    if newer > 0 {
        return Err(DomainError::NotLatestTurn);
    }
    messages::Entity::update_many()
        .col_expr(messages::Column::DeletedAt, Expr::value(target.now))
        .filter(messages::Column::ChatId.eq(target.chat_id))
        .filter(messages::Column::RequestId.eq(target.request_id))
        .filter(messages::Column::DeletedAt.is_null())
        .secure()
        .scope_with(tenant_scope)
        .exec(tx)
        .await
        .map_err(map_scope_err)?;
    Ok(())
}

/// The user message of a turn, soft-deleted or not.
async fn find_user_message(
    tx: &impl toolkit_db::secure::DBRunner,
    tenant_scope: &AccessScope,
    chat_id: Uuid,
    request_id: Uuid,
) -> Result<Option<messages::Model>, DomainError> {
    Ok(messages::Entity::find()
        .filter(messages::Column::ChatId.eq(chat_id))
        .filter(messages::Column::RequestId.eq(request_id))
        .filter(messages::Column::Role.eq("user"))
        .secure()
        .scope_with(tenant_scope)
        .order_by(messages::Column::CreatedAt, sea_orm::Order::Asc)
        .order_by(messages::Column::Id, sea_orm::Order::Asc)
        .limit(1)
        .all(tx)
        .await
        .map_err(map_scope_err)?
        .into_iter()
        .next())
}

/// Deletes the chat's summary and clears `is_compressed` on all its messages when the summary
/// frontier is at or after the mutated turn's user message (DESIGN "Summary Interaction on Turn
/// Mutation"). A summary that ends before that message is kept.
async fn invalidate_summary(
    tx: &DbTx<'_>,
    tenant_scope: &AccessScope,
    chat_id: Uuid,
    user_message: &messages::Model,
) -> Result<(), DomainError> {
    let summary = thread_summaries::Entity::find()
        .filter(thread_summaries::Column::ChatId.eq(chat_id))
        .secure()
        .scope_with(tenant_scope)
        .one(tx)
        .await
        .map_err(map_scope_err)?;
    let Some(summary) = summary else {
        return Ok(());
    };
    let frontier = (
        summary.summarized_up_to_created_at,
        summary.summarized_up_to_message_id,
    );
    if frontier < (user_message.created_at, user_message.id) {
        return Ok(());
    }
    thread_summaries::Entity::delete_many()
        .filter(thread_summaries::Column::Id.eq(summary.id))
        .secure()
        .scope_with(tenant_scope)
        .exec(tx)
        .await
        .map_err(map_scope_err)?;
    messages::Entity::update_many()
        .col_expr(messages::Column::IsCompressed, Expr::value(false))
        .filter(messages::Column::ChatId.eq(chat_id))
        .filter(messages::Column::IsCompressed.eq(true))
        .secure()
        .scope_with(tenant_scope)
        .exec(tx)
        .await
        .map_err(map_scope_err)?;
    Ok(())
}

/// The attachment ids linked to a message in the order of the original request (link time, then
/// id).
async fn linked_in_order(
    runner: &impl DBRunner,
    tenant_scope: &AccessScope,
    chat_id: Uuid,
    message_id: Uuid,
) -> Result<Vec<Uuid>, DomainError> {
    let mut links = message_attachments::Entity::find()
        .filter(message_attachments::Column::ChatId.eq(chat_id))
        .filter(message_attachments::Column::MessageId.eq(message_id))
        .secure()
        .scope_with(tenant_scope)
        .all(runner)
        .await
        .map_err(map_scope_err)?;
    links.sort_by_key(|link| (link.created_at, link.attachment_id));
    Ok(links.into_iter().map(|link| link.attachment_id).collect())
}

/// Copies the links of the old user message to the new one, leaving out soft-deleted attachments.
async fn copy_attachment_links(
    tx: &DbTx<'_>,
    tenant_scope: &AccessScope,
    command: &ReplaceCommand,
    old_message: Uuid,
) -> Result<(), DomainError> {
    let linked = linked_in_order(tx, tenant_scope, command.chat_id, old_message).await?;
    if linked.is_empty() {
        return Ok(());
    }
    let live: Vec<Uuid> = attachments::Entity::find()
        .filter(attachments::Column::Id.is_in(linked.clone()))
        .filter(attachments::Column::ChatId.eq(command.chat_id))
        .filter(attachments::Column::DeletedAt.is_null())
        .secure()
        .scope_with(tenant_scope)
        .all(tx)
        .await
        .map_err(map_scope_err)?
        .into_iter()
        .map(|attachment| attachment.id)
        .collect();
    let kept = linked.into_iter().filter(|id| live.contains(id));
    for (position, attachment_id) in kept.enumerate() {
        let link = message_attachments::ActiveModel {
            tenant_id: ActiveValue::Set(command.tenant_id),
            chat_id: ActiveValue::Set(command.chat_id),
            message_id: ActiveValue::Set(command.new_user_message_id),
            attachment_id: ActiveValue::Set(attachment_id),
            created_at: ActiveValue::Set(rows::link_time(command.now, position)),
        };
        secure_insert::<message_attachments::Entity>(link, tenant_scope, tx)
            .await
            .map_err(map_scope_err)?;
    }
    Ok(())
}

/// The new `running` turn of a retry or an edit: the preflight columns stay NULL until the reserve.
fn unstarted_turn(command: &ReplaceCommand) -> chat_turns::ActiveModel {
    chat_turns::ActiveModel {
        id: ActiveValue::Set(command.new_turn_id),
        tenant_id: ActiveValue::Set(command.tenant_id),
        chat_id: ActiveValue::Set(command.chat_id),
        request_id: ActiveValue::Set(command.new_request_id),
        requester_type: ActiveValue::Set("user".to_owned()),
        requester_user_id: ActiveValue::Set(Some(command.user_id)),
        state: ActiveValue::Set(RUNNING.to_owned()),
        provider_name: ActiveValue::Set(None),
        provider_response_id: ActiveValue::Set(None),
        assistant_message_id: ActiveValue::Set(None),
        error_code: ActiveValue::Set(None),
        reserve_tokens: ActiveValue::Set(None),
        max_output_tokens_applied: ActiveValue::Set(None),
        reserved_credits_micro: ActiveValue::Set(None),
        policy_version_applied: ActiveValue::Set(None),
        effective_model: ActiveValue::Set(None),
        minimal_generation_floor_applied: ActiveValue::Set(None),
        error_detail: ActiveValue::Set(None),
        deleted_at: ActiveValue::Set(None),
        replaced_by_request_id: ActiveValue::Set(None),
        started_at: ActiveValue::Set(command.now),
        last_progress_at: ActiveValue::Set(Some(command.now)),
        web_search_enabled: ActiveValue::Set(command.web_search_enabled),
        web_search_completed_count: ActiveValue::Set(0),
        code_interpreter_completed_count: ActiveValue::Set(0),
        file_search_completed_count: ActiveValue::Set(0),
        completed_at: ActiveValue::Set(None),
        updated_at: ActiveValue::Set(command.now),
    }
}

fn replacement_audit(command: &ReplaceCommand) -> TurnMutationAuditEvent {
    let audit = TurnReplacementAudit {
        timestamp: command.now,
        tenant_id: command.tenant_id,
        actor_user_id: command.user_id,
        chat_id: command.chat_id,
        original_request_id: command.old_request_id,
        new_request_id: command.new_request_id,
    };
    match command.op {
        MutationOp::Edit => TurnMutationAuditEvent::Edit(audit),
        MutationOp::Retry | MutationOp::Delete => TurnMutationAuditEvent::Retry(audit),
    }
}

fn to_attachment_ref(row: attachments::Model) -> AttachmentRef {
    AttachmentRef {
        id: row.id,
        kind: row.attachment_kind,
        filename: row.filename,
        provider_file_id: row.provider_file_id,
        secondary_file_id: row.secondary_file_id,
    }
}

#[async_trait]
impl MutationRepository for MutationRepositoryImpl {
    async fn latest_turn_id(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
    ) -> Result<Option<Uuid>, DomainError> {
        let conn = self.db.conn()?;
        Ok(chat_turns::Entity::find()
            .filter(chat_turns::Column::ChatId.eq(chat_id))
            .filter(chat_turns::Column::DeletedAt.is_null())
            .secure()
            .scope_with(&scope.tenant_only())
            .order_by(chat_turns::Column::StartedAt, sea_orm::Order::Desc)
            .order_by(chat_turns::Column::Id, sea_orm::Order::Desc)
            .limit(1)
            .all(&conn)
            .await
            .map_err(map_scope_err)?
            .into_iter()
            .next()
            .map(|turn| turn.id))
    }

    async fn original_message(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
        request_id: Uuid,
    ) -> Result<Option<OriginalMessage>, DomainError> {
        let conn = self.db.conn()?;
        let tenant_scope = scope.tenant_only();
        let Some(message) = find_user_message(&conn, &tenant_scope, chat_id, request_id).await?
        else {
            return Ok(None);
        };
        let linked = linked_in_order(&conn, &tenant_scope, chat_id, message.id).await?;
        let attachments = if linked.is_empty() {
            Vec::new()
        } else {
            let rows = attachments::Entity::find()
                .filter(attachments::Column::Id.is_in(linked.clone()))
                .filter(attachments::Column::ChatId.eq(chat_id))
                .filter(attachments::Column::DeletedAt.is_null())
                .secure()
                .scope_with(&tenant_scope)
                .all(&conn)
                .await
                .map_err(map_scope_err)?;
            linked
                .iter()
                .filter_map(|id| rows.iter().find(|row| row.id == *id))
                .cloned()
                .map(to_attachment_ref)
                .collect()
        };
        Ok(Some(OriginalMessage {
            id: message.id,
            content: message.content,
            attachments,
        }))
    }

    async fn replace(
        &self,
        scope: &AccessScope,
        command: ReplaceCommand,
    ) -> Result<(), DomainError> {
        let scope = scope.clone();
        let outbox = Arc::clone(&self.outbox);
        let announced = in_transaction(&self.db.db(), move |tx| {
            Box::pin(async move {
                let tenant_scope = scope.tenant_only();
                lock_chat(tx, &scope, command.chat_id, Some(command.now)).await?;
                retire_turn(
                    tx,
                    &tenant_scope,
                    &Target {
                        user_id: command.user_id,
                        chat_id: command.chat_id,
                        request_id: command.old_request_id,
                        replaced_by: Some(command.new_request_id),
                        now: command.now,
                    },
                )
                .await?;
                let old_message =
                    find_user_message(tx, &tenant_scope, command.chat_id, command.old_request_id)
                        .await?
                        .ok_or_else(|| {
                            DomainError::internal("the replaced turn has no user message")
                        })?;
                invalidate_summary(tx, &tenant_scope, command.chat_id, &old_message).await?;
                secure_insert::<messages::Entity>(
                    rows::user_message(
                        command.new_user_message_id,
                        command.tenant_id,
                        command.chat_id,
                        command.new_request_id,
                        command.content.clone(),
                        command.now,
                    ),
                    &tenant_scope,
                    tx,
                )
                .await
                .map_err(map_scope_err)?;
                copy_attachment_links(tx, &tenant_scope, &command, old_message.id).await?;
                secure_insert::<chat_turns::Entity>(unstarted_turn(&command), &tenant_scope, tx)
                    .await
                    .map_err(|err| match map_scope_err(err) {
                        DomainError::UniqueViolation => DomainError::GenerationInProgress,
                        other => other,
                    })?;
                // Enqueue last: an error after the enqueue would drop the wake on rollback.
                let (wake, message) = outbox
                    .enqueue_mutation_audit(tx, &replacement_audit(&command))
                    .await?;
                Ok::<_, DomainError>((message, wake))
            })
        })
        .await?;
        self.outbox.announce(&[announced]);
        Ok(())
    }

    async fn delete(&self, scope: &AccessScope, command: DeleteCommand) -> Result<(), DomainError> {
        let scope = scope.clone();
        let outbox = Arc::clone(&self.outbox);
        let announced = in_transaction(&self.db.db(), move |tx| {
            Box::pin(async move {
                let tenant_scope = scope.tenant_only();
                lock_chat(tx, &scope, command.chat_id, None).await?;
                retire_turn(
                    tx,
                    &tenant_scope,
                    &Target {
                        user_id: command.user_id,
                        chat_id: command.chat_id,
                        request_id: command.request_id,
                        replaced_by: None,
                        now: command.now,
                    },
                )
                .await?;
                let old_message =
                    find_user_message(tx, &tenant_scope, command.chat_id, command.request_id)
                        .await?;
                if let Some(old_message) = old_message {
                    invalidate_summary(tx, &tenant_scope, command.chat_id, &old_message).await?;
                }
                let event = TurnMutationAuditEvent::Delete(TurnDeleteAudit {
                    timestamp: command.now,
                    tenant_id: command.tenant_id,
                    actor_user_id: command.user_id,
                    chat_id: command.chat_id,
                    request_id: command.request_id,
                });
                let (wake, message) = outbox.enqueue_mutation_audit(tx, &event).await?;
                Ok::<_, DomainError>((message, wake))
            })
        })
        .await?;
        self.outbox.announce(&[announced]);
        Ok(())
    }

    async fn reserve(
        &self,
        scope: &AccessScope,
        command: MutationReserveCommand,
    ) -> Result<(), DomainError> {
        let scope = scope.clone();
        in_transaction(&self.db.db(), move |tx| {
            Box::pin(async move {
                let tenant_scope = scope.tenant_only();
                quota_repo::reserve(
                    tx,
                    &scope,
                    command.tenant_id,
                    command.user_id,
                    &command.plan,
                    command.now,
                )
                .await?;
                let filled = chat_turns::Entity::update_many()
                    .col_expr(
                        chat_turns::Column::ReserveTokens,
                        Expr::value(command.reserve_tokens),
                    )
                    .col_expr(
                        chat_turns::Column::MaxOutputTokensApplied,
                        Expr::value(command.max_output_tokens_applied),
                    )
                    .col_expr(
                        chat_turns::Column::ReservedCreditsMicro,
                        Expr::value(command.plan.reserved_credits_micro),
                    )
                    .col_expr(
                        chat_turns::Column::PolicyVersionApplied,
                        Expr::value(command.policy_version),
                    )
                    .col_expr(
                        chat_turns::Column::EffectiveModel,
                        Expr::value(command.effective_model.clone()),
                    )
                    .col_expr(
                        chat_turns::Column::MinimalGenerationFloorApplied,
                        Expr::value(command.minimal_generation_floor_applied),
                    )
                    // A fresh progress mark makes a later orphan CAS, decided on a read from
                    // before this fill, fail its stale-progress predicate.
                    .col_expr(chat_turns::Column::LastProgressAt, Expr::value(command.now))
                    .col_expr(chat_turns::Column::UpdatedAt, Expr::value(command.now))
                    .filter(chat_turns::Column::Id.eq(command.turn_id))
                    .filter(chat_turns::Column::State.eq(RUNNING))
                    .filter(chat_turns::Column::ReserveTokens.is_null())
                    .secure()
                    .scope_with(&tenant_scope)
                    .exec(tx)
                    .await
                    .map_err(map_scope_err)?
                    .rows_affected;
                if filled != 1 {
                    return Err(DomainError::internal(
                        "the retry/edit turn is no longer waiting for its reserve",
                    ));
                }
                Ok(((), Wake::empty()))
            })
        })
        .await
    }

    async fn fail_unstarted(
        &self,
        scope: &AccessScope,
        turn_id: Uuid,
        error_code: &str,
        now: OffsetDateTime,
    ) -> Result<bool, DomainError> {
        let conn = self.db.conn()?;
        let affected = chat_turns::Entity::update_many()
            .col_expr(
                chat_turns::Column::State,
                Expr::value(TurnState::Failed.as_str()),
            )
            .col_expr(chat_turns::Column::ErrorCode, Expr::value(error_code))
            .col_expr(chat_turns::Column::CompletedAt, Expr::value(now))
            .col_expr(chat_turns::Column::UpdatedAt, Expr::value(now))
            .filter(chat_turns::Column::Id.eq(turn_id))
            .filter(chat_turns::Column::State.eq(RUNNING))
            // A filled reserve (an ambiguous reserve commit) must be settled: finalizing here
            // would leak it. With 0 rows the turn stays running for the normal finalization or
            // the orphan watchdog, which settle a filled reserve.
            .filter(chat_turns::Column::ReserveTokens.is_null())
            .secure()
            .scope_with(&scope.tenant_only())
            .exec(&conn)
            .await
            .map_err(map_scope_err)?
            .rows_affected;
        Ok(affected == 1)
    }
}
