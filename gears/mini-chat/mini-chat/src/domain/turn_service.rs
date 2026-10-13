//! Turn status and tail-only mutations (DESIGN §3.9).

use std::time::Instant;

use mini_chat_sdk::{MiniChatAuditEvent, TurnDeleteAuditEvent, TurnEditAuditEvent, TurnRetryAuditEvent};
use sea_orm::ActiveValue::Set;
use sea_orm::sea_query::Expr;
use sea_orm::{ColumnTrait, Condition, EntityTrait, QueryFilter};
use time::OffsetDateTime;
use toolkit_db::secure::{DbTx, SecureDeleteExt, SecureUpdateExt, secure_insert};
use toolkit_security::{AccessScope, SecurityContext};
use uuid::Uuid;

use crate::infra::db::WriteTransaction as _;
use crate::domain::authz::actions;
use crate::domain::error::{DomainError, QuotaScope};
use crate::domain::finalization::TurnCtx;
use crate::domain::repo::{self, state};
use crate::domain::service::{Svc, child_scope};
use crate::domain::stream_service::{StreamStart, insert_links, insert_user_message, touch_chat};
use crate::domain::quota;
use crate::infra::db::entities::{attachments, chat_turns, chats, messages, thread_summaries};
use crate::infra::db::now;

/// Mutation kind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mutation {
    /// Retry with the original content.
    Retry,
    /// Edit with new content.
    Edit(String),
}

/// Turn status view.
#[derive(Debug, Clone)]
pub struct TurnStatus {
    /// Request id.
    pub request_id: Uuid,
    /// API state.
    pub state: &'static str,
    /// Error code.
    pub error_code: Option<String>,
    /// Assistant message id.
    pub assistant_message_id: Option<Uuid>,
    /// Update time.
    pub updated_at: OffsetDateTime,
}

/// Soft-deletes the messages of a request.
async fn soft_delete_request_messages(tx: &DbTx<'_>, scope: &AccessScope, chat_id: Uuid, request_id: Uuid, ts: OffsetDateTime) -> Result<(), DomainError> {
    messages::Entity::update_many()
        .secure()
        .col_expr(messages::Column::DeletedAt, Expr::value(Some(ts)))
        .filter(
            Condition::all()
                .add(messages::Column::ChatId.eq(chat_id))
                .add(messages::Column::RequestId.eq(request_id))
                .add(messages::Column::DeletedAt.is_null()),
        )
        .scope_with(scope)
        .exec(tx)
        .await?;
    Ok(())
}

/// Deletes the summary when it covers the mutated turn (DESIGN "Summary Interaction on Turn Mutation").
async fn invalidate_summary(tx: &DbTx<'_>, scope: &AccessScope, chat_id: Uuid, user_msg: Option<&messages::Model>) -> Result<(), DomainError> {
    let Some(summary) = repo::thread_summary(tx, scope, chat_id).await? else { return Ok(()) };
    let covers = user_msg.is_none_or(|m| {
        (summary.summarized_up_to_created_at, summary.summarized_up_to_message_id) >= (m.created_at, m.id)
    });
    if !covers {
        return Ok(());
    }
    thread_summaries::Entity::delete_many()
        .filter(Condition::all().add(thread_summaries::Column::ChatId.eq(chat_id)))
        .secure()
        .scope_with(scope)
        .exec(tx)
        .await?;
    messages::Entity::update_many()
        .secure()
        .col_expr(messages::Column::IsCompressed, Expr::value(false))
        .filter(Condition::all().add(messages::Column::ChatId.eq(chat_id)).add(messages::Column::IsCompressed.eq(true)))
        .scope_with(scope)
        .exec(tx)
        .await?;
    Ok(())
}

