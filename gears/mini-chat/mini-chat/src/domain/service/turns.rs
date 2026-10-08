//! Turn status and tail-only turn mutations (retry, edit, delete; DESIGN
//! §3.9).

#[allow(unused_imports)]
use sea_orm::{EntityTrait as _, QueryFilter as _};
use std::sync::Arc;
use std::time::Instant;

use mini_chat_sdk::{TurnMutationAuditEvent, TurnMutationKind};
use sea_orm::sea_query::Expr;
use sea_orm::{ColumnTrait, Condition};
use toolkit_db::DbTx;
use toolkit_db::secure::{SecureDeleteExt, SecureEntityExt, SecureUpdateExt, secure_insert};
use toolkit_security::{AccessScope, SecurityContext};
use uuid::Uuid;

use super::Service;
use super::stream::{
    NewMessage, PreflightArgs, StreamStart, TurnRun, insert_links, insert_message, new_turn,
    touch_chat,
};
use crate::domain::authz::actions;
use crate::domain::clock;
use crate::domain::error::{DomainError, DomainResult};
use crate::domain::events::PAYLOAD_MUTATION_AUDIT;
use crate::infra::outbox::{OutboxEnqueuer, fire};
use crate::infra::storage::entity::{
    attachment, chat, chat_turn, message, message_attachment, thread_summary,
};

/// Turn status for the API.
#[derive(Debug, Clone)]
pub struct TurnStatus {
    pub request_id: Uuid,
    /// `running`, `done`, `error` or `cancelled`.
    pub state: &'static str,
    pub error_code: Option<String>,
    pub assistant_message_id: Option<Uuid>,
    pub updated_at: time::OffsetDateTime,
}

/// Mutation kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mutation {
    Retry,
    Edit,
    Delete,
}

impl Mutation {
    fn action(self) -> &'static str {
        match self {
            Self::Retry => actions::RETRY_TURN,
            Self::Edit => actions::EDIT_TURN,
            Self::Delete => actions::DELETE_TURN,
        }
    }

    fn audit_kind(self) -> TurnMutationKind {
        match self {
            Self::Retry => TurnMutationKind::TurnRetry,
            Self::Edit => TurnMutationKind::TurnEdit,
            Self::Delete => TurnMutationKind::TurnDelete,
        }
    }
}

/// Map an internal turn state onto the API state.
#[must_use]
pub fn api_state(state: &str) -> &'static str {
    match state {
        "running" => "running",
        "completed" => "done",
        "failed" => "error",
        _ => "cancelled",
    }
}

/// The latest non-deleted turn of a chat.
async fn latest_turn<R: toolkit_db::secure::DBRunner>(
    runner: &R,
    scope: &AccessScope,
    chat_id: Uuid,
) -> DomainResult<Option<chat_turn::Model>> {
    Ok(chat_turn::Entity::find()
        .secure()
        .scope_with(scope)
        .filter(
            Condition::all()
                .add(chat_turn::Column::ChatId.eq(chat_id))
                .add(chat_turn::Column::DeletedAt.is_null()),
        )
        .order_by(chat_turn::Column::StartedAt, sea_orm::Order::Desc)
        .order_by(chat_turn::Column::Id, sea_orm::Order::Desc)
        .limit(1)
        .one(runner)
        .await?)
}

