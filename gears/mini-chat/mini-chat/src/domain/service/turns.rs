//! Turn status and tail-only turn mutations (DESIGN §3.9).

use std::sync::Arc;
use std::time::Instant;

use chrono::{DateTime, Utc};
use mini_chat_sdk::{AuditEnvelope, TurnMutationAuditEvent};
use sea_orm::sea_query::Expr;
use sea_orm::{ColumnTrait, Condition, EntityTrait, QueryFilter};
use toolkit_db::DbTx;
use toolkit_db::secure::{DBRunner, SecureDeleteExt, SecureEntityExt, SecureUpdateExt};
use toolkit_security::{AccessScope, SecurityContext};
use uuid::Uuid;

use super::chats::{load_chat, touch_chat};
use super::quota;
use super::stream::{
    BuildInput, PreflightFacts, StreamStart, TurnRun, attachment_facts, chat_model, find_turn, insert_message,
    insert_turn, link_attachments, prior_context_tokens, NewMessage, NewTurn,
};
use super::{AppServices, now, policy};
use crate::domain::context;
use crate::domain::error::{DomainError, NotFoundKind};
use crate::infra::db::entity::{attachments, chat_turns, message_attachments, messages, thread_summaries};
use crate::infra::outbox::Wakes;

/// Turn status (DESIGN §3.3 "Turn Status API").
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnStatusView {
    pub request_id: Uuid,
    pub state: &'static str,
    pub error_code: Option<String>,
    pub assistant_message_id: Option<Uuid>,
    pub updated_at: DateTime<Utc>,
}

fn api_state(s: &str) -> &'static str {
    match s {
        "completed" => "done",
        "failed" => "error",
        "cancelled" => "cancelled",
        _ => "running",
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MutationOp {
    Retry,
    Edit,
}

impl MutationOp {
    fn action(self) -> &'static str {
        match self {
            Self::Retry => "retry_turn",
            Self::Edit => "edit_turn",
        }
    }

    fn event(self) -> &'static str {
        match self {
            Self::Retry => "turn_retry",
            Self::Edit => "turn_edit",
        }
    }
}

/// Latest non-deleted turn by `(started_at, id)`.
async fn latest_turn(runner: &impl DBRunner, tenant_id: Uuid, chat_id: Uuid) -> Result<Option<chat_turns::Model>, DomainError> {
    Ok(chat_turns::Entity::find()
        .secure()
        .scope_with(&AccessScope::for_tenant(tenant_id))
        .filter(
            Condition::all()
                .add(chat_turns::Column::ChatId.eq(chat_id))
                .add(chat_turns::Column::DeletedAt.is_null()),
        )
        .order_by(chat_turns::Column::StartedAt, sea_orm::Order::Desc)
        .order_by(chat_turns::Column::Id, sea_orm::Order::Desc)
        .limit(1)
        .all(runner)
        .await?
        .into_iter()
        .next())
}

/// Read-only mutation validation (rules 1–3).
async fn preview(
    runner: &impl DBRunner,
    tenant_id: Uuid,
    user_id: Uuid,
    chat_id: Uuid,
    request_id: Uuid,
) -> Result<chat_turns::Model, DomainError> {
    let target = find_turn(runner, tenant_id, chat_id, request_id)
        .await?
        .ok_or(DomainError::NotFound(NotFoundKind::Turn))?;
    if target.deleted_at.is_some() {
        return Err(DomainError::NotLatestTurn);
    }
    if target.state == "running" {
        return Err(DomainError::TurnNotTerminal);
    }
    if target.requester_user_id != Some(user_id) {
        return Err(DomainError::AccessDenied);
    }
    let latest = latest_turn(runner, tenant_id, chat_id).await?;
    if latest.as_ref().map(|t| t.id) != Some(target.id) {
        return Err(DomainError::NotLatestTurn);
    }
    Ok(target)
}

/// User message of a turn.
async fn turn_user_message(
    runner: &impl DBRunner,
    tenant_id: Uuid,
    chat_id: Uuid,
    request_id: Uuid,
) -> Result<Option<messages::Model>, DomainError> {
    Ok(messages::Entity::find()
        .secure()
        .scope_with(&AccessScope::for_tenant(tenant_id))
        .filter(
            Condition::all()
                .add(messages::Column::ChatId.eq(chat_id))
                .add(messages::Column::RequestId.eq(request_id))
                .add(messages::Column::Role.eq("user")),
        )
        .order_by(messages::Column::CreatedAt, sea_orm::Order::Desc)
        .limit(1)
        .all(runner)
        .await?
        .into_iter()
        .next())
}