/// Marks an unstarted retry/edit turn failed.
async fn fail_unstarted(svc: &Svc, scope: &AccessScope, turn_id: Uuid, code: &str) {
    let Ok(conn) = svc.db.conn() else { return };
    let ts = now();
    if let Err(e) = chat_turns::Entity::update_many()
        .secure()
        .col_expr(chat_turns::Column::State, Expr::value(state::FAILED))
        .col_expr(chat_turns::Column::ErrorCode, Expr::value(Some(code.to_owned())))
        .col_expr(chat_turns::Column::CompletedAt, Expr::value(Some(ts)))
        .col_expr(chat_turns::Column::UpdatedAt, Expr::value(ts))
        .filter(Condition::all().add(chat_turns::Column::Id.eq(turn_id)).add(chat_turns::Column::State.eq(state::RUNNING)))
        .scope_with(scope)
        .exec(&conn)
        .await
    {
        tracing::warn!(turn_id = %turn_id, error = %e, "marking unstarted turn failed");
    }
}

impl Svc {
    /// `GET /chats/{id}/turns/{request_id}`.
    ///
    /// # Errors
    /// 404 / PDP.
    pub async fn turn_status(&self, ctx: &SecurityContext, chat_id: Uuid, request_id: Uuid) -> Result<TurnStatus, DomainError> {
        let (_, chat) = self.authorized_chat(ctx, actions::READ_TURN, chat_id).await?;
        let conn = self.db.conn()?;
        let turn = repo::turn_by_request(&conn, &child_scope(&chat), chat_id, request_id)
            .await?
            .filter(|t| t.deleted_at.is_none())
            .ok_or(DomainError::TurnNotFound)?;
        let api_state = match turn.state.as_str() {
            state::COMPLETED => "done",
            state::FAILED => "error",
            state::CANCELLED => "cancelled",
            _ => "running",
        };
        Ok(TurnStatus {
            request_id: turn.request_id,
            state: api_state,
            error_code: if api_state == "error" { turn.error_code.clone() } else { None },
            assistant_message_id: if matches!(api_state, "done" | "cancelled") { turn.assistant_message_id } else { None },
            updated_at: turn.updated_at,
        })
    }

    /// Read-only mutation preview: target must be the latest terminal turn of the caller.
    async fn mutation_preview(&self, ctx: &SecurityContext, action: &str, chat_id: Uuid, request_id: Uuid) -> Result<(chats::Model, chat_turns::Model), DomainError> {
        let (_, chat) = self.authorized_chat(ctx, action, chat_id).await?;
        let scope = child_scope(&chat);
        let conn = self.db.conn()?;
        let target = repo::turn_by_request(&conn, &scope, chat_id, request_id).await?.ok_or(DomainError::TurnNotFound)?;
        if target.deleted_at.is_some() {
            return Err(DomainError::NotLatestTurn);
        }
        if target.requester_user_id != Some(ctx.subject_id()) {
            return Err(DomainError::PermissionDenied);
        }
        if target.state == state::RUNNING {
            return Err(DomainError::TurnNotTerminal);
        }
        let latest = repo::latest_turn(&conn, &scope, chat_id).await?;
        if latest.as_ref().map(|t| t.id) != Some(target.id) {
            return Err(DomainError::NotLatestTurn);
        }
        Ok((chat, target))
    }