/// Soft-delete the latest turn (CAS) and its messages, invalidate the
/// summary, bump the chat and enqueue the mutation audit event.
#[allow(clippy::too_many_arguments)]
async fn mutate_tail(
    tx: &DbTx<'_>,
    outbox: &OutboxEnqueuer,
    scope: &AccessScope,
    chat_id: Uuid,
    old: &chat_turn::Model,
    new_request_id: Option<Uuid>,
    actor: Uuid,
    kind: Mutation,
) -> DomainResult<Vec<toolkit_db::outbox::Wake>> {
    let now = clock::now();
    let res = chat_turn::Entity::update_many()
        .col_expr(chat_turn::Column::DeletedAt, Expr::value(Some(now)))
        .col_expr(
            chat_turn::Column::ReplacedByRequestId,
            Expr::value(new_request_id),
        )
        .col_expr(chat_turn::Column::UpdatedAt, Expr::value(now))
        .filter(
            Condition::all()
                .add(chat_turn::Column::Id.eq(old.id))
                .add(chat_turn::Column::DeletedAt.is_null())
                .add(chat_turn::Column::State.ne("running")),
        )
        .secure()
        .scope_with(scope)
        .exec(tx)
        .await?;
    if res.rows_affected == 0 {
        return Err(
            if super::Service::running_turn(tx, scope, chat_id)
                .await?
                .is_some()
            {
                DomainError::GenerationInProgress
            } else {
                DomainError::NotLatestTurn
            },
        );
    }
    if let Some(latest) = latest_turn(tx, scope, chat_id).await?
        && latest.started_at > old.started_at
    {
        return Err(if latest.state == "running" {
            DomainError::GenerationInProgress
        } else {
            DomainError::NotLatestTurn
        });
    }
    // The old turn's user message (for the summary check), then soft-delete
    // the turn's messages.
    let old_user = message::Entity::find()
        .secure()
        .scope_with(scope)
        .filter(
            Condition::all()
                .add(message::Column::ChatId.eq(chat_id))
                .add(message::Column::RequestId.eq(old.request_id))
                .add(message::Column::Role.eq("user")),
        )
        .one(tx)
        .await?;
    message::Entity::update_many()
        .col_expr(message::Column::DeletedAt, Expr::value(Some(now)))
        .filter(
            Condition::all()
                .add(message::Column::ChatId.eq(chat_id))
                .add(message::Column::RequestId.eq(old.request_id))
                .add(message::Column::DeletedAt.is_null()),
        )
        .secure()
        .scope_with(scope)
        .exec(tx)
        .await?;
    if let Some(u) = old_user
        && let Some(s) = thread_summary::Entity::find()
            .secure()
            .scope_with(scope)
            .filter(Condition::all().add(thread_summary::Column::ChatId.eq(chat_id)))
            .one(tx)
            .await?
        && (s.summarized_up_to_created_at, s.summarized_up_to_message_id) >= (u.created_at, u.id)
    {
        thread_summary::Entity::delete_many()
            .filter(Condition::all().add(thread_summary::Column::ChatId.eq(chat_id)))
            .secure()
            .scope_with(scope)
            .exec(tx)
            .await?;
        message::Entity::update_many()
            .col_expr(message::Column::IsCompressed, Expr::value(false))
            .filter(Condition::all().add(message::Column::ChatId.eq(chat_id)))
            .secure()
            .scope_with(scope)
            .exec(tx)
            .await?;
    }
    touch_chat(tx, scope, chat_id, now).await?;
    let audit = TurnMutationAuditEvent {
        event_type: kind.audit_kind(),
        timestamp: now,
        tenant_id: old.tenant_id,
        actor_user_id: actor,
        chat_id,
        original_request_id: (kind != Mutation::Delete).then_some(old.request_id),
        new_request_id,
        request_id: (kind == Mutation::Delete).then_some(old.request_id),
    };
    let wake = outbox
        .enqueue_json(
            tx,
            &outbox.queues.audit_queue_name,
            old.tenant_id,
            PAYLOAD_MUTATION_AUDIT,
            &audit,
        )
        .await?;
    Ok(vec![wake])
}

impl Service {
    /// `GET /v1/chats/{id}/turns/{request_id}`.
    ///
    /// # Errors
    /// Authz errors, `ChatNotFound`, `TurnNotFound`.
    pub async fn get_turn(
        &self,
        ctx: &SecurityContext,
        chat_id: Uuid,
        request_id: Uuid,
    ) -> DomainResult<TurnStatus> {
        let (_, scope) = self
            .authorized_chat(ctx, actions::READ_TURN, chat_id)
            .await?;
        let conn = self.db.conn()?;
        let turn = chat_turn::Entity::find()
            .secure()
            .scope_with(&scope)
            .filter(
                Condition::all()
                    .add(chat_turn::Column::ChatId.eq(chat_id))
                    .add(chat_turn::Column::RequestId.eq(request_id))
                    .add(chat_turn::Column::DeletedAt.is_null()),
            )
            .one(&conn)
            .await?
            .ok_or(DomainError::TurnNotFound { request_id })?;
        let state = api_state(&turn.state);
        Ok(TurnStatus {
            request_id,
            state,
            error_code: if state == "error" {
                turn.error_code
            } else {
                None
            },
            assistant_message_id: if matches!(state, "done" | "cancelled") {
                turn.assistant_message_id
            } else {
                None
            },
            updated_at: turn.updated_at,
        })
    }

