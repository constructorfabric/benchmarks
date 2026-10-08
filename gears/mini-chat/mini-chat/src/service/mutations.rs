//! Turn mutations: retry, edit and delete of the latest terminal turn
//! (DESIGN §3.9).

use std::sync::Arc;

use mini_chat_sdk::{MiniChatAuditEvent, TurnMutationAuditEvent};
use sea_orm::sea_query::Expr;
use sea_orm::{ColumnTrait, Condition, EntityTrait, Order, Set};
use time::OffsetDateTime;
use toolkit_db::outbox::Wake;
use toolkit_db::secure::{
    AccessScope, DBRunner, SecureDeleteExt, SecureEntityExt, SecureUpdateExt, secure_insert,
};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use super::AppState;
use super::quota::reserve_in_tx;
use super::stream::{SetupInputs, StreamStart, TurnPlan, validate_content};
use crate::domain::authz::{self, actions};
use crate::domain::error::{DomainError, DomainResult, Res};
use crate::infra::db::entity::{
    attachment, chat, chat_turn, message, message_attachment, thread_summary,
};
use crate::infra::repo::{self, now_utc};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MutationKind {
    Retry,
    Edit,
    Delete,
}

impl MutationKind {
    fn action(self) -> &'static str {
        match self {
            Self::Retry => actions::RETRY_TURN,
            Self::Edit => actions::EDIT_TURN,
            Self::Delete => actions::DELETE_TURN,
        }
    }

    fn audit_type(self) -> &'static str {
        match self {
            Self::Retry => "turn_retry",
            Self::Edit => "turn_edit",
            Self::Delete => "turn_delete",
        }
    }
}

fn not_latest() -> DomainError {
    DomainError::aborted(
        "NOT_LATEST_TURN",
        "Only the latest turn of the chat can be modified",
    )
}

fn generation_in_progress() -> DomainError {
    DomainError::aborted(
        "GENERATION_IN_PROGRESS",
        "A generation is already running for this chat",
    )
}

fn turn_running() -> DomainError {
    DomainError::precondition(
        Res::Turn,
        "turn_state",
        "STATE",
        "The turn is still running; wait for it to finish",
    )
}

/// Validated mutation target.
struct Preview {
    chat: chat::Model,
    turn: chat_turn::Model,
    scope: AccessScope,
    user_message: Option<message::Model>,
    attachments: Vec<attachment::Model>,
}

/// Read-only validation of a mutation (rules 1–3).
async fn preview(
    state: &AppState,
    ctx: &SecurityContext,
    chat_id: Uuid,
    request_id: Uuid,
    kind: MutationKind,
) -> DomainResult<Preview> {
    let scopes = authz::chat_scopes(&state.enforcer, ctx, kind.action(), Some(chat_id)).await?;
    let conn = state.conn()?;
    let chat = repo::find_chat(&conn, &scopes.owner, chat_id)
        .await?
        .ok_or_else(|| DomainError::not_found(Res::Chat, chat_id))?;
    let turn = repo::find_turn_by_request(&conn, &scopes.tenant, chat_id, request_id)
        .await?
        .ok_or_else(|| DomainError::not_found(Res::Turn, request_id))?;
    if turn.deleted_at.is_some() {
        return Err(not_latest());
    }
    if turn.state == "running" {
        return Err(turn_running());
    }
    let latest = repo::latest_turn(&conn, &scopes.tenant, chat_id).await?;
    if latest.as_ref().map(|t| t.id) != Some(turn.id) {
        return Err(not_latest());
    }
    if turn.requester_user_id != Some(ctx.subject_id()) {
        return Err(DomainError::forbidden());
    }
    let user_message = message::Entity::find()
        .secure()
        .scope_with(&scopes.tenant)
        .filter(
            Condition::all()
                .add(message::Column::ChatId.eq(chat_id))
                .add(message::Column::RequestId.eq(request_id))
                .add(message::Column::Role.eq("user")),
        )
        .order_by(message::Column::CreatedAt, Order::Asc)
        .one(&conn)
        .await?;
    let attachments = match &user_message {
        Some(m) => {
            let mut map =
                super::messages::attachments_for_messages(&conn, &scopes.tenant, chat_id, &[m.id])
                    .await?;
            map.remove(&m.id).unwrap_or_default()
        }
        None => vec![],
    };
    Ok(Preview {
        chat,
        turn,
        scope: scopes.tenant,
        user_message,
        attachments,
    })
}