/// Non-deleted attachments linked to a message.
async fn linked_attachments(
    runner: &impl DBRunner,
    tenant_id: Uuid,
    chat_id: Uuid,
    message_id: Uuid,
) -> Result<Vec<attachments::Model>, DomainError> {
    let scope = AccessScope::for_tenant(tenant_id);
    let mut links = message_attachments::Entity::find()
        .secure()
        .scope_with(&scope)
        .filter(
            Condition::all()
                .add(message_attachments::Column::ChatId.eq(chat_id))
                .add(message_attachments::Column::MessageId.eq(message_id)),
        )
        .all(runner)
        .await?;
    if links.is_empty() {
        return Ok(Vec::new());
    }
    links.sort_by_key(|l| (l.created_at, l.attachment_id));
    let ids: Vec<Uuid> = links.iter().map(|l| l.attachment_id).collect();
    let rows = attachments::Entity::find()
        .secure()
        .scope_with(&scope)
        .filter(
            Condition::all()
                .add(attachments::Column::Id.is_in(ids.clone()))
                .add(attachments::Column::DeletedAt.is_null()),
        )
        .all(runner)
        .await?;
    Ok(ids
        .iter()
        .filter_map(|id| rows.iter().find(|a| a.id == *id).cloned())
        .collect())
}

/// Deletes the chat summary when it covers the given message and clears
/// `is_compressed` (DESIGN §3.9 "Summary Interaction on Turn Mutation").
///
/// # Errors
/// Database failure.
pub async fn invalidate_summary(
    tx: &DbTx<'_>,
    tenant_id: Uuid,
    chat_id: Uuid,
    user_message: Option<&messages::Model>,
) -> Result<(), DomainError> {
    let scope = AccessScope::for_tenant(tenant_id);
    let Some(summary) = thread_summaries::Entity::find()
        .secure()
        .scope_with(&scope)
        .filter(Condition::all().add(thread_summaries::Column::ChatId.eq(chat_id)))
        .one(tx)
        .await?
    else {
        return Ok(());
    };
    let covers = user_message.is_none_or(|m| {
        (summary.summarized_up_to_created_at, summary.summarized_up_to_message_id) >= (m.created_at, m.id)
    });
    if !covers {
        return Ok(());
    }
    thread_summaries::Entity::delete_many()
        .secure()
        .scope_with(&scope)
        .filter(Condition::all().add(thread_summaries::Column::ChatId.eq(chat_id)))
        .exec(tx)
        .await?;
    messages::Entity::update_many()
        .col_expr(messages::Column::IsCompressed, Expr::value(false))
        .filter(Condition::all().add(messages::Column::ChatId.eq(chat_id)))
        .secure()
        .scope_with(&scope)
        .exec(tx)
        .await?;
    Ok(())
}

/// Soft-deletes a turn (CAS on non-deleted, terminal) and its messages.
async fn soft_delete_turn(
    tx: &DbTx<'_>,
    tenant_id: Uuid,
    chat_id: Uuid,
    target: &chat_turns::Model,
    replaced_by: Option<Uuid>,
) -> Result<(), DomainError> {
    let scope = AccessScope::for_tenant(tenant_id);
    let ts = now();
    let rows = chat_turns::Entity::update_many()
        .col_expr(chat_turns::Column::DeletedAt, Expr::value(Some(ts)))
        .col_expr(chat_turns::Column::ReplacedByRequestId, Expr::value(replaced_by))
        .col_expr(chat_turns::Column::UpdatedAt, Expr::value(ts))
        .filter(
            Condition::all()
                .add(chat_turns::Column::Id.eq(target.id))
                .add(chat_turns::Column::DeletedAt.is_null())
                .add(chat_turns::Column::State.ne("running")),
        )
        .secure()
        .scope_with(&scope)
        .exec(tx)
        .await?
        .rows_affected;
    if rows == 0 {
        return Err(DomainError::NotLatestTurn);
    }
    messages::Entity::update_many()
        .col_expr(messages::Column::DeletedAt, Expr::value(Some(ts)))
        .filter(
            Condition::all()
                .add(messages::Column::ChatId.eq(chat_id))
                .add(messages::Column::RequestId.eq(target.request_id))
                .add(messages::Column::DeletedAt.is_null()),
        )
        .secure()
        .scope_with(&scope)
        .exec(tx)
        .await?;
    Ok(())
}

