//! Turn status and tail-only turn mutations (DESIGN §3.3 Turn Status, §3.9).

use std::sync::Arc;

use mini_chat_sdk::{AuditEvent, TurnMutationAuditEvent};
use sea_orm::sea_query::Expr;
use sea_orm::{ColumnTrait, Condition, EntityTrait, QueryFilter, QueryOrder, QuerySelect, Set};
use time::OffsetDateTime;
use toolkit_db::outbox::Wake;
use toolkit_db::secure::{DBRunner, DbTx, SecureDeleteExt, SecureEntityExt, SecureInsertExt, SecureUpdateExt};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use super::stream::{TurnStream, empty_content, insert_user_message, touch_chat};
use super::{MiniChatService, now};
use crate::domain::authz::{ChatScopes, actions};
use crate::domain::error::{DomainError, DomainResult, Res};
use crate::domain::quota::reserve_and_recheck;
use crate::infra::db::entities::{chat_turns, message_attachments, messages, thread_summaries};
use crate::infra::outbox::fire;

/// Public turn status.
#[derive(Debug, Clone)]
pub struct TurnStatus {
    pub request_id: Uuid,
    pub state: &'static str,
    pub error_code: Option<String>,
    pub assistant_message_id: Option<Uuid>,
    pub updated_at: OffsetDateTime,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MutationOp {
    Retry,
    Edit,
    Delete,
}

impl MutationOp {
    fn action(self) -> &'static str {
        match self {
            Self::Retry => actions::RETRY_TURN,
            Self::Edit => actions::EDIT_TURN,
            Self::Delete => actions::DELETE_TURN,
        }
    }

    fn event_type(self) -> &'static str {
        match self {
            Self::Retry => mini_chat_sdk::audit::event_types::TURN_RETRY,
            Self::Edit => mini_chat_sdk::audit::event_types::TURN_EDIT,
            Self::Delete => mini_chat_sdk::audit::event_types::TURN_DELETE,
        }
    }
}

fn not_latest() -> DomainError {
    DomainError::aborted(Res::Turn, "NOT_LATEST_TURN", "only the latest turn can be mutated")
}

fn api_state(state: &str) -> &'static str {
    match state {
        "completed" => "done",
        "failed" => "error",
        "cancelled" => "cancelled",
        _ => "running",
    }
}

pub(crate) async fn latest_turn(runner: &impl DBRunner, scopes: &ChatScopes, chat_id: Uuid) -> DomainResult<Option<chat_turns::Model>> {
    Ok(chat_turns::Entity::find()
        .filter(Condition::all().add(chat_turns::Column::ChatId.eq(chat_id)).add(chat_turns::Column::DeletedAt.is_null()))
        .order_by(chat_turns::Column::StartedAt, sea_orm::Order::Desc)
        .order_by(chat_turns::Column::Id, sea_orm::Order::Desc)
        .limit(1)
        .secure()
        .scope_with(&scopes.tenant)
        .one(runner)
        .await?)
}

/// Delete the summary when it covers the mutated turn's user message.
async fn invalidate_summary(tx: &DbTx<'_>, scopes: &ChatScopes, chat_id: Uuid, user_msg: Option<&messages::Model>) -> DomainResult<()> {
    let Some(summary) = thread_summaries::Entity::find()
        .filter(thread_summaries::Column::ChatId.eq(chat_id))
        .secure()
        .scope_with(&scopes.tenant)
        .one(tx)
        .await?
    else {
        return Ok(());
    };
    let covers = user_msg.is_none_or(|m| {
        (summary.summarized_up_to_created_at, summary.summarized_up_to_message_id) >= (m.created_at, m.id)
    });
    if covers {
        thread_summaries::Entity::delete_many()
            .filter(thread_summaries::Column::ChatId.eq(chat_id))
            .secure()
            .scope_with(&scopes.tenant)
            .exec(tx)
            .await?;
        messages::Entity::update_many()
            .col_expr(messages::Column::IsCompressed, Expr::value(false))
            .filter(messages::Column::ChatId.eq(chat_id))
            .secure()
            .scope_with(&scopes.tenant)
            .exec(tx)
            .await?;
    }
    Ok(())
}

