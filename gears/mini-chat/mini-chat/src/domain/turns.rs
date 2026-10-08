//! Turn mutations: shared validation, summary invalidation and delete (DESIGN §3.9).

use mini_chat_sdk::{MiniChatAuditEvent, TurnMutationAuditEvent};
use sea_orm::sea_query::Expr;
use sea_orm::{ColumnTrait, Condition};
use toolkit_db::secure::DBRunner;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::domain::app::AppServices;
use crate::domain::authz::actions;
use crate::domain::error::{DomainError, DomainResult, Resource, retry_contention};
use crate::domain::time::now;
use crate::infra::db::entities::{chat, chat_turn, message};
use crate::infra::db::repo;
use crate::infra::outbox::Wakes;

/// Validated target of a turn mutation.
#[derive(Debug, Clone)]
pub struct MutationTarget {
    pub chat: chat::Model,
    pub turn: chat_turn::Model,
}

/// Read-only mutation preview: running → 400, not latest → 409, foreign → 403.
///
/// # Errors
/// `NotFound`, `TurnNotTerminal`, `NotLatestTurn`, `AuthzDenied`.
pub async fn preview_target(
    runner: &impl DBRunner,
    chat: &chat::Model,
    request_id: Uuid,
    caller: Uuid,
) -> DomainResult<chat_turn::Model> {
    let turn = repo::find_turn_by_request(runner, chat.tenant_id, chat.id, request_id)
        .await?
        .ok_or(DomainError::NotFound(Resource::Turn))?;
    if turn.state == "running" && turn.deleted_at.is_none() {
        return Err(DomainError::TurnNotTerminal);
    }
    let latest = repo::latest_turn(runner, chat.tenant_id, chat.id).await?;
    if turn.deleted_at.is_some() || latest.as_ref().map(|t| t.id) != Some(turn.id) {
        return Err(DomainError::NotLatestTurn);
    }
    if turn.requester_user_id != Some(caller) {
        return Err(DomainError::AuthzDenied);
    }
    Ok(turn)
}

/// Re-validates the target inside the mutation transaction and soft-deletes it.
///
/// # Errors
/// `NotLatestTurn` when another mutation won.
pub async fn soft_delete_target(
    runner: &impl DBRunner,
    chat: &chat::Model,
    turn: &chat_turn::Model,
    replaced_by: Option<Uuid>,
) -> DomainResult<()> {
    let latest = repo::latest_turn(runner, chat.tenant_id, chat.id).await?;
    if latest.as_ref().map(|t| t.id) != Some(turn.id) {
        return Err(DomainError::NotLatestTurn);
    }
    let ts = now();
    let mut cols = vec![
        (chat_turn::Column::DeletedAt, Expr::value(ts)),
        (chat_turn::Column::UpdatedAt, Expr::value(ts)),
    ];
    if let Some(r) = replaced_by {
        cols.push((chat_turn::Column::ReplacedByRequestId, Expr::value(r)));
    }
    let n = repo::update_turn_where(
        runner,
        chat.tenant_id,
        turn.id,
        Condition::all()
            .add(chat_turn::Column::DeletedAt.is_null())
            .add(chat_turn::Column::State.ne("running")),
        cols,
    )
    .await?;
    if n == 0 {
        return Err(DomainError::NotLatestTurn);
    }
    repo::soft_delete_turn_messages(runner, chat.tenant_id, chat.id, turn.request_id, ts).await?;
    Ok(())
}

/// Deletes the thread summary when its frontier is at or after the user message.
///
/// # Errors
/// Database errors.
pub async fn invalidate_summary_if_covers(
    runner: &impl DBRunner,
    chat: &chat::Model,
    user_msg: Option<&message::Model>,
) -> DomainResult<()> {
    let Some(summary) = repo::find_summary(runner, chat.tenant_id, chat.id).await? else {
        return Ok(());
    };
    let covers = user_msg.is_none_or(|m| {
        (
            summary.summarized_up_to_created_at,
            summary.summarized_up_to_message_id,
        ) >= (m.created_at, m.id)
    });
    if covers {
        repo::delete_summary(runner, chat.tenant_id, chat.id).await?;
        repo::clear_compressed(runner, chat.tenant_id, chat.id).await?;
    }
    Ok(())
}

/// The original (latest) user message of a turn, deleted or not.
///
/// # Errors
/// Database errors.
pub async fn turn_user_message(
    runner: &impl DBRunner,
    chat: &chat::Model,
    request_id: Uuid,
) -> DomainResult<Option<message::Model>> {
    let msgs = repo::turn_messages_any(runner, chat.tenant_id, chat.id, request_id).await?;
    Ok(msgs.into_iter().find(|m| m.role == "user"))
}

impl AppServices {
    /// Validates a mutation request (authorization once, then preview).
    ///
    /// # Errors
    /// See [`preview_target`].
    pub async fn mutation_preview(
        &self,
        ctx: &SecurityContext,
        action: &str,
        chat_id: Uuid,
        request_id: Uuid,
    ) -> DomainResult<MutationTarget> {
        let chat = self.authorized_chat(ctx, action, chat_id).await?;
        let conn = self.db.conn()?;
        let turn = preview_target(&conn, &chat, request_id, ctx.subject_id()).await?;
        Ok(MutationTarget { chat, turn })
    }

    /// `DELETE /chats/{id}/turns/{request_id}`.
    ///
    /// # Errors
    /// Mutation validation errors.
    pub async fn delete_turn(
        &self,
        ctx: &SecurityContext,
        chat_id: Uuid,
        request_id: Uuid,
    ) -> DomainResult<()> {
        let started = std::time::Instant::now();
        let target = self
            .mutation_preview(ctx, actions::DELETE_TURN, chat_id, request_id)
            .await?;
        let outbox = std::sync::Arc::clone(&self.outbox);
        let actor = ctx.subject_id();
        let res = retry_contention(|| {
            let outbox = std::sync::Arc::clone(&outbox);
            let target = target.clone();
            self.db.transaction(move |tx| {
                Box::pin(async move {
                    let user_msg =
                        turn_user_message(tx, &target.chat, target.turn.request_id).await?;
                    soft_delete_target(tx, &target.chat, &target.turn, None).await?;
                    invalidate_summary_if_covers(tx, &target.chat, user_msg.as_ref()).await?;
                    let ev = MiniChatAuditEvent::Mutation(TurnMutationAuditEvent {
                        event_type: "turn_delete".to_owned(),
                        timestamp: now(),
                        tenant_id: target.chat.tenant_id,
                        actor_user_id: actor,
                        chat_id: target.chat.id,
                        original_request_id: None,
                        new_request_id: None,
                        request_id: Some(target.turn.request_id),
                    });
                    let mut wakes = Wakes::default();
                    wakes.push(outbox.audit(tx, &ev).await.map_err(internal_payload)?);
                    Ok(wakes)
                })
            })
        })
        .await;
        let result = if res.is_ok() { "ok" } else { "error" };
        self.metrics
            .inc("turn_mutation", &[("op", "delete"), ("result", result)]);
        self.metrics.record(
            "turn_mutation_latency_ms",
            started.elapsed().as_secs_f64() * 1000.0,
            &[("op", "delete")],
        );
        res?.fire();
        Ok(())
    }
}

/// Outbox payload-size failures outside chat deletion are internal errors.
#[must_use]
pub fn internal_payload(e: DomainError) -> DomainError {
    match e {
        DomainError::PayloadTooLarge(m) => DomainError::Internal(m),
        other => other,
    }
}
