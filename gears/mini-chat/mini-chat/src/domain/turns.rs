//! Turn mutations: retry, edit and delete of the latest turn (DESIGN §3.9).

use std::sync::Arc;

use mini_chat_sdk::{MiniChatAuditEvent, TurnMutationAuditEvent};
use sea_orm::sea_query::Expr;
use sea_orm::{ActiveValue, ColumnTrait, Condition, EntityTrait, Order, QueryFilter};
use time::OffsetDateTime;
use toolkit_db::DbTx;
use toolkit_db::secure::{DBRunner, SecureDeleteExt, SecureEntityExt, SecureInsertExt, SecureUpdateExt};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use super::app::{App, fire, now, owner_scope, tenant_scope};
use super::authz::{actions, chat_scope};
use super::chats::{load_chat, touch_chat};
use super::context;
use super::error::DomainError;
use super::finalize::TurnRecord;
use super::quota::{self, PeriodStarts, TurnReserve};
use super::stream::{StreamHeader, StreamStart, TurnRun, insert_user_message, load_chat_facts, load_images};
use crate::infra::db::entity::{chat_turns, chats, message_attachments, messages, thread_summaries};

/// Kind of mutation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mutation {
    Retry,
    Edit,
}

impl Mutation {
    fn action(self) -> &'static str {
        match self {
            Self::Retry => actions::RETRY_TURN,
            Self::Edit => actions::EDIT_TURN,
        }
    }

    fn audit_type(self) -> &'static str {
        match self {
            Self::Retry => "turn_retry",
            Self::Edit => "turn_edit",
        }
    }
}

/// Latest non-deleted turn of a chat by `(started_at, id)`.
///
/// # Errors
/// Database errors.
pub async fn latest_turn(runner: &impl DBRunner, tenant_id: Uuid, chat_id: Uuid) -> Result<Option<chat_turns::Model>, DomainError> {
    Ok(chat_turns::Entity::find()
        .secure()
        .scope_with(&tenant_scope(tenant_id))
        .filter(
            Condition::all()
                .add(chat_turns::Column::ChatId.eq(chat_id))
                .add(chat_turns::Column::DeletedAt.is_null()),
        )
        .order_by(chat_turns::Column::StartedAt, Order::Desc)
        .order_by(chat_turns::Column::Id, Order::Desc)
        .limit(1)
        .one(runner)
        .await?)
}

/// Read-only mutation preview: existence, terminal state, latest, ownership.
async fn preview(runner: &impl DBRunner, ctx: &SecurityContext, chat: &chats::Model, request_id: Uuid) -> Result<chat_turns::Model, DomainError> {
    let target = super::stream::find_turn(runner, chat.tenant_id, chat.id, request_id)
        .await?
        .ok_or(DomainError::TurnNotFound)?;
    if target.deleted_at.is_some() {
        return Err(DomainError::NotLatestTurn);
    }
    if target.state == "running" {
        return Err(DomainError::TurnNotTerminal);
    }
    let latest = latest_turn(runner, chat.tenant_id, chat.id).await?;
    if latest.map(|l| l.id) != Some(target.id) {
        return Err(DomainError::NotLatestTurn);
    }
    if target.requester_user_id.is_some_and(|u| u != ctx.subject_id()) {
        return Err(DomainError::AuthzDenied);
    }
    Ok(target)
}

/// The user message of a turn.
async fn turn_user_message(runner: &impl DBRunner, chat: &chats::Model, request_id: Uuid) -> Result<Option<messages::Model>, DomainError> {
    Ok(messages::Entity::find()
        .secure()
        .scope_with(&tenant_scope(chat.tenant_id))
        .filter(
            Condition::all()
                .add(messages::Column::ChatId.eq(chat.id))
                .add(messages::Column::RequestId.eq(request_id))
                .add(messages::Column::Role.eq("user")),
        )
        .one(runner)
        .await?)
}

/// Non-deleted attachments linked to a message.
async fn linked_attachment_ids(runner: &impl DBRunner, chat: &chats::Model, message_id: Uuid) -> Result<Vec<Uuid>, DomainError> {
    let links = message_attachments::Entity::find()
        .secure()
        .scope_with(&tenant_scope(chat.tenant_id))
        .filter(
            Condition::all()
                .add(message_attachments::Column::ChatId.eq(chat.id))
                .add(message_attachments::Column::MessageId.eq(message_id)),
        )
        .order_by(message_attachments::Column::CreatedAt, Order::Asc)
        .all(runner)
        .await?;
    let ids: Vec<Uuid> = links.iter().map(|l| l.attachment_id).collect();
    if ids.is_empty() {
        return Ok(ids);
    }
    use crate::infra::db::entity::attachments;
    let live: Vec<Uuid> = attachments::Entity::find()
        .secure()
        .scope_with(&tenant_scope(chat.tenant_id))
        .filter(
            Condition::all()
                .add(attachments::Column::Id.is_in(ids.clone()))
                .add(attachments::Column::DeletedAt.is_null()),
        )
        .all(runner)
        .await?
        .into_iter()
        .map(|a| a.id)
        .collect();
    Ok(ids.into_iter().filter(|i| live.contains(i)).collect())
}