/// Soft-delete a turn and its messages.
async fn soft_delete_turn(tx: &DbTx<'_>, scopes: &ChatScopes, turn: &chat_turns::Model, replaced_by: Option<Uuid>) -> DomainResult<()> {
    let ts = now();
    let res = chat_turns::Entity::update_many()
        .col_expr(chat_turns::Column::DeletedAt, Expr::value(ts))
        .col_expr(chat_turns::Column::ReplacedByRequestId, Expr::value(replaced_by))
        .col_expr(chat_turns::Column::UpdatedAt, Expr::value(ts))
        .filter(Condition::all().add(chat_turns::Column::Id.eq(turn.id)).add(chat_turns::Column::DeletedAt.is_null()))
        .secure()
        .scope_with(&scopes.tenant)
        .exec(tx)
        .await?;
    if res.rows_affected == 0 {
        return Err(not_latest());
    }
    messages::Entity::update_many()
        .col_expr(messages::Column::DeletedAt, Expr::value(ts))
        .filter(
            Condition::all()
                .add(messages::Column::ChatId.eq(turn.chat_id))
                .add(messages::Column::RequestId.eq(turn.request_id))
                .add(messages::Column::DeletedAt.is_null()),
        )
        .secure()
        .scope_with(&scopes.tenant)
        .exec(tx)
        .await?;
    Ok(())
}

impl MiniChatService {
    /// `GET /v1/chats/{id}/turns/{request_id}`.
    ///
    /// # Errors
    /// Authorization or not found.
    pub async fn turn_status(&self, ctx: &SecurityContext, chat_id: Uuid, request_id: Uuid) -> DomainResult<TurnStatus> {
        let scopes = self.scopes(ctx, actions::READ_TURN, Some(chat_id)).await?;
        let conn = self.db.conn()?;
        self.load_chat(&conn, &scopes, chat_id).await?;
        let turn = self
            .find_turn_any(&conn, &scopes, chat_id, request_id)
            .await?
            .filter(|t| t.deleted_at.is_none())
            .ok_or(DomainError::NotFound(Res::Turn))?;
        let state = api_state(&turn.state);
        Ok(TurnStatus {
            request_id: turn.request_id,
            state,
            error_code: if state == "error" { turn.error_code.clone() } else { None },
            assistant_message_id: if matches!(state, "done" | "cancelled") { turn.assistant_message_id } else { None },
            updated_at: turn.updated_at,
        })
    }

    /// Read-only mutation checks: exists, latest, terminal, owned.
    async fn mutation_preview(&self, ctx: &SecurityContext, scopes: &ChatScopes, chat_id: Uuid, request_id: Uuid) -> DomainResult<chat_turns::Model> {
        let conn = self.db.conn()?;
        let target = self.find_turn_any(&conn, scopes, chat_id, request_id).await?.ok_or(DomainError::NotFound(Res::Turn))?;
        if target.deleted_at.is_some() {
            return Err(not_latest());
        }
        if target.state == "running" {
            return Err(DomainError::precondition(Res::Turn, "turn_state", "STATE", "the turn is still running"));
        }
        if target.requester_user_id != Some(ctx.subject_id()) {
            return Err(DomainError::PermissionDenied);
        }
        let latest = latest_turn(&conn, scopes, chat_id).await?;
        if latest.as_ref().map(|t| t.id) != Some(target.id) {
            return Err(not_latest());
        }
        Ok(target)
    }