    /// `DELETE /chats/{id}/turns/{request_id}`.
    ///
    /// # Errors
    /// 400/403/404/409.
    pub async fn delete_turn(&self, ctx: &SecurityContext, chat_id: Uuid, request_id: Uuid) -> Result<(), DomainError> {
        let started = Instant::now();
        let (chat, target) = self.mutation_preview(ctx, actions::DELETE_TURN, chat_id, request_id).await?;
        let scope = child_scope(&chat);
        let outbox = self.outbox.clone();
        let actor = ctx.subject_id();
        let res = self
            .db
            .write_transaction(move |tx| {
                Box::pin(async move {
                    let ts = now();
                    let user_msg = repo::messages_of_request(tx, &scope, chat_id, request_id).await?.into_iter().find(|m| m.role == "user");
                    let r = chat_turns::Entity::update_many()
                        .secure()
                        .col_expr(chat_turns::Column::DeletedAt, Expr::value(Some(ts)))
                        .col_expr(chat_turns::Column::UpdatedAt, Expr::value(ts))
                        .filter(
                            Condition::all()
                                .add(chat_turns::Column::Id.eq(target.id))
                                .add(chat_turns::Column::DeletedAt.is_null())
                                .add(chat_turns::Column::State.ne(state::RUNNING)),
                        )
                        .scope_with(&scope)
                        .exec(tx)
                        .await?;
                    if r.rows_affected != 1 {
                        return Err(DomainError::NotLatestTurn);
                    }
                    soft_delete_request_messages(tx, &scope, chat_id, request_id, ts).await?;
                    invalidate_summary(tx, &scope, chat_id, user_msg.as_ref()).await?;
                    let ev = MiniChatAuditEvent::TurnDelete(TurnDeleteAuditEvent {
                        tenant_id: chat.tenant_id,
                        actor_user_id: actor,
                        chat_id,
                        request_id,
                        timestamp: OffsetDateTime::now_utc(),
                    });
                    outbox.audit(tx, chat.tenant_id, &ev).await.map_err(|e| match e {
                        DomainError::CleanupPayloadTooLarge(m) => DomainError::internal(m),
                        other => other,
                    })
                })
            })
            .await;
        let result = if res.is_ok() { "ok" } else { "error" };
        self.metrics.inc("turn_mutation_total", &[("op", "delete"), ("result", result)]);
        #[allow(clippy::cast_precision_loss)]
        self.metrics.record("turn_mutation_latency_ms", started.elapsed().as_millis() as f64, &[("op", "delete")]);
        res?.fire();
        Ok(())
    }

    /// Retry / edit: preview, preflight, mutation commit, setup, reserve.
    ///
    /// # Errors
    /// Every rejection as a JSON `Problem`.
    #[allow(clippy::too_many_lines)]
    pub async fn mutate_turn(&self, ctx: &SecurityContext, chat_id: Uuid, request_id: Uuid, mutation: Mutation) -> Result<StreamStart, DomainError> {
        let started = Instant::now();
        let op = if mutation == Mutation::Retry { "retry" } else { "edit" };
        let res = self.mutate_turn_inner(ctx, chat_id, request_id, mutation).await;
        self.metrics.inc("turn_mutation_total", &[("op", op), ("result", if res.is_ok() { "ok" } else { "error" })]);
        #[allow(clippy::cast_precision_loss)]
        self.metrics.record("turn_mutation_latency_ms", started.elapsed().as_millis() as f64, &[("op", op)]);
        res
    }