/// Soft-deletes the target turn (CAS on `deleted_at IS NULL` and terminal
/// state) and its messages, and drops a summary that covers its user message.
async fn soft_delete_turn(
    tx: &DbTx<'_>,
    chat: &chats::Model,
    target: &chat_turns::Model,
    replaced_by: Option<Uuid>,
    at: OffsetDateTime,
) -> Result<(), DomainError> {
    let scope = tenant_scope(chat.tenant_id);
    let rows = chat_turns::Entity::update_many()
        .col_expr(chat_turns::Column::DeletedAt, Expr::value(Some(at)))
        .col_expr(chat_turns::Column::ReplacedByRequestId, Expr::value(replaced_by))
        .col_expr(chat_turns::Column::UpdatedAt, Expr::value(at))
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
    if latest_turn(tx, chat.tenant_id, chat.id).await?.is_some_and(|l| {
        (l.started_at, l.id) > (target.started_at, target.id)
    }) {
        return Err(DomainError::NotLatestTurn);
    }
    let user_msg = turn_user_message(tx, chat, target.request_id).await?;
    messages::Entity::update_many()
        .col_expr(messages::Column::DeletedAt, Expr::value(Some(at)))
        .filter(
            Condition::all()
                .add(messages::Column::ChatId.eq(chat.id))
                .add(messages::Column::RequestId.eq(target.request_id))
                .add(messages::Column::DeletedAt.is_null()),
        )
        .secure()
        .scope_with(&scope)
        .exec(tx)
        .await?;
    if let Some(um) = user_msg {
        let summary = thread_summaries::Entity::find()
            .secure()
            .scope_with(&scope)
            .filter(Condition::all().add(thread_summaries::Column::ChatId.eq(chat.id)))
            .one(tx)
            .await?;
        if let Some(s) = summary
            && (s.summarized_up_to_created_at, s.summarized_up_to_message_id) >= (um.created_at, um.id)
        {
            thread_summaries::Entity::delete_many()
                .filter(Condition::all().add(thread_summaries::Column::Id.eq(s.id)))
                .secure()
                .scope_with(&scope)
                .exec(tx)
                .await?;
            messages::Entity::update_many()
                .col_expr(messages::Column::IsCompressed, Expr::value(false))
                .filter(Condition::all().add(messages::Column::ChatId.eq(chat.id)))
                .secure()
                .scope_with(&scope)
                .exec(tx)
                .await?;
        }
    }
    Ok(())
}

