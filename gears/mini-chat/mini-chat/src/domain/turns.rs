//! Turn status and turn mutations: retry / edit / delete of the latest terminal turn
//! (DESIGN §3.3 "Turn Status API", §3.9 "Turn Mutation Rules", §3.6 retry/edit variant).

use std::sync::Arc;
use std::time::Instant;

use mini_chat_sdk::{MiniChatAuditEvent, TurnMutationAuditEvent};
use sea_orm::ActiveValue::Set;
use sea_orm::sea_query::Expr;
use sea_orm::{ColumnTrait, Condition, EntityTrait, QueryFilter};
use toolkit_db::DbTx;
use toolkit_db::outbox::Wake;
use toolkit_db::secure::{SecureUpdateExt, secure_insert};
use toolkit_security::{AccessScope, SecurityContext};
use uuid::Uuid;

use crate::clock;
use crate::domain::chats;
use crate::domain::error::{DomainError, Resource};
use crate::domain::quota;
use crate::domain::services::AppServices;
use crate::domain::stream::finalize;
use crate::domain::stream::queries::{self, STATE_CANCELLED, STATE_COMPLETED, STATE_FAILED, STATE_RUNNING};
use crate::domain::stream::setup::{self, LiveTurn, StartOutcome};
use crate::domain::summary;
use crate::infra::db::entities::{chat, chat_turn, message};
use crate::infra::outbox::PAYLOAD_AUDIT;

/// Turn status as exposed by the API.
#[derive(Debug, Clone)]
pub struct TurnStatus {
    pub request_id: Uuid,
    /// `running` | `done` | `error` | `cancelled`
    pub state: &'static str,
    pub error_code: Option<String>,
    pub assistant_message_id: Option<Uuid>,
    pub updated_at: time::OffsetDateTime,
}

fn turn_not_found(request_id: Uuid) -> DomainError {
    DomainError::not_found(Resource::Turn, request_id)
}

/// `GET /v1/chats/{id}/turns/{request_id}`.
///
/// # Errors
/// 404 (chat / turn), authz errors.
pub async fn get_status(app: &AppServices, ctx: &SecurityContext, chat_id: Uuid, request_id: Uuid) -> Result<TurnStatus, DomainError> {
    let scope = app.authz.chat_scope(ctx, "read_turn", Some(chat_id)).await?;
    let chat = chats::load_chat(app, &scope, chat_id).await?;
    let conn = app.db.conn()?;
    let turn = queries::find_turn(&conn, chat.tenant_id, chat_id, request_id)
        .await?
        .filter(|t| t.deleted_at.is_none())
        .ok_or_else(|| turn_not_found(request_id))?;
    let state = match turn.state.as_str() {
        STATE_RUNNING => "running",
        STATE_COMPLETED => "done",
        STATE_CANCELLED => "cancelled",
        _ => "error",
    };
    Ok(TurnStatus {
        request_id: turn.request_id,
        state,
        error_code: if state == "error" { turn.error_code.clone() } else { None },
        assistant_message_id: if matches!(state, "done" | "cancelled") { turn.assistant_message_id } else { None },
        updated_at: turn.updated_at,
    })
}

/// Mutation kind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mutation {
    Retry,
    Edit { content: String },
    Delete,
}

impl Mutation {
    const fn action(&self) -> &'static str {
        match self {
            Self::Retry => "retry_turn",
            Self::Edit { .. } => "edit_turn",
            Self::Delete => "delete_turn",
        }
    }

    const fn audit_type(&self) -> &'static str {
        match self {
            Self::Retry => "turn_retry",
            Self::Edit { .. } => "turn_edit",
            Self::Delete => "turn_delete",
        }
    }
}

fn not_latest() -> DomainError {
    DomainError::aborted(Resource::Turn, "NOT_LATEST_TURN", "Only the latest turn can be modified")
}

fn generation_in_progress() -> DomainError {
    DomainError::aborted(Resource::Turn, "GENERATION_IN_PROGRESS", "Another generation is in progress for this chat")
}

fn not_terminal() -> DomainError {
    DomainError::precondition(Resource::Turn, "turn_state", "STATE", "The turn is still running")
}