    async fn mutation_audit(
        &self,
        tx: &DbTx<'_>,
        op: MutationOp,
        ctx_ids: (Uuid, Uuid, Uuid),
        original: Uuid,
        new_request_id: Option<Uuid>,
    ) -> DomainResult<Wake> {
        let (tenant_id, actor, chat_id) = ctx_ids;
        let ev = AuditEvent::Mutation(TurnMutationAuditEvent {
            event_type: op.event_type().to_owned(),
            timestamp: OffsetDateTime::now_utc(),
            tenant_id,
            actor_user_id: actor,
            chat_id,
            original_request_id: (op != MutationOp::Delete).then_some(original),
            new_request_id,
            request_id: (op == MutationOp::Delete).then_some(original),
            trace_id: None,
        });
        self.outbox.audit(tx, &ev).await
    }

    /// `DELETE /v1/chats/{id}/turns/{request_id}`.
    ///
    /// # Errors
    /// Authorization, not found, state or latest-turn violations.
    pub async fn delete_turn(&self, ctx: &SecurityContext, chat_id: Uuid, request_id: Uuid) -> DomainResult<()> {
        let scopes = self.scopes(ctx, MutationOp::Delete.action(), Some(chat_id)).await?;
        {
            let conn = self.db.conn()?;
            self.load_chat(&conn, &scopes, chat_id).await?;
        }
        let target = self.mutation_preview(ctx, &scopes, chat_id, request_id).await?;
        let ids = (ctx.subject_tenant_id(), ctx.subject_id(), chat_id);
        let svc_outbox = self.outbox.clone();
        let wakes = self
            .tx(move |tx| {
                let scopes = scopes.clone();
                let target = target.clone();
                let outbox = svc_outbox.clone();
                Box::pin(async move {
                    let latest = latest_turn(tx, &scopes, chat_id).await?;
                    if latest.as_ref().map(|t| t.id) != Some(target.id) {
                        return Err(not_latest());
                    }
                    let user_msg = messages::Entity::find()
                        .filter(
                            Condition::all()
                                .add(messages::Column::ChatId.eq(chat_id))
                                .add(messages::Column::RequestId.eq(target.request_id))
                                .add(messages::Column::Role.eq("user")),
                        )
                        .secure()
                        .scope_with(&scopes.tenant)
                        .one(tx)
                        .await?;
                    soft_delete_turn(tx, &scopes, &target, None).await?;
                    invalidate_summary(tx, &scopes, chat_id, user_msg.as_ref()).await?;
                    let (tenant_id, actor, chat_id) = ids;
                    let ev = AuditEvent::Mutation(TurnMutationAuditEvent {
                        event_type: MutationOp::Delete.event_type().to_owned(),
                        timestamp: OffsetDateTime::now_utc(),
                        tenant_id,
                        actor_user_id: actor,
                        chat_id,
                        original_request_id: None,
                        new_request_id: None,
                        request_id: Some(target.request_id),
                        trace_id: None,
                    });
                    Ok(vec![outbox.audit(tx, &ev).await?])
                })
            })
            .await?;
        fire(wakes);
        Ok(())
    }

