//! Turn status and tail-only turn mutations (DESIGN §3.9).

use std::sync::Arc;
use std::time::Instant;

use mini_chat_sdk::audit::{TurnMutationAuditEvent, TurnMutationEventType};
use mini_chat_sdk::AuditEvent;
use sea_orm::sea_query::Expr;
use sea_orm::{ColumnTrait, Condition, EntityTrait, QueryFilter, QueryOrder, QuerySelect, Set};
use serde_json::json;
use toolkit_db::outbox::Wake;
use toolkit_db::secure::{AccessScope, DBRunner, SecureDeleteExt, SecureEntityExt, SecureUpdateExt, secure_insert};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use super::authz::actions;
use super::billing::{TurnState, codes};
use super::clock::{self, Timestamp};
use super::error::DomainError;
use super::quota::QuotaInputs;
use super::service::{ChatAccess, MiniChat};
use super::stream::{CommittedTurn, load_chat_facts};
use super::stream_types::StreamStart;
use crate::infra::outbox::{OutboxKind, enqueue_json};
use crate::infra::storage::entity::{attachment, chat, chat_turn, message, message_attachment, thread_summary};

/// Retry or edit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MutationKind {
    Retry,
    Edit,
}

impl MutationKind {
    const fn action(self) -> &'static str {
        match self {
            Self::Retry => actions::RETRY_TURN,
            Self::Edit => actions::EDIT_TURN,
        }
    }

    const fn label(self) -> &'static str {
        match self {
            Self::Retry => "retry",
            Self::Edit => "edit",
        }
    }
}

/// Latest non-deleted turn of a chat: max `(started_at, id)`.
///
/// # Errors
/// DB errors.
pub async fn latest_turn(runner: &impl DBRunner, scope: &AccessScope, chat_id: Uuid) -> Result<Option<chat_turn::Model>, DomainError> {
    Ok(chat_turn::Entity::find()
        .filter(chat_turn::Column::ChatId.eq(chat_id))
        .filter(chat_turn::Column::DeletedAt.is_null())
        .order_by_desc(chat_turn::Column::StartedAt)
        .order_by_desc(chat_turn::Column::Id)
        .limit(1)
        .secure()
        .scope_with(scope)
        .one(runner)
        .await?)
}

async fn find_turn(runner: &impl DBRunner, scope: &AccessScope, chat_id: Uuid, request_id: Uuid) -> Result<Option<chat_turn::Model>, DomainError> {
    Ok(chat_turn::Entity::find()
        .filter(chat_turn::Column::ChatId.eq(chat_id))
        .filter(chat_turn::Column::RequestId.eq(request_id))
        .secure()
        .scope_with(scope)
        .one(runner)
        .await?)
}

/// Mutation eligibility (rules 1–3) on an already loaded turn.
fn check_target(turn: &chat_turn::Model, latest: Option<&chat_turn::Model>, caller: Uuid) -> Result<(), DomainError> {
    if turn.deleted_at.is_some() {
        return Err(DomainError::NotLatestTurn);
    }
    if turn.state == TurnState::Running.as_str() {
        return Err(DomainError::TurnNotTerminal);
    }
    if latest.is_none_or(|l| l.id != turn.id) {
        return Err(DomainError::NotLatestTurn);
    }
    if turn.requester_user_id != Some(caller) {
        return Err(DomainError::Forbidden);
    }
    Ok(())
}

/// Soft-delete a turn's messages and invalidate a covering summary.
async fn soft_delete_turn_content(
    tx: &impl DBRunner,
    child: &AccessScope,
    chat_id: Uuid,
    request_id: Uuid,
    user_msg: Option<&message::Model>,
    now: Timestamp,
) -> Result<(), DomainError> {
    message::Entity::update_many()
        .secure()
        .col_expr(message::Column::DeletedAt, Expr::value(Some(now)))
        .filter(
            Condition::all()
                .add(message::Column::ChatId.eq(chat_id))
                .add(message::Column::RequestId.eq(request_id))
                .add(message::Column::DeletedAt.is_null()),
        )
        .scope_with(child)
        .exec(tx)
        .await?;
    let Some(um) = user_msg else {
        return Ok(());
    };
    let summary = thread_summary::Entity::find()
        .filter(thread_summary::Column::ChatId.eq(chat_id))
        .secure()
        .scope_with(child)
        .one(tx)
        .await?;
    if let Some(s) = summary
        && (s.summarized_up_to_created_at, s.summarized_up_to_message_id) >= (um.created_at, um.id)
    {
        thread_summary::Entity::delete_many()
            .secure()
            .scope_with(child)
            .filter(Condition::all().add(thread_summary::Column::Id.eq(s.id)))
            .exec(tx)
            .await?;
        message::Entity::update_many()
            .secure()
            .col_expr(message::Column::IsCompressed, Expr::value(false))
            .filter(Condition::all().add(message::Column::ChatId.eq(chat_id)).add(message::Column::IsCompressed.eq(true)))
            .scope_with(child)
            .exec(tx)
            .await?;
    }
    Ok(())
}