/// Read-only mutation preview (latest, terminal, ownership).
async fn preview(
    app: &AppServices,
    ctx: &SecurityContext,
    chat_id: Uuid,
    request_id: Uuid,
    m: &Mutation,
) -> Result<(chat::Model, chat_turn::Model), DomainError> {
    let scope = app.authz.chat_scope(ctx, m.action(), Some(chat_id)).await?;
    let chat = chats::load_chat(app, &scope, chat_id).await?;
    let conn = app.db.conn()?;
    let turn = queries::find_turn(&conn, chat.tenant_id, chat_id, request_id)
        .await?
        .ok_or_else(|| turn_not_found(request_id))?;
    check_target(&conn, ctx, &chat, &turn).await?;
    Ok((chat, turn))
}

async fn check_target(
    db: &impl toolkit_db::secure::DBRunner,
    ctx: &SecurityContext,
    chat: &chat::Model,
    turn: &chat_turn::Model,
) -> Result<(), DomainError> {
    if turn.deleted_at.is_some() {
        return Err(not_latest());
    }
    if turn.requester_user_id != Some(ctx.subject_id()) {
        return Err(DomainError::permission_denied());
    }
    if turn.state == STATE_RUNNING {
        return Err(not_terminal());
    }
    let latest = queries::latest_turn(db, chat.tenant_id, chat.id).await?;
    if latest.map(|t| t.id) != Some(turn.id) {
        return Err(not_latest());
    }
    Ok(())
}

/// Soft-deletes the old turn and its messages; invalidates a covering summary. Returns the old
/// user message.
async fn soft_delete_turn(
    tx: &DbTx<'_>,
    turn: &chat_turn::Model,
    replaced_by: Option<Uuid>,
    now: time::OffsetDateTime,
) -> Result<Option<message::Model>, DomainError> {
    let scope = AccessScope::for_tenant(turn.tenant_id);
    let mut upd = chat_turn::Entity::update_many()
        .col_expr(chat_turn::Column::DeletedAt, Expr::value(now))
        .col_expr(chat_turn::Column::UpdatedAt, Expr::value(now));
    if let Some(r) = replaced_by {
        upd = upd.col_expr(chat_turn::Column::ReplacedByRequestId, Expr::value(r));
    }
    let rows = upd
        .filter(
            Condition::all()
                .add(chat_turn::Column::Id.eq(turn.id))
                .add(chat_turn::Column::DeletedAt.is_null())
                .add(chat_turn::Column::State.ne(STATE_RUNNING)),
        )
        .secure()
        .scope_with(&scope)
        .exec(tx)
        .await?
        .rows_affected;
    if rows != 1 {
        return Err(not_latest());
    }
    let user_msg = queries::turn_user_message(tx, turn.tenant_id, turn.chat_id, turn.request_id).await?;
    message::Entity::update_many()
        .col_expr(message::Column::DeletedAt, Expr::value(now))
        .filter(
            Condition::all()
                .add(message::Column::ChatId.eq(turn.chat_id))
                .add(message::Column::RequestId.eq(turn.request_id))
                .add(message::Column::DeletedAt.is_null()),
        )
        .secure()
        .scope_with(&scope)
        .exec(tx)
        .await?;
    if let Some(m) = &user_msg {
        summary::invalidate_for_mutation(tx, turn.tenant_id, turn.chat_id, m.created_at, m.id).await?;
    }
    Ok(user_msg)
}

async fn enqueue_mutation_audit(
    app: &AppServices,
    tx: &DbTx<'_>,
    ev: TurnMutationAuditEvent,
) -> Result<Wake, DomainError> {
    let tenant = ev.tenant_id;
    app.outbox
        .enqueue_json(tx, app.outbox.audit_queue(), tenant, PAYLOAD_AUDIT, &MiniChatAuditEvent::TurnMutation(ev))
        .await
}

/// Map of mutation-commit errors: unique violations on the running-turn index → GENERATION_IN_PROGRESS;
/// payload too large → 500 on mutations.
fn map_commit_err(e: DomainError) -> DomainError {
    if e.is_unique_violation() {
        return generation_in_progress();
    }
    if let DomainError::InvalidFormat(m) = e {
        return DomainError::internal(m);
    }
    e
}