    #[allow(
        clippy::too_many_lines,
        clippy::cognitive_complexity,
        reason = "retry/edit orchestration: preview, preflight, mutation transaction and stream start"
    )]
    async fn mutate_turn_inner(&self, ctx: &SecurityContext, chat_id: Uuid, request_id: Uuid, mutation: Mutation) -> Result<StreamStart, DomainError> {
        if let Mutation::Edit(c) = &mutation
            && c.trim().is_empty()
        {
            return Err(DomainError::EmptyContent);
        }
        let action = if mutation == Mutation::Retry { actions::RETRY_TURN } else { actions::EDIT_TURN };
        let (chat, target) = self.mutation_preview(ctx, action, chat_id, request_id).await?;
        let scope = child_scope(&chat);
        let (orig_msg, linked) = {
            let conn = self.db.conn()?;
            let orig = repo::messages_of_request(&conn, &scope, chat_id, request_id).await?.into_iter().find(|m| m.role == "user");
            let links = match &orig {
                Some(m) => repo::links_of_messages(&conn, &scope, &[m.id]).await?,
                None => Vec::new(),
            };
            let ids: Vec<Uuid> = links.iter().map(|l| l.attachment_id).collect();
            let linked = repo::attachments_by_ids(&conn, &scope, chat_id, &ids).await?;
            (orig, linked)
        };
        let content = match &mutation {
            Mutation::Retry => orig_msg.as_ref().map(|m| m.content.clone()).unwrap_or_default(),
            Mutation::Edit(c) => c.clone(),
        };
        let images: Vec<attachments::Model> =
            linked.iter().filter(|a| a.attachment_kind == "image" && a.status == "ready").cloned().collect();
        let files = self.chat_files(&scope, chat_id).await?;
        let prior = {
            let conn = self.db.conn()?;
            repo::prior_context_tokens(&conn, &scope, chat_id).await?
        };
        let a = self.preflight_a(ctx, &chat, &content, images.len(), target.web_search_enabled, &files, prior).await?;

        // Mutation commit.
        let new_request = Uuid::new_v4();
        let new_turn = Uuid::new_v4();
        let user_message_id = Uuid::new_v4();
        let ts = now();
        let link_ids: Vec<Uuid> = linked.iter().map(|a| a.id).collect();
        let outbox = self.outbox.clone();
        let tx_scope = scope.clone();
        let actor = ctx.subject_id();
        let tenant_id = chat.tenant_id;
        let target_id = target.id;
        let web_search = target.web_search_enabled;
        let content_tx = content.clone();
        let is_retry = mutation == Mutation::Retry;
        let res = self
            .db
            .write_transaction(move |tx| {
                Box::pin(async move {
                    let current = chat_turns::Entity::find_by_id(target_id);
                    let current = toolkit_db::secure::SecureEntityExt::secure(current).scope_with(&tx_scope).one(tx).await?;
                    if current.as_ref().is_none_or(|t| t.deleted_at.is_some()) {
                        if repo::has_running_turn(tx, &tx_scope, chat_id).await? {
                            return Err(DomainError::GenerationInProgress);
                        }
                        return Err(DomainError::NotLatestTurn);
                    }
                    let user_msg = repo::messages_of_request(tx, &tx_scope, chat_id, request_id).await?.into_iter().find(|m| m.role == "user");
                    chat_turns::Entity::update_many()
                        .secure()
                        .col_expr(chat_turns::Column::DeletedAt, Expr::value(Some(ts)))
                        .col_expr(chat_turns::Column::ReplacedByRequestId, Expr::value(Some(new_request)))
                        .col_expr(chat_turns::Column::UpdatedAt, Expr::value(ts))
                        .filter(Condition::all().add(chat_turns::Column::Id.eq(target_id)))
                        .scope_with(&tx_scope)
                        .exec(tx)
                        .await?;
                    soft_delete_request_messages(tx, &tx_scope, chat_id, request_id, ts).await?;
                    insert_user_message(tx, &tx_scope, tenant_id, chat_id, user_message_id, new_request, &content_tx, ts).await?;
                    if !link_ids.is_empty() {
                        insert_links(tx, &tx_scope, tenant_id, chat_id, user_message_id, &link_ids, ts).await?;
                    }
                    let am = chat_turns::ActiveModel {
                        id: Set(new_turn),
                        tenant_id: Set(tenant_id),
                        chat_id: Set(chat_id),
                        request_id: Set(new_request),
                        requester_type: Set("user".into()),
                        requester_user_id: Set(Some(actor)),
                        state: Set(state::RUNNING.into()),
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
                        web_search_enabled: Set(web_search),
                        web_search_completed_count: Set(0),
                        code_interpreter_completed_count: Set(0),
                        file_search_completed_count: Set(0),
                        completed_at: Set(None),
                        updated_at: Set(ts),
                    };
                    secure_insert::<chat_turns::Entity>(am, &tx_scope, tx).await.map_err(|e| {
                        if e.is_unique_violation() { DomainError::GenerationInProgress } else { e.into() }
                    })?;
                    invalidate_summary(tx, &tx_scope, chat_id, user_msg.as_ref()).await?;
                    touch_chat(tx, &tx_scope, chat_id, ts).await?;
                    let ev = if is_retry {
                        MiniChatAuditEvent::TurnRetry(TurnRetryAuditEvent {
                            tenant_id, actor_user_id: actor, chat_id, original_request_id: request_id, new_request_id: new_request, timestamp: OffsetDateTime::now_utc(),
                        })
                    } else {
                        MiniChatAuditEvent::TurnEdit(TurnEditAuditEvent {
                            tenant_id, actor_user_id: actor, chat_id, original_request_id: request_id, new_request_id: new_request, timestamp: OffsetDateTime::now_utc(),
                        })
                    };
                    outbox.audit(tx, tenant_id, &ev).await.map_err(|e| match e {
                        DomainError::CleanupPayloadTooLarge(m) => DomainError::internal(m),
                        other => other,
                    })
                })
            })
            .await
            .map_err(|e| match e {
                DomainError::Conflict(_) => DomainError::GenerationInProgress,
                other => other,
            })?;
        res.fire();

        // Setup after the commit: context, provider, reserve.
        let (plan, request, target_provider, has_summary) =
            match self.preflight_b(ctx, &chat, &scope, &content, &images, &files, &a, Some(new_request)).await {
                Ok(v) => v,
                Err(e) => {
                    let code = if matches!(e, DomainError::ContextBudgetExceeded) { "context_length_exceeded" } else { "turn_setup_failed" };
                    fail_unstarted(self, &scope, new_turn, code).await;
                    return Err(e);
                }
            };
        let floor = i64::from(self.cfg.estimation_budgets.minimal_generation_floor).min(a.decision.reserve.max_output_tokens_applied);
        let reserve = a.decision.reserve;
        let tier = a.decision.effective.tier;
        let limits = a.limits.clone();
        let starts = quota::PeriodStarts::of(ts);
        let tx_scope = scope.clone();
        let effective_id = a.decision.effective.id.clone();
        let policy_version = a.snapshot.policy_version;
        let user_id = ctx.subject_id();
        let reserve_res = self
            .db
            .write_transaction(move |tx| {
                Box::pin(async move {
                    quota::write_reserve(tx, tenant_id, user_id, &starts, tier, reserve.reserved_credits_micro, &limits).await?;
                    chat_turns::Entity::update_many()
                        .secure()
                        .col_expr(chat_turns::Column::ReserveTokens, Expr::value(Some(reserve.reserve_tokens)))
                        .col_expr(chat_turns::Column::MaxOutputTokensApplied, Expr::value(Some(i32::try_from(reserve.max_output_tokens_applied).unwrap_or(i32::MAX))))
                        .col_expr(chat_turns::Column::ReservedCreditsMicro, Expr::value(Some(reserve.reserved_credits_micro)))
                        .col_expr(chat_turns::Column::PolicyVersionApplied, Expr::value(Some(i64::try_from(policy_version).unwrap_or(i64::MAX))))
                        .col_expr(chat_turns::Column::EffectiveModel, Expr::value(Some(effective_id)))
                        .col_expr(chat_turns::Column::MinimalGenerationFloorApplied, Expr::value(Some(i32::try_from(floor).unwrap_or(i32::MAX))))
                        .filter(Condition::all().add(chat_turns::Column::Id.eq(new_turn)).add(chat_turns::Column::State.eq(state::RUNNING)))
                        .scope_with(&tx_scope)
                        .exec(tx)
                        .await?;
                    Ok(())
                })
            })
            .await;
        if let Err(e) = reserve_res {
            let code = if matches!(e, DomainError::QuotaExceeded(QuotaScope::Tokens)) { "quota_exceeded" } else { "turn_setup_failed" };
            fail_unstarted(self, &scope, new_turn, code).await;
            return Err(e);
        }
        for p in ["daily", "monthly"] {
            self.metrics.inc("quota_reserve_total", &[("period", p)]);
        }
        let citations = repo::citation_map(&files.ready);
        Ok(StreamStart::Live(Box::new(TurnCtx {
            tenant_id,
            user_id,
            chat_id,
            turn_id: new_turn,
            request_id: new_request,
            assistant_message_id: Uuid::new_v4(),
            user_message_id,
            user_message_created_at: ts,
            selected_model: chat.model.clone(),
            effective: a.decision.effective.clone(),
            decision: a.decision.decision,
            downgrade_reason: a.decision.downgrade_reason,
            reserve,
            policy_version,
            limits: a.limits,
            starts,
            floor_applied: floor,
            tools: a.decision.tools,
            target: target_provider,
            request,
            citations,
            assembled_tokens: plan.assembled_tokens,
            effective_budget: plan.effective_budget,
            messages_truncated: plan.messages_truncated,
            has_summary,
            summary_applied: plan.summary_applied,
            started: Instant::now(),
        })))
    }
}