async fn turn_user_message(runner: &impl DBRunner, scope: &AccessScope, chat_id: Uuid, request_id: Uuid) -> Result<Option<message::Model>, DomainError> {
    Ok(message::Entity::find()
        .filter(message::Column::ChatId.eq(chat_id))
        .filter(message::Column::RequestId.eq(request_id))
        .filter(message::Column::Role.eq("user"))
        .order_by_desc(message::Column::CreatedAt)
        .limit(1)
        .secure()
        .scope_with(scope)
        .one(runner)
        .await?)
}

impl MiniChat {
    /// `GET /v1/chats/{id}/turns/{request_id}`.
    ///
    /// # Errors
    /// `ChatNotFound`, `TurnNotFound` (also for soft-deleted turns).
    pub async fn get_turn(&self, ctx: &SecurityContext, chat_id: Uuid, request_id: Uuid) -> Result<chat_turn::Model, DomainError> {
        let access = self.load_chat(ctx, actions::READ_TURN, chat_id).await?;
        let conn = self.db.conn()?;
        find_turn(&conn, &access.child_scope, chat_id, request_id)
            .await?
            .filter(|t| t.deleted_at.is_none())
            .ok_or(DomainError::TurnNotFound(request_id))
    }

    async fn mutation_target(
        &self,
        ctx: &SecurityContext,
        action: &str,
        chat_id: Uuid,
        request_id: Uuid,
    ) -> Result<(ChatAccess, chat_turn::Model), DomainError> {
        let access = self.load_chat(ctx, action, chat_id).await?;
        let conn = self.db.conn()?;
        let turn = find_turn(&conn, &access.child_scope, chat_id, request_id)
            .await?
            .ok_or(DomainError::TurnNotFound(request_id))?;
        let latest = latest_turn(&conn, &access.child_scope, chat_id).await?;
        check_target(&turn, latest.as_ref(), ctx.subject_id())?;
        Ok((access, turn))
    }

    /// `DELETE /v1/chats/{id}/turns/{request_id}`.
    ///
    /// # Errors
    /// Mutation eligibility errors, DB errors.
    pub async fn delete_turn(&self, ctx: &SecurityContext, chat_id: Uuid, request_id: Uuid) -> Result<(), DomainError> {
        let started = Instant::now();
        let res = self.delete_turn_inner(ctx, chat_id, request_id).await;
        self.metrics.turn_mutation(
            "delete",
            if res.is_ok() { "ok" } else { "error" },
            started.elapsed().as_secs_f64() * 1000.0,
        );
        res
    }