    /// Read-only validation of a mutation: 404, running (400), not latest
    /// (409), not the requester (403).
    async fn mutation_preview(
        &self,
        ctx: &SecurityContext,
        chat_id: Uuid,
        request_id: Uuid,
        kind: Mutation,
    ) -> DomainResult<(chat::Model, AccessScope, chat_turn::Model)> {
        let (chat, scope) = self.authorized_chat(ctx, kind.action(), chat_id).await?;
        let conn = self.db.conn()?;
        let turn = chat_turn::Entity::find()
            .secure()
            .scope_with(&scope)
            .filter(
                Condition::all()
                    .add(chat_turn::Column::ChatId.eq(chat_id))
                    .add(chat_turn::Column::RequestId.eq(request_id)),
            )
            .one(&conn)
            .await?
            .ok_or(DomainError::TurnNotFound { request_id })?;
        if turn.deleted_at.is_none() && turn.state == "running" {
            return Err(DomainError::TurnNotTerminal);
        }
        let latest = latest_turn(&conn, &scope, chat_id).await?;
        if turn.deleted_at.is_some() || latest.as_ref().is_none_or(|l| l.id != turn.id) {
            return Err(DomainError::NotLatestTurn);
        }
        if turn.requester_user_id != Some(ctx.subject_id()) {
            return Err(DomainError::AccessDenied);
        }
        Ok((chat, scope, turn))
    }

    /// `DELETE /v1/chats/{id}/turns/{request_id}`.
    ///
    /// # Errors
    /// Preview errors, `GenerationInProgress`, `NotLatestTurn`.
    pub async fn delete_turn(
        &self,
        ctx: &SecurityContext,
        chat_id: Uuid,
        request_id: Uuid,
    ) -> DomainResult<()> {
        let started = Instant::now();
        let r = self.delete_turn_inner(ctx, chat_id, request_id).await;
        self.record_mutation("delete", r.is_ok(), started);
        r
    }

    fn record_mutation(&self, op: &str, ok: bool, started: Instant) {
        let m = &self.metrics;
        m.turn_mutation.add(
            1,
            &crate::infra::metrics::labels(&[
                ("op", op),
                ("result", if ok { "ok" } else { "error" }),
            ]),
        );
        #[allow(clippy::cast_precision_loss)]
        m.turn_mutation_latency_ms.record(
            started.elapsed().as_millis() as f64,
            &crate::infra::metrics::labels(&[("op", op)]),
        );
    }

    async fn delete_turn_inner(
        &self,
        ctx: &SecurityContext,
        chat_id: Uuid,
        request_id: Uuid,
    ) -> DomainResult<()> {
        let (_, scope, turn) = self
            .mutation_preview(ctx, chat_id, request_id, Mutation::Delete)
            .await?;
        let outbox = Arc::clone(&self.outbox);
        let actor = ctx.subject_id();
        let wakes = self
            .tx(move |tx| {
                let outbox = Arc::clone(&outbox);
                let scope = scope.clone();
                let turn = turn.clone();
                Box::pin(async move {
                    mutate_tail(
                        tx,
                        &outbox,
                        &scope,
                        chat_id,
                        &turn,
                        None,
                        actor,
                        Mutation::Delete,
                    )
                    .await
                })
            })
            .await?;
        fire(wakes);
        Ok(())
    }

    /// `POST .../turns/{request_id}/retry` and `PATCH .../turns/{request_id}`.
    ///
    /// # Errors
    /// Preview and preflight rejections (previous turn unchanged); setup
    /// failures after the commit mark the new turn `failed`.
    pub async fn regenerate_turn(
        self: &Arc<Self>,
        ctx: &SecurityContext,
        chat_id: Uuid,
        request_id: Uuid,
        new_content: Option<String>,
    ) -> DomainResult<StreamStart> {
        let started = Instant::now();
        let op = if new_content.is_some() {
            "edit"
        } else {
            "retry"
        };
        let r = self
            .regenerate_turn_inner(ctx, chat_id, request_id, new_content)
            .await;
        self.record_mutation(op, r.is_ok(), started);
        r
    }