/// `DELETE /v1/chats/{id}/turns/{request_id}`.
///
/// # Errors
/// Mutation errors.
pub async fn delete_turn(app: &Arc<AppServices>, ctx: &SecurityContext, chat_id: Uuid, request_id: Uuid) -> Result<(), DomainError> {
    let (chat, turn) = preview(app, ctx, chat_id, request_id, &Mutation::Delete).await?;
    let actor = ctx.subject_id();
    let wake = crate::domain::tx::retry_contention(|| {
        let app2 = Arc::clone(app);
        let ctx2 = ctx.clone();
        let chat = chat.clone();
        let turn = turn.clone();
        app.db.transaction(move |tx| {
            Box::pin(async move {
                let now = clock::now();
                let fresh = queries::turn_by_id(tx, chat.tenant_id, turn.id).await?.ok_or_else(not_latest)?;
                check_target(tx, &ctx2, &chat, &fresh).await?;
                soft_delete_turn(tx, &fresh, None, now).await?;
                enqueue_mutation_audit(
                    &app2,
                    tx,
                    TurnMutationAuditEvent {
                        event_type: Mutation::Delete.audit_type().to_owned(),
                        tenant_id: chat.tenant_id,
                        actor_user_id: actor,
                        chat_id: chat.id,
                        original_request_id: None,
                        new_request_id: None,
                        request_id: Some(fresh.request_id),
                        timestamp: now,
                    },
                )
                .await
            })
        })
    })
    .await
    .map_err(map_commit_err)?;
    wake.fire();
    crate::infra::metrics::incr("mini_chat_turn_mutation", 1, &[("op", "delete".to_owned()), ("result", "ok".to_owned())]);
    Ok(())
}