    async fn delete_turn_inner(&self, ctx: &SecurityContext, chat_id: Uuid, request_id: Uuid) -> Result<(), DomainError> {
        let (access, turn) = self.mutation_target(ctx, actions::DELETE_TURN, chat_id, request_id).await?;
        let child = access.child_scope.clone();
        let slot = self.outbox.clone();
        let caller = ctx.subject_id();
        let tenant_id = access.chat.tenant_id;
        let wake: Wake = self
            .tx(move |tx| {
                Box::pin(async move {
                    let now = clock::now();
                    let latest = latest_turn(tx, &child, chat_id).await?;
                    let fresh = find_turn(tx, &child, chat_id, request_id).await?.ok_or(DomainError::TurnNotFound(request_id))?;
                    check_target(&fresh, latest.as_ref(), caller)?;
                    let rows = chat_turn::Entity::update_many()
                        .secure()
                        .col_expr(chat_turn::Column::DeletedAt, Expr::value(Some(now)))
                        .col_expr(chat_turn::Column::UpdatedAt, Expr::value(now))
                        .filter(
                            Condition::all()
                                .add(chat_turn::Column::Id.eq(turn.id))
                                .add(chat_turn::Column::DeletedAt.is_null())
                                .add(chat_turn::Column::State.ne("running")),
                        )
                        .scope_with(&child)
                        .exec(tx)
                        .await?
                        .rows_affected;
                    if rows == 0 {
                        return Err(DomainError::NotLatestTurn);
                    }
                    let um = turn_user_message(tx, &child, chat_id, request_id).await?;
                    soft_delete_turn_content(tx, &child, chat_id, request_id, um.as_ref(), now).await?;
                    let ev = AuditEvent::TurnMutation(TurnMutationAuditEvent {
                        event_type: TurnMutationEventType::TurnDelete,
                        tenant_id,
                        actor_user_id: caller,
                        chat_id,
                        original_request_id: None,
                        new_request_id: None,
                        request_id: Some(request_id),
                        timestamp: clock::to_time(now),
                    });
                    enqueue_json(&slot, tx, OutboxKind::Audit, chat_id, &ev).await
                })
            })
            .await?;
        wake.fire();
        Ok(())
    }

    /// `POST .../turns/{request_id}/retry` and `PATCH .../turns/{request_id}`.
    ///
    /// # Errors
    /// Mutation eligibility, preflight and setup errors.
    pub async fn mutate_turn(
        self: &Arc<Self>,
        ctx: &SecurityContext,
        chat_id: Uuid,
        request_id: Uuid,
        kind: MutationKind,
        new_content: Option<String>,
    ) -> Result<StreamStart, DomainError> {
        let started = Instant::now();
        let res = self.mutate_turn_inner(ctx, chat_id, request_id, kind, new_content).await;
        self.metrics.turn_mutation(
            kind.label(),
            if res.is_ok() { "ok" } else { "error" },
            started.elapsed().as_secs_f64() * 1000.0,
        );
        res
    }