impl AppServices {
    /// `GET /v1/chats/{id}/turns/{request_id}`.
    ///
    /// # Errors
    /// `NotFound`, authorization and database errors.
    pub async fn get_turn(&self, ctx: &SecurityContext, chat_id: Uuid, request_id: Uuid) -> Result<TurnStatusView, DomainError> {
        let scope = self.authz.chat_scope(ctx, "read_turn", Some(chat_id)).await?;
        let conn = self.conn()?;
        let chat = load_chat(&conn, &scope, chat_id).await?;
        let t = find_turn(&conn, chat.tenant_id, chat_id, request_id)
            .await?
            .filter(|t| t.deleted_at.is_none())
            .ok_or(DomainError::NotFound(NotFoundKind::Turn))?;
        let state = api_state(&t.state);
        Ok(TurnStatusView {
            request_id: t.request_id,
            state,
            error_code: if state == "error" { t.error_code } else { None },
            assistant_message_id: if matches!(state, "done" | "cancelled") {
                t.assistant_message_id
            } else {
                None
            },
            updated_at: t.updated_at,
        })
    }

    /// `DELETE /v1/chats/{id}/turns/{request_id}`.
    ///
    /// # Errors
    /// Mutation rule violations, authorization and database errors.
    pub async fn delete_turn(&self, ctx: &SecurityContext, chat_id: Uuid, request_id: Uuid) -> Result<(), DomainError> {
        let scope = self.authz.chat_scope(ctx, "delete_turn", Some(chat_id)).await?;
        let user_id = ctx.subject_id();
        let conn = self.conn()?;
        let chat = load_chat(&conn, &scope, chat_id).await?;
        let target = preview(&conn, chat.tenant_id, user_id, chat_id, request_id).await?;
        let user_msg = turn_user_message(&conn, chat.tenant_id, chat_id, request_id).await?;
        drop(conn);
        let outbox = Arc::clone(&self.outbox);
        let tenant_id = chat.tenant_id;
        let wakes = self
            .db
            .transaction_ref_mapped(move |tx| {
                Box::pin(async move {
                    crate::domain::service::lock_for_write(tx).await?;
                    soft_delete_turn(tx, tenant_id, chat_id, &target, None).await?;
                    invalidate_summary(tx, tenant_id, chat_id, user_msg.as_ref()).await?;
                    let mut w = Wakes::default();
                    let ev = TurnMutationAuditEvent {
                        event_type: "turn_delete".to_owned(),
                        tenant_id,
                        actor_user_id: user_id,
                        chat_id,
                        original_request_id: None,
                        new_request_id: None,
                        request_id: Some(request_id),
                        timestamp: time::OffsetDateTime::now_utc(),
                    };
                    w.push(outbox.audit(tx, tenant_id, &AuditEnvelope::Mutation(ev)).await?);
                    Ok::<_, DomainError>(w)
                })
            })
            .await?;
        wakes.fire();
        Ok(())
    }