impl App {
    /// Retry or edit of the latest turn; streams the new generation.
    ///
    /// # Errors
    /// Pre-stream JSON errors.
    #[allow(clippy::too_many_lines)]
    pub async fn start_mutation(
        self: &Arc<Self>,
        ctx: SecurityContext,
        chat_id: Uuid,
        request_id: Uuid,
        kind: Mutation,
        new_content: Option<String>,
    ) -> Result<StreamStart, DomainError> {
        if kind == Mutation::Edit && new_content.as_deref().is_none_or(|c| c.trim().is_empty()) {
            return Err(DomainError::EmptyContent);
        }
        let scope = chat_scope(&self.enforcer, &ctx, kind.action(), Some(chat_id)).await?;
        let tenant_id = ctx.subject_tenant_id();
        let user_id = ctx.subject_id();
        let conn = self.db.conn()?;
        let chat = load_chat(&conn, &scope, chat_id).await?;
        let target = preview(&conn, &ctx, &chat, request_id).await?;
        let original = turn_user_message(&conn, &chat, target.request_id).await?;
        let content = match kind {
            Mutation::Edit => new_content.unwrap_or_default(),
            Mutation::Retry => original.as_ref().map(|m| m.content.clone()).unwrap_or_default(),
        };
        if content.trim().is_empty() {
            return Err(DomainError::EmptyContent);
        }
        let attachment_ids = match &original {
            Some(m) => linked_attachment_ids(&conn, &chat, m.id).await?,
            None => Vec::new(),
        };
        let images = load_images(&conn, chat.tenant_id, chat.id, &attachment_ids).await?;
        let facts = load_chat_facts(&conn, chat.tenant_id, chat.id).await?;
        drop(conn);
        let pre = self
            .preflight(tenant_id, user_id, &chat, &facts, &content, &images, target.web_search_enabled)
            .await?;

        // Mutation commit.
        let new_request_id = Uuid::new_v4();
        let new_turn_id = Uuid::new_v4();
        let user_message_id = Uuid::new_v4();
        let app = Arc::clone(self);
        let chat_c = chat.clone();
        let target_c = target.clone();
        let content_c = content.clone();
        let ids_c = attachment_ids.clone();
        let commit_at = now();
        let res = self
            .db
            .transaction(move |tx| {
                Box::pin(async move {
                    let at = commit_at;
                    soft_delete_turn(tx, &chat_c, &target_c, Some(new_request_id), at).await?;
                    insert_user_message(tx, chat_c.tenant_id, chat_c.id, user_message_id, new_request_id, &content_c, at).await?;
                    let scope = tenant_scope(chat_c.tenant_id);
                    for id in &ids_c {
                        let am = message_attachments::ActiveModel {
                            tenant_id: ActiveValue::Set(chat_c.tenant_id),
                            chat_id: ActiveValue::Set(chat_c.id),
                            message_id: ActiveValue::Set(user_message_id),
                            attachment_id: ActiveValue::Set(*id),
                            created_at: ActiveValue::Set(at),
                        };
                        message_attachments::Entity::insert(am).secure().scope_unchecked(&scope)?.exec(tx).await?;
                    }
                    let am = chat_turns::ActiveModel {
                        id: ActiveValue::Set(new_turn_id),
                        tenant_id: ActiveValue::Set(chat_c.tenant_id),
                        chat_id: ActiveValue::Set(chat_c.id),
                        request_id: ActiveValue::Set(new_request_id),
                        requester_type: ActiveValue::Set("user".into()),
                        requester_user_id: ActiveValue::Set(Some(user_id)),
                        state: ActiveValue::Set("running".into()),
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
                        started_at: ActiveValue::Set(at),
                        last_progress_at: ActiveValue::Set(Some(at)),
                        web_search_enabled: ActiveValue::Set(target_c.web_search_enabled),
                        web_search_completed_count: ActiveValue::Set(0),
                        code_interpreter_completed_count: ActiveValue::Set(0),
                        file_search_completed_count: ActiveValue::Set(0),
                        completed_at: ActiveValue::Set(None),
                        updated_at: ActiveValue::Set(at),
                    };
                    chat_turns::Entity::insert(am)
                        .secure()
                        .scope_unchecked(&scope)?
                        .exec(tx)
                        .await
                        .map_err(|e| {
                            if e.is_unique_violation() {
                                DomainError::GenerationInProgress
                            } else {
                                e.into()
                            }
                        })?;
                    touch_chat(tx, chat_c.tenant_id, chat_c.id, at).await?;
                    let ev = MiniChatAuditEvent::TurnMutation(TurnMutationAuditEvent {
                        event_type: kind.audit_type().into(),
                        tenant_id: chat_c.tenant_id,
                        actor_user_id: user_id,
                        chat_id: chat_c.id,
                        original_request_id: Some(target_c.request_id),
                        new_request_id: Some(new_request_id),
                        request_id: None,
                        timestamp: at,
                    });
                    let wake = app.outbox.audit(tx, &ev).await?;
                    Ok(vec![wake])
                })
            })
            .await
            .map_err(|e| if matches!(e, DomainError::UniqueViolation) { DomainError::GenerationInProgress } else { e })?;
        fire(res);

        // Post-commit setup: context, provider, reserve.
        let call = match self
            .prepare_call(tenant_id, user_id, chat.id, &pre, &facts, &content, &images, Some(new_request_id))
            .await
        {
            Ok(c) => c,
            Err(e) => {
                let code = if matches!(e, DomainError::ContextBudgetExceeded) {
                    "context_length_exceeded"
                } else {
                    "turn_setup_failed"
                };
                self.fail_unstarted_turn(chat.tenant_id, new_turn_id, code).await?;
                return Err(e);
            }
        };
        let reserve = pre.decision.reserve;
        let tier = pre.decision.effective.tier;
        let limits = pre.limits.clone();
        let policy_version = pre.snapshot.policy_version;
        let effective_model = pre.decision.effective.id.clone();
        let chat_tenant = chat.tenant_id;
        let at = commit_at;
        let eff_c = effective_model.clone();
        let reserve_res = self
            .db
            .transaction(move |tx| {
                Box::pin(async move {
                    let rows = chat_turns::Entity::update_many()
                        .col_expr(chat_turns::Column::ReserveTokens, Expr::value(Some(reserve.reserve_tokens)))
                        .col_expr(
                            chat_turns::Column::MaxOutputTokensApplied,
                            Expr::value(Some(i32::try_from(reserve.max_output_tokens_applied).unwrap_or(i32::MAX))),
                        )
                        .col_expr(chat_turns::Column::ReservedCreditsMicro, Expr::value(Some(reserve.reserved_credits_micro)))
                        .col_expr(
                            chat_turns::Column::PolicyVersionApplied,
                            Expr::value(Some(i64::try_from(policy_version).unwrap_or(i64::MAX))),
                        )
                        .col_expr(chat_turns::Column::EffectiveModel, Expr::value(Some(eff_c)))
                        .col_expr(
                            chat_turns::Column::MinimalGenerationFloorApplied,
                            Expr::value(Some(i32::try_from(reserve.minimal_generation_floor_applied).unwrap_or(i32::MAX))),
                        )
                        .col_expr(chat_turns::Column::UpdatedAt, Expr::value(now()))
                        .filter(
                            Condition::all()
                                .add(chat_turns::Column::Id.eq(new_turn_id))
                                .add(chat_turns::Column::State.eq("running")),
                        )
                        .secure()
                        .scope_with(&tenant_scope(chat_tenant))
                        .exec(tx)
                        .await?
                        .rows_affected;
                    if rows == 0 {
                        return Err(DomainError::internal("mutation turn is no longer running"));
                    }
                    quota::write_reserve(
                        tx,
                        &owner_scope(tenant_id, user_id),
                        tenant_id,
                        user_id,
                        tier,
                        reserve.reserved_credits_micro,
                        PeriodStarts::at(at),
                        &limits,
                        at,
                    )
                    .await
                })
            })
            .await;
        if let Err(e) = reserve_res {
            let code = if matches!(e, DomainError::QuotaExceeded(_)) { "quota_exceeded" } else { "turn_setup_failed" };
            self.fail_unstarted_turn(chat.tenant_id, new_turn_id, code).await?;
            return Err(e);
        }
        let summary_trigger = self.cfg.thread_summary_worker.enabled
            && context::summary_trigger(&call.plan, call.has_summary, self.cfg.thread_summary_worker.compression_threshold_pct);
        let assistant_message_id = Uuid::new_v4();
        let rec = TurnRecord {
            tenant_id: chat.tenant_id,
            user_id,
            chat_id: chat.id,
            turn_id: new_turn_id,
            request_id: new_request_id,
            message_id: assistant_message_id,
            selected_model: chat.model.clone(),
            effective_model,
            policy_version,
            reserve: TurnReserve {
                reserve_tokens: reserve.reserve_tokens,
                max_output_tokens_applied: reserve.max_output_tokens_applied,
                reserved_credits_micro: reserve.reserved_credits_micro,
                minimal_generation_floor_applied: reserve.minimal_generation_floor_applied,
            },
            started_at: at,
            downgrade_reason: pre.decision.downgrade_reason.map(str::to_owned),
        };
        let header = StreamHeader {
            request_id: new_request_id,
            message_id: assistant_message_id,
            is_new_turn: true,
            summary_token_estimate: call.plan.summary_applied,
        };
        Ok(StreamStart::Live(self.spawn_turn(
            header,
            TurnRun {
                rec,
                provider: call.provider,
                request: call.request,
                file_map: facts.file_map,
                summary_trigger,
            },
        )))
    }