/// Soft-delete the turn and its messages; invalidate a summary covering it.
async fn soft_delete_turn(
    tx: &impl DBRunner,
    scope: &AccessScope,
    turn: &chat_turn::Model,
    user_message: Option<&message::Model>,
    replaced_by: Option<Uuid>,
    now: OffsetDateTime,
) -> DomainResult<()> {
    let res = chat_turn::Entity::update_many()
        .secure()
        .scope_with(scope)
        .col_expr(chat_turn::Column::DeletedAt, Expr::value(Some(now)))
        .col_expr(
            chat_turn::Column::ReplacedByRequestId,
            Expr::value(replaced_by),
        )
        .col_expr(chat_turn::Column::UpdatedAt, Expr::value(now))
        .filter(
            Condition::all()
                .add(chat_turn::Column::Id.eq(turn.id))
                .add(chat_turn::Column::DeletedAt.is_null())
                .add(chat_turn::Column::State.ne("running")),
        )
        .exec(tx)
        .await?;
    if res.rows_affected == 0 {
        // Lost the race to a concurrent mutation that already replaced the
        // turn and started its generation.
        if repo::find_running_turn(tx, scope, turn.chat_id)
            .await?
            .is_some()
        {
            return Err(generation_in_progress());
        }
        return Err(not_latest());
    }
    // Still the latest turn? (a concurrent send may have inserted a newer one)
    let latest = repo::latest_turn(tx, scope, turn.chat_id).await?;
    if latest.is_some_and(|t| (t.started_at, t.id) > (turn.started_at, turn.id)) {
        return Err(not_latest());
    }
    message::Entity::update_many()
        .secure()
        .scope_with(scope)
        .col_expr(message::Column::DeletedAt, Expr::value(Some(now)))
        .filter(
            Condition::all()
                .add(message::Column::ChatId.eq(turn.chat_id))
                .add(message::Column::RequestId.eq(turn.request_id))
                .add(message::Column::DeletedAt.is_null()),
        )
        .exec(tx)
        .await?;
    // Summary invalidation.
    if let Some(s) = repo::find_summary(tx, scope, turn.chat_id).await? {
        let covers = match user_message {
            Some(m) => {
                (s.summarized_up_to_created_at, s.summarized_up_to_message_id)
                    >= (m.created_at, m.id)
            }
            None => s.summarized_up_to_created_at >= turn.started_at,
        };
        if covers {
            thread_summary::Entity::delete_many()
                .secure()
                .scope_with(scope)
                .filter(Condition::all().add(thread_summary::Column::ChatId.eq(turn.chat_id)))
                .exec(tx)
                .await?;
            message::Entity::update_many()
                .secure()
                .scope_with(scope)
                .col_expr(message::Column::IsCompressed, Expr::value(false))
                .filter(Condition::all().add(message::Column::ChatId.eq(turn.chat_id)))
                .exec(tx)
                .await?;
        }
    }
    Ok(())
}

fn setup_failure_code(e: &DomainError) -> &'static str {
    match e {
        DomainError::OutOfRange { reason, .. }
            if reason == "CONTEXT_BUDGET_EXCEEDED" || reason == "INPUT_TOO_LONG" =>
        {
            "context_length_exceeded"
        }
        DomainError::ResourceExhausted { .. } => "quota_exceeded",
        _ => "turn_setup_failed",
    }
}

impl AppState {
    /// `DELETE /v1/chats/{id}/turns/{request_id}`.
    ///
    /// # Errors
    /// Returns the domain error of the step that failed (validation,
    /// authorization, persistence or a downstream dependency).
    pub async fn delete_turn(
        &self,
        ctx: &SecurityContext,
        chat_id: Uuid,
        request_id: Uuid,
    ) -> DomainResult<()> {
        let p = Arc::new(preview(self, ctx, chat_id, request_id, MutationKind::Delete).await?);
        let outbox = self.outbox.get().await?;
        let tenant = ctx.subject_tenant_id();
        let actor = ctx.subject_id();
        let audit_queue = self.cfg.outbox.audit_queue_name.clone();
        let partitions = self.cfg.outbox.num_partitions;
        let wake: Wake = self
            .write_tx(move |tx| {
                let p = Arc::clone(&p);
                let outbox = Arc::clone(&outbox);
                let audit_queue = audit_queue.clone();
                Box::pin(async move {
                    let now = now_utc();
                    soft_delete_turn(tx, &p.scope, &p.turn, p.user_message.as_ref(), None, now)
                        .await?;
                    let ev = MiniChatAuditEvent::TurnMutation(TurnMutationAuditEvent {
                        event_type: MutationKind::Delete.audit_type().into(),
                        tenant_id: tenant,
                        actor_user_id: actor,
                        chat_id: p.chat.id,
                        request_id: Some(p.turn.request_id),
                        original_request_id: None,
                        new_request_id: None,
                        timestamp: OffsetDateTime::now_utc(),
                    });
                    Ok(super::outbox::enqueue_json(
                        &outbox,
                        tx,
                        &audit_queue,
                        super::outbox::partition_for(tenant, partitions),
                        super::outbox::PT_AUDIT,
                        &ev,
                    )
                    .await?)
                })
            })
            .await?;
        wake.fire();
        Ok(())
    }