    #[allow(clippy::too_many_lines)]
    async fn regenerate_turn_inner(
        self: &Arc<Self>,
        ctx: &SecurityContext,
        chat_id: Uuid,
        request_id: Uuid,
        new_content: Option<String>,
    ) -> DomainResult<StreamStart> {
        let kind = if new_content.is_some() {
            Mutation::Edit
        } else {
            Mutation::Retry
        };
        if let Some(c) = &new_content
            && c.trim().is_empty()
        {
            return Err(DomainError::EmptyContent);
        }
        let (chat, scope, old) = self
            .mutation_preview(ctx, chat_id, request_id, kind)
            .await?;
        let conn = self.db.conn()?;
        let old_user = message::Entity::find()
            .secure()
            .scope_with(&scope)
            .filter(
                Condition::all()
                    .add(message::Column::ChatId.eq(chat_id))
                    .add(message::Column::RequestId.eq(old.request_id))
                    .add(message::Column::Role.eq("user")),
            )
            .one(&conn)
            .await?
            .ok_or_else(|| DomainError::internal("turn has no user message"))?;
        // Non-deleted attachments of the original message (in order).
        let links = message_attachment::Entity::find()
            .secure()
            .scope_with(&scope)
            .filter(
                Condition::all()
                    .add(message_attachment::Column::ChatId.eq(chat_id))
                    .add(message_attachment::Column::MessageId.eq(old_user.id)),
            )
            .order_by(message_attachment::Column::CreatedAt, sea_orm::Order::Asc)
            .all(&conn)
            .await?;
        let link_ids: Vec<Uuid> = links.iter().map(|l| l.attachment_id).collect();
        let atts: Vec<attachment::Model> = if link_ids.is_empty() {
            Vec::new()
        } else {
            let rows = attachment::Entity::find()
                .secure()
                .scope_with(&scope)
                .filter(
                    Condition::all()
                        .add(attachment::Column::ChatId.eq(chat_id))
                        .add(attachment::Column::Id.is_in(link_ids.clone()))
                        .add(attachment::Column::DeletedAt.is_null()),
                )
                .all(&conn)
                .await?;
            link_ids
                .iter()
                .filter_map(|id| rows.iter().find(|a| a.id == *id).cloned())
                .collect()
        };
        let content = new_content.unwrap_or_else(|| old_user.content.clone());
        let images: Vec<attachment::Model> = atts
            .iter()
            .filter(|a| a.attachment_kind == "image")
            .cloned()
            .collect();
        let pre = self
            .preflight(PreflightArgs {
                ctx,
                chat: &chat,
                scope: &scope,
                content: &content,
                images,
                web_search: old.web_search_enabled,
                exclude_request_id: Some(old.request_id),
            })
            .await?;

        // Mutation commit.
        let new_request_id = Uuid::new_v4();
        let turn_id = Uuid::new_v4();
        let user_message_id = Uuid::new_v4();
        let tenant_id = ctx.subject_tenant_id();
        let user_id = ctx.subject_id();
        let att_ids: Vec<Uuid> = atts.iter().map(|a| a.id).collect();
        let outbox = Arc::clone(&self.outbox);
        let web_search = old.web_search_enabled;
        let (wakes, user_created) = {
            let scope = scope.clone();
            let old = old.clone();
            let content = content.clone();
            self.tx(move |tx| {
                let outbox = Arc::clone(&outbox);
                let scope = scope.clone();
                let old = old.clone();
                let content = content.clone();
                let att_ids = att_ids.clone();
                Box::pin(async move {
                    let wakes = mutate_tail(
                        tx,
                        &outbox,
                        &scope,
                        chat_id,
                        &old,
                        Some(new_request_id),
                        user_id,
                        kind,
                    )
                    .await?;
                    let now = clock::now();
                    insert_message(
                        tx,
                        &scope,
                        NewMessage {
                            id: user_message_id,
                            tenant_id,
                            chat_id,
                            request_id: new_request_id,
                            role: "user",
                            content: &content,
                            model: None,
                            usage: None,
                            provider_response_id: None,
                            created_at: now,
                        },
                    )
                    .await?;
                    insert_links(tx, &scope, tenant_id, chat_id, user_message_id, &att_ids).await?;
                    let am = new_turn(
                        turn_id,
                        tenant_id,
                        chat_id,
                        new_request_id,
                        user_id,
                        web_search,
                        None,
                        now,
                    );
                    match secure_insert::<chat_turn::Entity>(am, &scope, tx).await {
                        Ok(_) => {}
                        Err(e) if e.is_unique_violation() => {
                            return Err(DomainError::GenerationInProgress);
                        }
                        Err(e) => return Err(e.into()),
                    }
                    Ok((wakes, now))
                })
            })
            .await?
        };
        fire(wakes);

        // Setup after the commit: context, provider, reserve.
        let ready_images: Vec<attachment::Model> = pre
            .images
            .iter()
            .filter(|a| a.status == "ready")
            .cloned()
            .collect();
        let built = self
            .build_request(
                tenant_id,
                user_id,
                &chat,
                &scope,
                &pre.decision,
                &content,
                &ready_images,
                Some(new_request_id),
            )
            .await;
        let (plan, request, target, file_map, knowledge) = match built {
            Ok(v) => v,
            Err(e) => {
                let code = if matches!(e, DomainError::ContextBudgetExceeded) {
                    "context_length_exceeded"
                } else {
                    "turn_setup_failed"
                };
                self.fail_unstarted(tenant_id, turn_id, code, &e).await;
                return Err(e);
            }
        };
        let decision = pre.decision.clone();
        let reserve = self
            .tx(move |tx| {
                let decision = decision.clone();
                Box::pin(async move {
                    Self::write_reserve(tx, tenant_id, user_id, &decision).await?;
                    let scope = AccessScope::for_tenant(tenant_id);
                    let r = &decision.reserve;
                    let res = chat_turn::Entity::update_many()
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
                            Expr::value(Some(
                                i64::try_from(decision.policy_version).unwrap_or(i64::MAX),
                            )),
                        )
                        .col_expr(
                            chat_turn::Column::EffectiveModel,
                            Expr::value(Some(decision.effective.id.clone())),
                        )
                        .col_expr(
                            chat_turn::Column::MinimalGenerationFloorApplied,
                            Expr::value(Some(
                                i32::try_from(decision.minimal_generation_floor_applied)
                                    .unwrap_or(i32::MAX),
                            )),
                        )
                        .filter(
                            Condition::all()
                                .add(chat_turn::Column::Id.eq(turn_id))
                                .add(chat_turn::Column::State.eq("running"))
                                .add(chat_turn::Column::ReserveTokens.is_null()),
                        )
                        .secure()
                        .scope_with(&scope)
                        .exec(tx)
                        .await?;
                    if res.rows_affected == 0 {
                        return Err(DomainError::internal(
                            "retry/edit turn is no longer running",
                        ));
                    }
                    Ok(())
                })
            })
            .await;
        if reserve.is_ok() {
            self.record_reserve();
        }
        if let Err(e) = reserve {
            let code = if e.is_quota_exceeded() {
                "quota_exceeded"
            } else {
                "turn_setup_failed"
            };
            self.fail_unstarted(tenant_id, turn_id, code, &e).await;
            return Err(e);
        }
        let run = TurnRun {
            tenant_id,
            user_id,
            chat_id,
            turn_id,
            request_id: new_request_id,
            assistant_message_id: Uuid::new_v4(),
            user_message_key: (user_created, user_message_id),
            decision: pre.decision,
            target,
            request,
            file_map,
            plan_tokens: plan.assembled_tokens,
            effective_budget: plan.effective_budget,
            messages_truncated: plan.messages_truncated,
            has_summary: plan.summary.is_some(),
            knowledge,
            started: Instant::now(),
        };
        Ok(StreamStart::Live(
            self.spawn_turn(run, plan.summary_applied),
        ))
    }

    /// Mark an unstarted retry/edit turn `failed` (no reserve, no events).
    async fn fail_unstarted(
        &self,
        tenant_id: Uuid,
        turn_id: Uuid,
        code: &str,
        cause: &DomainError,
    ) {
        let Ok(conn) = self.db.conn() else { return };
        let now = clock::now();
        let res = chat_turn::Entity::update_many()
            .col_expr(chat_turn::Column::State, Expr::value("failed"))
            .col_expr(
                chat_turn::Column::ErrorCode,
                Expr::value(Some(code.to_owned())),
            )
            .col_expr(
                chat_turn::Column::ErrorDetail,
                Expr::value(Some(cause.to_string())),
            )
            .col_expr(chat_turn::Column::CompletedAt, Expr::value(Some(now)))
            .col_expr(chat_turn::Column::UpdatedAt, Expr::value(now))
            .filter(
                Condition::all()
                    .add(chat_turn::Column::Id.eq(turn_id))
                    .add(chat_turn::Column::State.eq("running")),
            )
            .secure()
            .scope_with(&AccessScope::for_tenant(tenant_id))
            .exec(&conn)
            .await;
        if let Err(e) = res {
            tracing::error!(error = %e, "mini-chat: failed to mark unstarted turn failed");
        }
    }
}