    /// `DELETE /v1/chats/{id}/turns/{request_id}`.
    ///
    /// # Errors
    /// Authorization, not found, state, conflict or database errors.
    pub async fn delete_turn(self: &Arc<Self>, ctx: &SecurityContext, chat_id: Uuid, request_id: Uuid) -> Result<(), DomainError> {
        let scope = chat_scope(&self.enforcer, ctx, actions::DELETE_TURN, Some(chat_id)).await?;
        let (chat, target) = {
            let conn = self.db.conn()?;
            let chat = load_chat(&conn, &scope, chat_id).await?;
            let target = preview(&conn, ctx, &chat, request_id).await?;
            (chat, target)
        };
        let app = Arc::clone(self);
        let user_id = ctx.subject_id();
        let wakes = self
            .db
            .transaction(move |tx| {
                Box::pin(async move {
                    let at = now();
                    soft_delete_turn(tx, &chat, &target, None, at).await?;
                    let ev = MiniChatAuditEvent::TurnMutation(TurnMutationAuditEvent {
                        event_type: "turn_delete".into(),
                        tenant_id: chat.tenant_id,
                        actor_user_id: user_id,
                        chat_id: chat.id,
                        original_request_id: None,
                        new_request_id: None,
                        request_id: Some(target.request_id),
                        timestamp: at,
                    });
                    Ok(vec![app.outbox.audit(tx, &ev).await?])
                })
            })
            .await?;
        fire(wakes);
        Ok(())
    }
}