    /// `POST /turns/{request_id}/retry` and `PATCH /turns/{request_id}`.
    ///
    /// # Errors
    /// Returns the domain error of the step that failed (validation,
    /// authorization, persistence or a downstream dependency).
    #[allow(clippy::too_many_lines)]
    pub async fn mutate_and_stream(
        self: &Arc<Self>,
        ctx: SecurityContext,
        chat_id: Uuid,
        request_id: Uuid,
        new_content: Option<String>,
    ) -> DomainResult<StreamStart> {
        let kind = if new_content.is_some() {
            MutationKind::Edit
        } else {
            MutationKind::Retry
        };
        if let Some(c) = &new_content {
            validate_content(c)?;
        }
        let p = preview(self, &ctx, chat_id, request_id, kind).await?;
        let content = match &new_content {
            Some(c) => c.clone(),
            None => p
                .user_message
                .as_ref()
                .map(|m| m.content.clone())
                .ok_or_else(|| DomainError::internal("turn without a user message"))?,
        };
        let web_search = p.turn.web_search_enabled;
        // Step 2: mutation preflight (no writes).
        let snapshot = self.chat_model_snapshot(&ctx, &p.chat).await?;
        let new_request_id = Uuid::new_v4();
        let new_turn_id = Uuid::now_v7();
        self.turn_guards(&SetupInputs {
            ctx: &ctx,
            chat: &p.chat,
            tenant_scope: &p.scope,
            snapshot: snapshot.clone(),
            content: &content,
            message_attachments: p.attachments.clone(),
            web_search,
            request_id: new_request_id,
            turn_id: new_turn_id,
            exclude_request_ids: vec![request_id],
        })
        .await?;

        // Step 3: mutation commit.
        let outbox = self.outbox.get().await?;
        let tenant = ctx.subject_tenant_id();
        let actor = ctx.subject_id();
        let user_message_id = Uuid::now_v7();
        let p = Arc::new(p);
        let content_c = content.clone();
        let audit_queue = self.cfg.outbox.audit_queue_name.clone();
        let partitions = self.cfg.outbox.num_partitions;
        let res = self
            .write_tx({
                let p = Arc::clone(&p);
                move |tx| {
                    let p = Arc::clone(&p);
                    let outbox = Arc::clone(&outbox);
                    let content = content_c.clone();
                    let audit_queue = audit_queue.clone();
                    Box::pin(async move {
                        let now = now_utc();
                        soft_delete_turn(
                            tx,
                            &p.scope,
                            &p.turn,
                            p.user_message.as_ref(),
                            Some(new_request_id),
                            now,
                        )
                        .await?;
                        let created = repo::next_message_time(tx, &p.scope, p.chat.id).await?;
                        let am = message::ActiveModel {
                            id: Set(user_message_id),
                            tenant_id: Set(tenant),
                            chat_id: Set(p.chat.id),
                            request_id: Set(Some(new_request_id)),
                            role: Set("user".to_owned()),
                            content: Set(content),
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
                            created_at: Set(created),
                            deleted_at: Set(None),
                        };
                        secure_insert::<message::Entity>(am, &p.scope, tx).await?;
                        // Copy non-deleted attachment links.
                        for a in &p.attachments {
                            if a.deleted_at.is_some() {
                                continue;
                            }
                            let link = message_attachment::ActiveModel {
                                tenant_id: Set(tenant),
                                chat_id: Set(p.chat.id),
                                message_id: Set(user_message_id),
                                attachment_id: Set(a.id),
                                created_at: Set(created),
                            };
                            secure_insert::<message_attachment::Entity>(link, &p.scope, tx).await?;
                        }
                        let turn = chat_turn::ActiveModel {
                            id: Set(new_turn_id),
                            tenant_id: Set(tenant),
                            chat_id: Set(p.chat.id),
                            request_id: Set(new_request_id),
                            requester_type: Set("user".to_owned()),
                            requester_user_id: Set(Some(actor)),
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
                        if let Err(e) = secure_insert::<chat_turn::Entity>(turn, &p.scope, tx).await
                        {
                            let e = DomainError::from(e);
                            if e.is_unique_violation() {
                                return Err(generation_in_progress());
                            }
                            return Err(e);
                        }
                        repo::touch_chat(tx, &p.scope, p.chat.id, created).await?;
                        let ev = MiniChatAuditEvent::TurnMutation(TurnMutationAuditEvent {
                            event_type: kind.audit_type().into(),
                            tenant_id: tenant,
                            actor_user_id: actor,
                            chat_id: p.chat.id,
                            request_id: None,
                            original_request_id: Some(p.turn.request_id),
                            new_request_id: Some(new_request_id),
                            timestamp: OffsetDateTime::now_utc(),
                        });
                        let w = super::outbox::enqueue_json(
                            &outbox,
                            tx,
                            &audit_queue,
                            super::outbox::partition_for(tenant, partitions),
                            super::outbox::PT_AUDIT,
                            &ev,
                        )
                        .await?;
                        Ok(w)
                    })
                }
            })
            .await;
        let wake = match res {
            Ok(w) => w,
            Err(e) if e.is_unique_violation() => return Err(generation_in_progress()),
            Err(e) => return Err(e),
        };
        wake.fire();

        // Step 4: context assembly, provider resolution, reserve.
        match self
            .setup_mutated_turn(
                &ctx,
                &p,
                snapshot,
                &content,
                new_request_id,
                new_turn_id,
                user_message_id,
                web_search,
            )
            .await
        {
            Ok(plan) => Ok(self.launch(plan)),
            Err(e) => {
                let code = setup_failure_code(&e);
                self.fail_unstarted_turn(new_turn_id, tenant, code).await;
                Err(e)
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn setup_mutated_turn(
        &self,
        ctx: &SecurityContext,
        p: &Preview,
        snapshot: mini_chat_sdk::PolicySnapshot,
        content: &str,
        request_id: Uuid,
        turn_id: Uuid,
        user_message_id: Uuid,
        web_search: bool,
    ) -> DomainResult<TurnPlan> {
        let attachments: Vec<attachment::Model> = p
            .attachments
            .iter()
            .filter(|a| a.deleted_at.is_none())
            .cloned()
            .collect();
        let mut plan = self
            .plan_turn(SetupInputs {
                ctx,
                chat: &p.chat,
                tenant_scope: &p.scope,
                snapshot,
                content,
                message_attachments: attachments,
                web_search,
                request_id,
                turn_id,
                exclude_request_ids: vec![request_id],
            })
            .await?;
        plan.user_message_id = user_message_id;
        let plan_c = plan.clone();
        self.write_tx(move |tx| {
            let plan = plan_c.clone();
            Box::pin(async move {
                let scope = AccessScope::for_tenant(plan.tenant_id);
                reserve_in_tx(
                    tx,
                    &scope,
                    plan.tenant_id,
                    plan.user_id,
                    &plan.periods,
                    plan.premium,
                    plan.reserve.reserved_credits_micro,
                    &plan.limits,
                )
                .await?;
                let r = &plan.reserve;
                let res = chat_turn::Entity::update_many()
                    .secure()
                    .scope_with(&scope)
                    .col_expr(
                        chat_turn::Column::ReserveTokens,
                        Expr::value(Some(r.reserve_tokens)),
                    )
                    .col_expr(
                        chat_turn::Column::MaxOutputTokensApplied,
                        Expr::value(Some(
                            i32::try_from(r.max_output_tokens_applied).unwrap_or(i32::MAX),
                        )),
                    )
                    .col_expr(
                        chat_turn::Column::ReservedCreditsMicro,
                        Expr::value(Some(r.reserved_credits_micro)),
                    )
                    .col_expr(
                        chat_turn::Column::PolicyVersionApplied,
                        Expr::value(Some(i64::try_from(plan.policy_version).unwrap_or(i64::MAX))),
                    )
                    .col_expr(
                        chat_turn::Column::EffectiveModel,
                        Expr::value(Some(plan.effective.id.clone())),
                    )
                    .col_expr(
                        chat_turn::Column::MinimalGenerationFloorApplied,
                        Expr::value(Some(
                            i32::try_from(r.minimal_generation_floor_applied).unwrap_or(i32::MAX),
                        )),
                    )
                    .filter(
                        Condition::all()
                            .add(chat_turn::Column::Id.eq(plan.turn_id))
                            .add(chat_turn::Column::State.eq("running")),
                    )
                    .exec(tx)
                    .await?;
                if res.rows_affected == 0 {
                    return Err(DomainError::internal("mutated turn is no longer running"));
                }
                Ok(())
            })
        })
        .await?;
        Ok(plan)
    }
}