/// Retry or edit: preview, preflight, mutation commit, then context + reserve; streams like send.
///
/// # Errors
/// Mutation / preflight / setup errors (setup failures after the commit mark the new turn failed).
#[allow(clippy::too_many_lines)]
pub async fn start_mutation(
    app: &Arc<AppServices>,
    ctx: &SecurityContext,
    chat_id: Uuid,
    request_id: Uuid,
    m: Mutation,
) -> Result<StartOutcome, DomainError> {
    let started = Instant::now();
    if let Mutation::Edit { content } = &m {
        setup::validate_content(content)?;
    }
    let (chat, turn) = preview(app, ctx, chat_id, request_id, &m).await?;
    let tenant = chat.tenant_id;
    let user = ctx.subject_id();

    // Original user message + its (non-deleted) attachments.
    let conn = app.db.conn()?;
    let orig = queries::turn_user_message(&conn, tenant, chat_id, turn.request_id)
        .await?
        .ok_or_else(|| DomainError::internal(format!("turn {} has no user message", turn.id)))?;
    let orig_attachments = queries::message_attachments(&conn, tenant, chat_id, orig.id).await?;
    drop(conn);
    let content = match &m {
        Mutation::Edit { content } => content.clone(),
        _ => orig.content.clone(),
    };
    let images: Vec<_> = orig_attachments.iter().filter(|a| a.attachment_kind == "image").cloned().collect();
    let web_search = turn.web_search_enabled;

    // Preflight before the mutation commits: a rejection leaves the previous answer in place.
    let pf = setup::run_preflight(app, ctx, &chat, &content, &images, web_search).await?;

    // Mutation commit.
    let new_request_id = Uuid::new_v4();
    let new_turn_id = Uuid::new_v4();
    let new_user_msg_id = Uuid::new_v4();
    let link_ids: Vec<Uuid> = orig_attachments.iter().map(|a| a.id).collect();
    let wake = crate::domain::tx::retry_contention(|| {
        let app2 = Arc::clone(app);
        let ctx2 = ctx.clone();
        let chat2 = chat.clone();
        let turn2 = turn.clone();
        let content2 = content.clone();
        let link_ids = link_ids.clone();
        let audit_type = m.audit_type().to_owned();
        app.db.transaction(move |tx| {
            Box::pin(async move {
                let now = clock::now();
                let scope = AccessScope::for_tenant(tenant);
                let fresh = queries::turn_by_id(tx, tenant, turn2.id).await?.ok_or_else(not_latest)?;
                check_target(tx, &ctx2, &chat2, &fresh).await?;
                soft_delete_turn(tx, &fresh, Some(new_request_id), now).await?;
                let msg = message::ActiveModel {
                    id: Set(new_user_msg_id),
                    tenant_id: Set(tenant),
                    chat_id: Set(chat2.id),
                    request_id: Set(Some(new_request_id)),
                    role: Set("user".to_owned()),
                    content: Set(content2),
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
                    created_at: Set(now),
                    deleted_at: Set(None),
                };
                secure_insert::<message::Entity>(msg, &scope, tx).await?;
                queries::link_attachments(tx, tenant, chat2.id, new_user_msg_id, &link_ids, now).await?;
                let turn_am = setup::new_turn_model(new_turn_id, tenant, chat2.id, new_request_id, user, web_search, now, None);
                secure_insert::<chat_turn::Entity>(turn_am, &scope, tx).await?;
                chats::touch_chat(tx, tenant, chat2.id, now).await?;
                enqueue_mutation_audit(
                    &app2,
                    tx,
                    TurnMutationAuditEvent {
                        event_type: audit_type,
                        tenant_id: tenant,
                        actor_user_id: user,
                        chat_id: chat2.id,
                        original_request_id: Some(fresh.request_id),
                        new_request_id: Some(new_request_id),
                        request_id: None,
                        timestamp: now,
                    },
                )
                .await
            })
        })
    })
    .await
    .map_err(map_commit_err)?;
    wake.fire();

    // After the commit: context, provider, reserve. Failures mark the new turn failed.
    let plan = match setup::build_provider_plan(app, ctx, &chat, &pf, &content, None, Some(new_user_msg_id)).await {
        Ok(p) => p,
        Err(e) => {
            let code = match &e {
                DomainError::OutOfRange { reason, .. } if reason == "CONTEXT_BUDGET_EXCEEDED" => "context_length_exceeded",
                _ => "turn_setup_failed",
            };
            let _ = finalize::fail_unstarted_turn(app, tenant, new_turn_id, code).await;
            return Err(e);
        }
    };
    let reserve = crate::domain::tx::retry_contention(|| {
        let decision = pf.decision.clone();
        app.db.transaction(move |tx| {
            Box::pin(async move {
                quota::reserve(tx, tenant, user, &decision).await?;
                let rows = chat_turn::Entity::update_many()
                    .col_expr(chat_turn::Column::ReserveTokens, Expr::value(decision.reserve_tokens))
                    .col_expr(
                        chat_turn::Column::MaxOutputTokensApplied,
                        Expr::value(i32::try_from(decision.max_output_tokens_applied).unwrap_or(i32::MAX)),
                    )
                    .col_expr(chat_turn::Column::ReservedCreditsMicro, Expr::value(decision.reserved_credits_micro))
                    .col_expr(chat_turn::Column::PolicyVersionApplied, Expr::value(decision.policy_version))
                    .col_expr(chat_turn::Column::EffectiveModel, Expr::value(decision.effective_model.id.clone()))
                    .col_expr(
                        chat_turn::Column::MinimalGenerationFloorApplied,
                        Expr::value(i32::try_from(decision.minimal_generation_floor_applied).unwrap_or(i32::MAX)),
                    )
                    .col_expr(chat_turn::Column::UpdatedAt, Expr::value(clock::now()))
                    .filter(
                        Condition::all()
                            .add(chat_turn::Column::Id.eq(new_turn_id))
                            .add(chat_turn::Column::State.eq(STATE_RUNNING))
                            .add(chat_turn::Column::ReserveTokens.is_null()),
                    )
                    .secure()
                    .scope_with(&AccessScope::for_tenant(tenant))
                    .exec(tx)
                    .await?
                    .rows_affected;
                if rows != 1 {
                    return Err(DomainError::internal("retry/edit turn is no longer running"));
                }
                Ok(())
            })
        })
    })
    .await;
    if let Err(e) = reserve {
        let code = if matches!(e, DomainError::ResourceExhausted { .. }) { "quota_exceeded" } else { "turn_setup_failed" };
        let _ = finalize::fail_unstarted_turn(app, tenant, new_turn_id, code).await;
        return Err(e);
    }
    let op = if matches!(m, Mutation::Retry) { "retry" } else { "edit" };
    crate::infra::metrics::incr("mini_chat_turn_mutation", 1, &[("op", op.to_owned()), ("result", "ok".to_owned())]);

    Ok(StartOutcome::Live(Box::new(LiveTurn {
        ctx: ctx.clone(),
        chat,
        turn_id: new_turn_id,
        request_id: new_request_id,
        message_id: Uuid::new_v4(),
        summary_applied: plan.plan.summary_applied,
        summary_trigger: setup::summary_trigger(&plan),
        decision: pf.decision,
        provider: plan.provider,
        request_body: plan.body,
        file_map: pf.attach.file_map,
        started,
    })))
}

/// `true` for terminal turn states.
#[must_use]
pub fn is_terminal(state: &str) -> bool {
    matches!(state, STATE_COMPLETED | STATE_FAILED | STATE_CANCELLED)
}

#[cfg(test)]
#[path = "turns_tests.rs"]
mod tests;