    #[allow(clippy::too_many_lines, clippy::cognitive_complexity, reason = "ordered mutation pipeline")]
    async fn mutate_turn_inner(
        self: &Arc<Self>,
        ctx: &SecurityContext,
        chat_id: Uuid,
        request_id: Uuid,
        kind: MutationKind,
        new_content: Option<String>,
    ) -> Result<StreamStart, DomainError> {
        if kind == MutationKind::Edit && new_content.as_deref().is_none_or(|c| c.trim().is_empty()) {
            // Authorization still runs first so an unknown chat stays 404.
            self.load_chat(ctx, kind.action(), chat_id).await?;
            return Err(DomainError::EmptyContent);
        }
        let (access, old) = self.mutation_target(ctx, kind.action(), chat_id, request_id).await?;
        let policy = self.policy.current(ctx.subject_id()).await?;
        Self::check_chat_model(&policy, &access.chat)?;
        let conn = self.db.conn()?;
        let old_user = turn_user_message(&conn, &access.child_scope, chat_id, request_id).await?;
        let content = match (kind, new_content) {
            (MutationKind::Edit, Some(c)) => c,
            _ => old_user.as_ref().map(|m| m.content.clone()).unwrap_or_default(),
        };
        if content.trim().is_empty() {
            return Err(DomainError::EmptyContent);
        }
        // Live attachments of the original user message.
        let mut live_attachments: Vec<attachment::Model> = Vec::new();
        if let Some(um) = &old_user {
            let links = message_attachment::Entity::find()
                .filter(message_attachment::Column::ChatId.eq(chat_id))
                .filter(message_attachment::Column::MessageId.eq(um.id))
                .secure()
                .scope_with(&access.child_scope)
                .all(&conn)
                .await?;
            if !links.is_empty() {
                let ids: Vec<Uuid> = links.iter().map(|l| l.attachment_id).collect();
                live_attachments = attachment::Entity::find()
                    .filter(attachment::Column::ChatId.eq(chat_id))
                    .filter(attachment::Column::Id.is_in(ids))
                    .filter(attachment::Column::DeletedAt.is_null())
                    .secure()
                    .scope_with(&access.child_scope)
                    .all(&conn)
                    .await?;
            }
        }
        let images: Vec<attachment::Model> = live_attachments
            .iter()
            .filter(|a| a.attachment_kind == "image" && a.status == "ready")
            .cloned()
            .collect();
        if images.len() > self.cfg.rag.max_images_per_message as usize {
            return Err(DomainError::TooManyImages { count: images.len(), max: self.cfg.rag.max_images_per_message });
        }
        let web_search = old.web_search_enabled;
        if web_search && policy.snapshot.kill_switches.disable_web_search {
            return Err(DomainError::FeatureDisabled("web_search"));
        }
        let facts = load_chat_facts(&conn, &access.child_scope, access.chat.tenant_id, chat_id).await?;
        let limits = self.policy.user_limits(ctx.subject_id(), policy.version).await?;
        let inputs = QuotaInputs {
            message_bytes: content.len(),
            prior_context_tokens: facts.prior_context_tokens,
            images: u32::try_from(images.len()).unwrap_or(u32::MAX),
            has_ready_docs: facts.has_ready_docs,
            has_ready_code_files: !facts.ci_file_ids.is_empty(),
            web_search_requested: web_search,
        };
        let decision = self
            .quota()
            .preflight(&conn, &access.scope, access.chat.tenant_id, ctx.subject_id(), &access.chat.model, &policy, limits, &inputs)
            .await;
        let decision = match decision {
            Ok(d) => d,
            Err(e) => {
                self.metrics.quota_preflight("reject", &access.chat.model, "none");
                return Err(e);
            }
        };
        self.metrics.quota_preflight(decision.quota_decision(), &decision.effective.id, decision.effective.tier.as_str());
        #[allow(clippy::drop_non_drop, reason = "explicitly release the DB conn handle before further awaits/transactions")]
        drop(conn);
        Self::post_cascade_guards(&policy, &decision, &content, images.len())?;

        // Mutation commit.
        let new_request = Uuid::new_v4();
        let new_turn_id = Uuid::now_v7();
        let new_user_msg = Uuid::now_v7();
        let child = access.child_scope.clone();
        let scope = access.scope.clone();
        let slot = self.outbox.clone();
        let caller = ctx.subject_id();
        let tenant_id = access.chat.tenant_id;
        let old_id = old.id;
        let copy_ids: Vec<Uuid> = live_attachments.iter().map(|a| a.id).collect();
        let content_tx = content.clone();
        let event_type = match kind {
            MutationKind::Retry => TurnMutationEventType::TurnRetry,
            MutationKind::Edit => TurnMutationEventType::TurnEdit,
        };
        let committed = self
            .tx(move |tx| {
                Box::pin(async move {
                    let now = clock::now();
                    let latest = latest_turn(tx, &child, chat_id).await?;
                    let fresh = find_turn(tx, &child, chat_id, request_id).await?.ok_or(DomainError::TurnNotFound(request_id))?;
                    check_target(&fresh, latest.as_ref(), caller)?;
                    let rows = chat_turn::Entity::update_many()
                        .secure()
                        .col_expr(chat_turn::Column::DeletedAt, Expr::value(Some(now)))
                        .col_expr(chat_turn::Column::ReplacedByRequestId, Expr::value(Some(new_request)))
                        .col_expr(chat_turn::Column::UpdatedAt, Expr::value(now))
                        .filter(
                            Condition::all()
                                .add(chat_turn::Column::Id.eq(old_id))
                                .add(chat_turn::Column::DeletedAt.is_null())
                                .add(chat_turn::Column::State.ne("running")),
                        )
                        .scope_with(&child)
                        .exec(tx)
                        .await?
                        .rows_affected;
                    if rows == 0 {
                        return Err(DomainError::GenerationInProgress);
                    }
                    let um = turn_user_message(tx, &child, chat_id, request_id).await?;
                    soft_delete_turn_content(tx, &child, chat_id, request_id, um.as_ref(), now).await?;
                    let msg = message::ActiveModel {
                        id: Set(new_user_msg),
                        tenant_id: Set(tenant_id),
                        chat_id: Set(chat_id),
                        request_id: Set(Some(new_request)),
                        role: Set("user".to_owned()),
                        content: Set(content_tx),
                        content_type: Set("text".to_owned()),
                        token_estimate: Set(0),
                        provider_response_id: Set(None),
                        request_kind: Set("chat".to_owned()),
                        features_used: Set(json!([])),
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
                    secure_insert::<message::Entity>(msg, &child, tx).await?;
                    for aid in &copy_ids {
                        let link = message_attachment::ActiveModel {
                            tenant_id: Set(tenant_id),
                            chat_id: Set(chat_id),
                            message_id: Set(new_user_msg),
                            attachment_id: Set(*aid),
                            created_at: Set(now),
                        };
                        secure_insert::<message_attachment::Entity>(link, &child, tx).await?;
                    }
                    let turn = chat_turn::ActiveModel {
                        id: Set(new_turn_id),
                        tenant_id: Set(tenant_id),
                        chat_id: Set(chat_id),
                        request_id: Set(new_request),
                        requester_type: Set("user".to_owned()),
                        requester_user_id: Set(Some(caller)),
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
                        started_at: Set(now),
                        last_progress_at: Set(Some(now)),
                        web_search_enabled: Set(web_search),
                        web_search_completed_count: Set(0),
                        code_interpreter_completed_count: Set(0),
                        file_search_completed_count: Set(0),
                        completed_at: Set(None),
                        updated_at: Set(now),
                    };
                    secure_insert::<chat_turn::Entity>(turn, &child, tx).await.map_err(|e| match DomainError::from(e) {
                        DomainError::UniqueViolation(_) => DomainError::GenerationInProgress,
                        other => other,
                    })?;
                    chat::Entity::update_many()
                        .secure()
                        .col_expr(chat::Column::UpdatedAt, Expr::value(now))
                        .filter(Condition::all().add(chat::Column::Id.eq(chat_id)))
                        .scope_with(&scope)
                        .exec(tx)
                        .await?;
                    let ev = AuditEvent::TurnMutation(TurnMutationAuditEvent {
                        event_type,
                        tenant_id,
                        actor_user_id: caller,
                        chat_id,
                        original_request_id: Some(request_id),
                        new_request_id: Some(new_request),
                        request_id: None,
                        timestamp: clock::to_time(now),
                    });
                    let wake = enqueue_json(&slot, tx, OutboxKind::Audit, chat_id, &ev).await?;
                    Ok((wake, CommittedTurn { turn_id: new_turn_id, user_message_id: new_user_msg, user_message_created_at: now, request_id: new_request }))
                })
            })
            .await;
        let (wake, committed) = match committed {
            Ok(v) => v,
            Err(DomainError::UniqueViolation(_)) => return Err(DomainError::GenerationInProgress),
            Err(e) => return Err(e),
        };
        wake.fire();

        // Post-commit setup: context assembly, provider resolution, reserve.
        let prepared = match self
            .prepare_turn(ctx, &access, &decision, &facts, committed.request_id, &content, &images)
            .await
        {
            Ok(p) => p,
            Err(e) => {
                let code = if matches!(e, DomainError::ContextBudgetExceeded(_)) {
                    codes::CONTEXT_LENGTH_EXCEEDED
                } else {
                    codes::TURN_SETUP_FAILED
                };
                self.fail_unstarted(&access, committed.turn_id, code).await;
                return Err(e);
            }
        };
        let quota = self.quota();
        let scope = access.scope.clone();
        let child = access.child_scope.clone();
        let d = decision.clone();
        let user_id = ctx.subject_id();
        let turn_id = committed.turn_id;
        let reserved = self
            .tx(move |tx| {
                Box::pin(async move {
                    quota.reserve(tx, &scope, tenant_id, user_id, &d).await?;
                    let rows = chat_turn::Entity::update_many()
                        .secure()
                        .col_expr(chat_turn::Column::ReserveTokens, Expr::value(Some(d.reserve.reserve_tokens)))
                        .col_expr(
                            chat_turn::Column::MaxOutputTokensApplied,
                            Expr::value(Some(i32::try_from(d.reserve.max_output_tokens_applied).unwrap_or(i32::MAX))),
                        )
                        .col_expr(chat_turn::Column::ReservedCreditsMicro, Expr::value(Some(d.reserved_credits_micro)))
                        .col_expr(
                            chat_turn::Column::PolicyVersionApplied,
                            Expr::value(Some(i64::try_from(d.policy_version).unwrap_or(i64::MAX))),
                        )
                        .col_expr(chat_turn::Column::EffectiveModel, Expr::value(Some(d.effective.id.clone())))
                        .col_expr(
                            chat_turn::Column::MinimalGenerationFloorApplied,
                            Expr::value(Some(i32::try_from(d.floor_applied).unwrap_or(i32::MAX))),
                        )
                        .col_expr(chat_turn::Column::LastProgressAt, Expr::value(Some(clock::now())))
                        .filter(
                            Condition::all()
                                .add(chat_turn::Column::Id.eq(turn_id))
                                .add(chat_turn::Column::State.eq("running"))
                                .add(chat_turn::Column::ReserveTokens.is_null()),
                        )
                        .scope_with(&child)
                        .exec(tx)
                        .await?
                        .rows_affected;
                    if rows == 0 {
                        return Err(DomainError::Internal("turn left running state before the reserve".to_owned()));
                    }
                    Ok(())
                })
            })
            .await;
        if let Err(e) = reserved {
            let code = if matches!(e, DomainError::QuotaExceeded(_)) { codes::QUOTA_EXCEEDED } else { codes::TURN_SETUP_FAILED };
            self.fail_unstarted(&access, turn_id, code).await;
            return Err(e);
        }
        self.metrics.quota_reserve();
        Ok(StreamStart::Live(self.start_live(ctx, &access, decision, prepared, committed)))
    }

    /// Plain CAS to `failed` for an unstarted retry/edit turn (no reserve,
    /// no settlement, no outbox event).
    async fn fail_unstarted(&self, access: &ChatAccess, turn_id: Uuid, code: &str) {
        let Ok(conn) = self.db.conn() else {
            return;
        };
        let now = clock::now();
        let res = chat_turn::Entity::update_many()
            .secure()
            .col_expr(chat_turn::Column::State, Expr::value("failed"))
            .col_expr(chat_turn::Column::ErrorCode, Expr::value(Some(code.to_owned())))
            .col_expr(chat_turn::Column::CompletedAt, Expr::value(Some(now)))
            .col_expr(chat_turn::Column::UpdatedAt, Expr::value(now))
            .filter(Condition::all().add(chat_turn::Column::Id.eq(turn_id)).add(chat_turn::Column::State.eq("running")))
            .scope_with(&access.child_scope)
            .exec(&conn)
            .await;
        if let Err(e) = res {
            tracing::error!(error = %e, %turn_id, "failed to mark unstarted turn as failed");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn turn(state: &str, user: Uuid) -> chat_turn::Model {
        let now = clock::now();
        chat_turn::Model {
            id: Uuid::now_v7(),
            tenant_id: Uuid::nil(),
            chat_id: Uuid::nil(),
            request_id: Uuid::new_v4(),
            requester_type: "user".into(),
            requester_user_id: Some(user),
            state: state.into(),
            provider_name: None,
            provider_response_id: None,
            assistant_message_id: None,
            error_code: None,
            reserve_tokens: None,
            max_output_tokens_applied: None,
            reserved_credits_micro: None,
            policy_version_applied: None,
            effective_model: None,
            minimal_generation_floor_applied: None,
            error_detail: None,
            deleted_at: None,
            replaced_by_request_id: None,
            started_at: now,
            last_progress_at: None,
            web_search_enabled: false,
            web_search_completed_count: 0,
            code_interpreter_completed_count: 0,
            file_search_completed_count: 0,
            completed_at: None,
            updated_at: now,
        }
    }

    #[test]
    fn eligibility_rules_in_order() {
        let me = Uuid::new_v4();
        let t = turn("completed", me);
        assert!(check_target(&t, Some(&t), me).is_ok());
        let running = turn("running", me);
        // Running is reported before the latest-turn check.
        assert!(matches!(check_target(&running, None, me), Err(DomainError::TurnNotTerminal)));
        let newer = turn("completed", me);
        assert!(matches!(check_target(&t, Some(&newer), me), Err(DomainError::NotLatestTurn)));
        let mut deleted = turn("completed", me);
        deleted.deleted_at = Some(clock::now());
        assert!(matches!(check_target(&deleted, Some(&t), me), Err(DomainError::NotLatestTurn)));
        assert!(matches!(check_target(&t, Some(&t), Uuid::new_v4()), Err(DomainError::Forbidden)));
    }
}