    /// `POST .../turns/{request_id}/retry` and `PATCH .../turns/{request_id}`.
    ///
    /// # Errors
    /// Pre-stream rejections (JSON `Problem`).
    #[allow(clippy::too_many_lines)]
    pub async fn mutate_turn(
        self: &Arc<Self>,
        ctx: &SecurityContext,
        chat_id: Uuid,
        request_id: Uuid,
        new_content: Option<String>,
    ) -> DomainResult<TurnStream> {
        let op = if new_content.is_some() { MutationOp::Edit } else { MutationOp::Retry };
        if new_content.as_deref().is_some_and(|c| c.trim().is_empty()) {
            return Err(empty_content());
        }
        let scopes = self.scopes(ctx, op.action(), Some(chat_id)).await?;
        let conn = self.db.conn()?;
        let chat = self.load_chat(&conn, &scopes, chat_id).await?;
        let target = self.mutation_preview(ctx, &scopes, chat_id, request_id).await?;
        let old_user = messages::Entity::find()
            .filter(
                Condition::all()
                    .add(messages::Column::ChatId.eq(chat_id))
                    .add(messages::Column::RequestId.eq(target.request_id))
                    .add(messages::Column::Role.eq("user"))
                    .add(messages::Column::DeletedAt.is_null()),
            )
            .secure()
            .scope_with(&scopes.tenant)
            .one(&conn)
            .await?
            .ok_or_else(|| DomainError::internal("turn without user message"))?;
        let content = new_content.unwrap_or_else(|| old_user.content.clone());
        let link_ids: Vec<Uuid> = message_attachments::Entity::find()
            .filter(
                Condition::all()
                    .add(message_attachments::Column::ChatId.eq(chat_id))
                    .add(message_attachments::Column::MessageId.eq(old_user.id)),
            )
            .order_by(message_attachments::Column::CreatedAt, sea_orm::Order::Asc)
            .secure()
            .scope_with(&scopes.tenant)
            .all(&conn)
            .await?
            .into_iter()
            .map(|l| l.attachment_id)
            .collect();
        let attached = self
            .load_message_attachments(&conn, &scopes, chat_id, ctx.subject_id(), &link_ids, false)
            .await?;
        let copied_ids: Vec<Uuid> = {
            // Copy every non-deleted linked attachment (images re-sent only when ready).
            let rows = crate::infra::db::entities::attachments::Entity::find()
                .filter(
                    Condition::all()
                        .add(crate::infra::db::entities::attachments::Column::ChatId.eq(chat_id))
                        .add(crate::infra::db::entities::attachments::Column::Id.is_in(link_ids.clone()))
                        .add(crate::infra::db::entities::attachments::Column::DeletedAt.is_null()),
                )
                .secure()
                .scope_with(&scopes.tenant)
                .all(&conn)
                .await?;
            link_ids.iter().filter(|id| rows.iter().any(|r| r.id == **id)).copied().collect()
        };
        let new_request_id = Uuid::new_v4();
        // Preflight before the mutation: a rejection leaves the previous turn intact.
        let plan = self
            .plan_turn(ctx, &scopes, &chat, &content, &attached, target.web_search_enabled, Some(target.request_id), new_request_id)
            .await?;

        // Mutation commit.
        let turn_id = Uuid::new_v4();
        let tenant_id = ctx.subject_tenant_id();
        let user_id = ctx.subject_id();
        let ids = (tenant_id, user_id, chat_id);
        let svc = Arc::clone(self);
        let commit_scopes = scopes.clone();
        let commit_target = target.clone();
        let commit_content = content.clone();
        let res = self
            .tx(move |tx| {
                let scopes = commit_scopes.clone();
                let target = commit_target.clone();
                let content = commit_content.clone();
                let copied_ids = copied_ids.clone();
                let svc = Arc::clone(&svc);
                let old_user = old_user.clone();
                Box::pin(async move {
                    let latest = latest_turn(tx, &scopes, chat_id).await?;
                    if latest.as_ref().map(|t| t.id) != Some(target.id) {
                        return Err(not_latest());
                    }
                    if latest.as_ref().is_some_and(|t| t.state == "running") {
                        return Err(DomainError::precondition(Res::Turn, "turn_state", "STATE", "the turn is still running"));
                    }
                    soft_delete_turn(tx, &scopes, &target, Some(new_request_id)).await?;
                    let ts = now();
                    insert_user_message(tx, &scopes, tenant_id, chat_id, Uuid::new_v4(), new_request_id, &content, ts, &copied_ids).await?;
                    let am = chat_turns::ActiveModel {
                        id: Set(turn_id),
                        tenant_id: Set(tenant_id),
                        chat_id: Set(chat_id),
                        request_id: Set(new_request_id),
                        requester_type: Set("user".into()),
                        requester_user_id: Set(Some(user_id)),
                        state: Set("running".into()),
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
                        started_at: Set(ts),
                        last_progress_at: Set(Some(ts)),
                        web_search_enabled: Set(target.web_search_enabled),
                        web_search_completed_count: Set(0),
                        code_interpreter_completed_count: Set(0),
                        file_search_completed_count: Set(0),
                        completed_at: Set(None),
                        updated_at: Set(ts),
                    };
                    chat_turns::Entity::insert(am)
                        .secure()
                        .scope_unchecked(&scopes.tenant)?
                        .exec(tx)
                        .await
                        .map_err(|e| {
                            if e.is_unique_violation() {
                                DomainError::aborted(Res::Turn, "GENERATION_IN_PROGRESS", "another generation started concurrently")
                            } else {
                                e.into()
                            }
                        })?;
                    invalidate_summary(tx, &scopes, chat_id, Some(&old_user)).await?;
                    touch_chat(tx, &scopes, chat_id).await?;
                    let wake = svc.mutation_audit(tx, op, ids, target.request_id, Some(new_request_id)).await?;
                    Ok(vec![wake])
                })
            })
            .await;
        let wakes = match res {
            Ok(w) => w,
            Err(e) if e.is_unique_violation() => {
                return Err(DomainError::aborted(Res::Turn, "GENERATION_IN_PROGRESS", "another generation started concurrently"));
            }
            Err(e) => return Err(e),
        };
        fire(wakes);

        // Reserve (last setup step); failures mark the new turn failed.
        let reserve = plan.cascade.reserve;
        let tier = plan.cascade.effective.tier;
        let limits = plan.limits.clone();
        let started_at = plan.started_at;
        let effective_model = plan.cascade.effective.id.clone();
        let policy_version = plan.snapshot.policy_version;
        let floor = plan.floor_applied;
        let rscopes = scopes.clone();
        let res = self
            .tx(move |tx| {
                let limits = limits.clone();
                let effective_model = effective_model.clone();
                let scopes = rscopes.clone();
                Box::pin(async move {
                    reserve_and_recheck(tx, tenant_id, user_id, started_at, tier, reserve.reserved_credits_micro, &limits).await?;
                    chat_turns::Entity::update_many()
                        .col_expr(chat_turns::Column::ReserveTokens, Expr::value(reserve.reserve_tokens))
                        .col_expr(
                            chat_turns::Column::MaxOutputTokensApplied,
                            Expr::value(i32::try_from(reserve.max_output_tokens_applied).unwrap_or(i32::MAX)),
                        )
                        .col_expr(chat_turns::Column::ReservedCreditsMicro, Expr::value(reserve.reserved_credits_micro))
                        .col_expr(chat_turns::Column::PolicyVersionApplied, Expr::value(i64::try_from(policy_version).unwrap_or(i64::MAX)))
                        .col_expr(chat_turns::Column::EffectiveModel, Expr::value(effective_model))
                        .col_expr(chat_turns::Column::MinimalGenerationFloorApplied, Expr::value(i32::try_from(floor).unwrap_or(i32::MAX)))
                        .filter(Condition::all().add(chat_turns::Column::Id.eq(turn_id)).add(chat_turns::Column::State.eq("running")))
                        .secure()
                        .scope_with(&scopes.tenant)
                        .exec(tx)
                        .await?;
                    Ok(())
                })
            })
            .await;
        if let Err(e) = res {
            let code = match &e {
                DomainError::QuotaExceeded { .. } => "quota_exceeded",
                DomainError::OutOfRange { reason, .. } if reason == "CONTEXT_BUDGET_EXCEEDED" => "context_length_exceeded",
                _ => "turn_setup_failed",
            };
            self.fail_unstarted_turn(tenant_id, turn_id, code).await;
            return Err(e);
        }
        Ok(self.spawn_turn(Self::live_turn(ctx, &chat, turn_id, new_request_id, plan)))
    }
}