    /// Retry (`content = None`) or edit (`content = Some`) of the latest turn.
    ///
    /// # Errors
    /// Mutation rule violations, preflight rejections, authorization and database errors.
    #[allow(clippy::too_many_lines)]
    pub async fn mutate_turn(
        self: &Arc<Self>,
        ctx: &SecurityContext,
        chat_id: Uuid,
        request_id: Uuid,
        op: MutationOp,
        content: Option<String>,
    ) -> Result<StreamStart, DomainError> {
        if let Some(c) = &content
            && c.trim().is_empty()
        {
            return Err(DomainError::EmptyContent);
        }
        let scope = self.authz.chat_scope(ctx, op.action(), Some(chat_id)).await?;
        let user_id = ctx.subject_id();
        let conn = self.conn()?;
        let chat = load_chat(&conn, &scope, chat_id).await?;
        let tenant_id = chat.tenant_id;
        // 1. read-only preview
        let target = preview(&conn, tenant_id, user_id, chat_id, request_id).await?;
        let old_msg = turn_user_message(&conn, tenant_id, chat_id, request_id).await?;
        let new_content = match (&content, &old_msg) {
            (Some(c), _) => c.clone(),
            (None, Some(m)) => m.content.clone(),
            (None, None) => return Err(DomainError::internal("turn has no user message")),
        };
        let carried = match &old_msg {
            Some(m) => linked_attachments(&conn, tenant_id, chat_id, m.id).await?,
            None => Vec::new(),
        };
        let images: Vec<&attachments::Model> = carried.iter().filter(|a| a.attachment_kind == "image").collect();
        // 2. preflight without changes
        let snapshot = policy::current_snapshot(self.policy.as_ref(), user_id).await?;
        chat_model(&snapshot, &chat.model)?;
        let limits = policy::user_limits(self.policy.as_ref(), user_id, snapshot.policy_version).await?;
        let web = target.web_search_enabled;
        if web && snapshot.kill_switches.disable_web_search {
            return Err(DomainError::FeatureDisabled("web_search"));
        }
        if !images.is_empty() && snapshot.kill_switches.disable_images {
            return Err(DomainError::FeatureDisabled("images"));
        }
        if images.len() > self.cfg.rag.max_images_per_message as usize {
            return Err(DomainError::TooManyImages);
        }
        let image_ids: Vec<String> = images.iter().filter_map(|a| a.provider_file_id.clone()).collect();
        let facts = attachment_facts(&conn, tenant_id, chat_id).await?;
        let prior = prior_context_tokens(&conn, tenant_id, chat_id).await?;
        let decision = self
            .preflight(&conn, &snapshot, &limits, &chat.model, tenant_id, user_id, PreflightFacts {
                content: &new_content,
                image_count: images.len(),
                prior,
                facts: &facts,
                web_search: web,
            })
            .await?;
        drop(conn);

        // 3. mutation commit
        let new_request_id = Uuid::new_v4();
        let new_turn_id = Uuid::new_v4();
        let new_msg_id = Uuid::new_v4();
        let carried_ids: Vec<Uuid> = carried.iter().map(|a| a.id).collect();
        let outbox = Arc::clone(&self.outbox);
        let mc = new_content.clone();
        let tgt = target.clone();
        let om = old_msg.clone();
        let commit = self
            .db
            .transaction_ref_mapped(move |tx| {
                Box::pin(async move {
                    crate::domain::service::lock_for_write(tx).await?;
                    insert_turn(tx, NewTurn {
                        id: new_turn_id,
                        tenant_id,
                        chat_id,
                        request_id: new_request_id,
                        user_id,
                        web_search_enabled: web,
                        preflight: None,
                    })
                    .await
                    .map_err(|e| match e {
                        DomainError::UniqueViolation => DomainError::GenerationInProgress,
                        other => other,
                    })?;
                    soft_delete_turn(tx, tenant_id, chat_id, &tgt, Some(new_request_id)).await?;
                    insert_message(tx, NewMessage {
                        id: new_msg_id,
                        tenant_id,
                        chat_id,
                        request_id: new_request_id,
                        role: "user",
                        content: mc,
                        model: None,
                        usage: None,
                        provider_response_id: None,
                        created_at: now(),
                    })
                    .await?;
                    link_attachments(tx, tenant_id, chat_id, new_msg_id, &carried_ids).await?;
                    invalidate_summary(tx, tenant_id, chat_id, om.as_ref()).await?;
                    touch_chat(tx, tenant_id, chat_id).await?;
                    let mut w = Wakes::default();
                    let ev = TurnMutationAuditEvent {
                        event_type: op.event().to_owned(),
                        tenant_id,
                        actor_user_id: user_id,
                        chat_id,
                        original_request_id: Some(request_id),
                        new_request_id: Some(new_request_id),
                        request_id: None,
                        timestamp: time::OffsetDateTime::now_utc(),
                    };
                    w.push(outbox.audit(tx, tenant_id, &AuditEnvelope::Mutation(ev)).await?);
                    Ok::<_, DomainError>(w)
                })
            })
            .await?;
        commit.fire();

        // 4. context assembly, provider resolution, reserve (last step)
        let conn = self.conn()?;
        let built = match self
            .build_request(&conn, BuildInput {
                ctx_tenant: tenant_id,
                ctx_user: user_id,
                chat_id,
                decision: &decision,
                content: &new_content,
                image_file_ids: image_ids,
                facts: &facts,
                exclude_request: Some(new_request_id),
            })
            .await
        {
            Ok(b) => b,
            Err(e) => {
                let code = if matches!(e, DomainError::ContextBudgetExceeded) {
                    "context_length_exceeded"
                } else {
                    "turn_setup_failed"
                };
                drop(conn);
                self.fail_unstarted(tenant_id, new_turn_id, code).await;
                return Err(e);
            }
        };
        drop(conn);
        let d = decision.clone();
        let reserve = self
            .db
            .transaction_ref_mapped(move |tx| {
                Box::pin(async move {
                    crate::domain::service::lock_for_write(tx).await?;
                    quota::reserve(tx, tenant_id, user_id, &d).await?;
                    chat_turns::Entity::update_many()
                        .col_expr(chat_turns::Column::ReserveTokens, Expr::value(Some(d.reserve_tokens)))
                        .col_expr(
                            chat_turns::Column::MaxOutputTokensApplied,
                            Expr::value(Some(i32::try_from(d.max_output_tokens_applied).unwrap_or(i32::MAX))),
                        )
                        .col_expr(
                            chat_turns::Column::ReservedCreditsMicro,
                            Expr::value(Some(d.reserved_credits_micro)),
                        )
                        .col_expr(
                            chat_turns::Column::PolicyVersionApplied,
                            Expr::value(Some(i64::try_from(d.policy_version).unwrap_or(i64::MAX))),
                        )
                        .col_expr(chat_turns::Column::EffectiveModel, Expr::value(Some(d.effective.id.clone())))
                        .col_expr(
                            chat_turns::Column::MinimalGenerationFloorApplied,
                            Expr::value(Some(i32::try_from(d.minimal_generation_floor_applied).unwrap_or(i32::MAX))),
                        )
                        .filter(
                            Condition::all()
                                .add(chat_turns::Column::Id.eq(new_turn_id))
                                .add(chat_turns::Column::State.eq("running")),
                        )
                        .secure()
                        .scope_with(&AccessScope::for_tenant(tenant_id))
                        .exec(tx)
                        .await?;
                    Ok::<_, DomainError>(())
                })
            })
            .await;
        if let Err(e) = reserve {
            let code = if matches!(e, DomainError::QuotaExceeded(_)) {
                "quota_exceeded"
            } else {
                "turn_setup_failed"
            };
            self.fail_unstarted(tenant_id, new_turn_id, code).await;
            return Err(e);
        }
        let run = TurnRun {
            tenant_id,
            user_id,
            chat_id,
            turn_id: new_turn_id,
            request_id: new_request_id,
            message_id: Uuid::new_v4(),
            selected_model: chat.model.clone(),
            decision,
            provider: built.provider,
            request: built.request,
            citation_map: built.citation_map,
            summary_applied: built.summary_applied,
            summary_trigger: self.cfg.thread_summary_worker.enabled
                && context::summary_trigger(
                    &built.plan,
                    built.has_summary,
                    self.cfg.thread_summary_worker.compression_threshold_pct,
                ),
            started: Instant::now(),
        };
        Ok(self.spawn_turn(run))
    }

    /// Marks an unstarted retry/edit turn failed (no reserve, no settlement).
    async fn fail_unstarted(&self, tenant_id: Uuid, turn_id: Uuid, code: &'static str) {
        let Ok(conn) = self.conn() else { return };
        let ts = now();
        let _ = chat_turns::Entity::update_many()
            .col_expr(chat_turns::Column::State, Expr::value("failed"))
            .col_expr(chat_turns::Column::ErrorCode, Expr::value(Some(code)))
            .col_expr(chat_turns::Column::CompletedAt, Expr::value(Some(ts)))
            .col_expr(chat_turns::Column::UpdatedAt, Expr::value(ts))
            .filter(
                Condition::all()
                    .add(chat_turns::Column::Id.eq(turn_id))
                    .add(chat_turns::Column::State.eq("running")),
            )
            .secure()
            .scope_with(&AccessScope::for_tenant(tenant_id))
            .exec(&conn)
            .await;
    }
}
