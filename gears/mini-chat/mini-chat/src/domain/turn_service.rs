//! Turn service: turn status, retry/edit/delete mutations (last turn only).

use std::sync::Arc;

use mini_chat_sdk::{PolicyDecisions, QuotaAuditDecision, ToolCalls, TurnAuditEvent};
use toolkit_db::outbox::Wake;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::api::rest::dto::{TurnStatusResponse, TurnStatusState, fmt_ts};
use crate::domain::finalize::now;
use crate::domain::services::ServiceContext;
use crate::domain::stream_service::{StreamService, StreamSetup};
use crate::error::{DomainError, ResourceKind};
use crate::infra::storage::entity::chat_turns;
use crate::infra::outbox::OutboxEnqueuer;
use crate::infra::storage::repo::turns::state;

/// The turn service.
pub struct TurnService {
    ctx: Arc<ServiceContext>,
    stream: Arc<StreamService>,
}

/// Mutation operation kinds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MutationOp {
    /// Retry.
    Retry,
    /// Edit.
    Edit,
    /// Delete.
    Delete,
}

impl MutationOp {
    fn label(self) -> &'static str {
        match self {
            Self::Retry => "retry",
            Self::Edit => "edit",
            Self::Delete => "delete",
        }
    }
}

/// A validated mutation target.
#[derive(Clone)]
struct MutationTarget {
    chat: crate::infra::storage::entity::chats::Model,
    turn: chat_turns::Model,
    user_message: crate::infra::storage::entity::messages::Model,
    attachment_ids: Vec<Uuid>,
}

impl TurnService {
    /// Create the service.
    #[must_use]
    pub fn new(ctx: Arc<ServiceContext>, stream: Arc<StreamService>) -> Self {
        Self { ctx, stream }
    }

    /// `GET /v1/chats/{id}/turns/{request_id}`.
    pub async fn get_turn(
        &self,
        ctx: &SecurityContext,
        chat_id: Uuid,
        request_id: Uuid,
    ) -> Result<TurnStatusResponse, DomainError> {
        let scope = self.ctx.pep.chat_scope(ctx, "read_turn", Some(chat_id)).await?;
        let tenant_id = ctx.subject_tenant_id();
        let user_id = ctx.subject_id();
        let conn = self.ctx.db.conn()?;
        self.ctx
            .repos
            .chats
            .get(&conn, &scope, tenant_id, user_id, chat_id)
            .await?
            .ok_or(DomainError::NotFound(ResourceKind::Chat))?;
        let turn = self
            .ctx
            .repos
            .turns
            .get_any(&conn, &scope, tenant_id, chat_id, request_id)
            .await?
            .filter(|t| t.deleted_at.is_none())
            .ok_or(DomainError::NotFound(ResourceKind::Turn))?;
        let state = match turn.state.as_str() {
            state::RUNNING => TurnStatusState::Running,
            state::COMPLETED => TurnStatusState::Done,
            state::FAILED => TurnStatusState::Error,
            _ => TurnStatusState::Cancelled,
        };
        let error_code = if state == TurnStatusState::Error {
            turn.error_code.clone()
        } else {
            None
        };
        let assistant_message_id = match state {
            TurnStatusState::Done => turn.assistant_message_id,
            TurnStatusState::Cancelled => turn.assistant_message_id,
            _ => None,
        };
        Ok(TurnStatusResponse {
            request_id,
            state,
            error_code,
            assistant_message_id,
            updated_at: turn.updated_at,
        })
    }

    /// Mutation preview: validate latest/terminal/ownership and load the
    /// target with its user message and attachment ids.
    async fn preview(
        &self,
        ctx: &SecurityContext,
        action: &str,
        chat_id: Uuid,
        request_id: Uuid,
    ) -> Result<(toolkit_db::secure::AccessScope, MutationTarget), DomainError> {
        let scope = self.ctx.pep.chat_scope(ctx, action, Some(chat_id)).await?;
        let tenant_id = ctx.subject_tenant_id();
        let user_id = ctx.subject_id();
        let conn = self.ctx.db.conn()?;
        let chat = self
            .ctx
            .repos
            .chats
            .get(&conn, &scope, tenant_id, user_id, chat_id)
            .await?
            .ok_or(DomainError::NotFound(ResourceKind::Chat))?;
        let turn = self
            .ctx
            .repos
            .turns
            .get_any(&conn, &scope, tenant_id, chat_id, request_id)
            .await?;
        let Some(turn) = turn else {
            return Err(DomainError::NotFound(ResourceKind::Turn));
        };
        if turn.deleted_at.is_some() {
            return Err(DomainError::NotLatestTurn);
        }
        // Ownership: the target turn must belong to the caller.
        if turn.requester_user_id != Some(user_id) {
            return Err(DomainError::PermissionDenied);
        }
        // Terminal-state check before the latest-turn check.
        if turn.state == state::RUNNING {
            return Err(DomainError::TurnStateNotTerminal);
        }
        let latest = self
            .ctx
            .repos
            .turns
            .latest(&conn, &scope, tenant_id, chat_id)
            .await?;
        if latest.map(|l| l.id) != Some(turn.id) {
            return Err(DomainError::NotLatestTurn);
        }
        // The turn's user message.
        let user_message = self
            .ctx
            .repos
            .messages
            .get_turn_user_message(&conn, tenant_id, chat_id, request_id)
            .await?
            .ok_or(DomainError::Internal)?;
        let attachment_ids = self
            .ctx
            .repos
            .message_attachments
            .attachments_of(&conn, tenant_id, chat_id, user_message.id)
            .await?;
        Ok((
            scope,
            MutationTarget {
                chat,
                turn,
                user_message,
                attachment_ids,
            },
        ))
    }

    /// `DELETE /v1/chats/{id}/turns/{request_id}`.
    pub async fn delete_turn(
        &self,
        ctx: &SecurityContext,
        chat_id: Uuid,
        request_id: Uuid,
    ) -> Result<(), DomainError> {
        let started = std::time::Instant::now();
        let (_scope, target) = self.preview(ctx, "delete_turn", chat_id, request_id).await?;
        let svc_ctx = Arc::clone(&self.ctx);
        let result = self
            .ctx
            .db
            .transaction(|tx| {
                let target = target.clone();
                let svc_ctx = Arc::clone(&svc_ctx);
                Box::pin(async move {
                    Self::mutation_commit(&svc_ctx, tx, &target, None, MutationOp::Delete).await
                })
            })
            .await;
        let ok = result.is_ok();
        self.record_mutation_ok(MutationOp::Delete, started, ok);
        let (_, wakes) = result?;
        for wake in wakes {
            wake.fire();
        }
        Ok(())
    }

    /// `POST /v1/chats/{id}/turns/{request_id}/retry`.
    pub async fn retry_turn(
        &self,
        ctx: &SecurityContext,
        chat_id: Uuid,
        request_id: Uuid,
    ) -> Result<StreamSetup, DomainError> {
        self.mutate_stream(ctx, chat_id, request_id, MutationOp::Retry, None)
            .await
    }

    /// `PATCH /v1/chats/{id}/turns/{request_id}`.
    pub async fn edit_turn(
        &self,
        ctx: &SecurityContext,
        chat_id: Uuid,
        request_id: Uuid,
        content: String,
    ) -> Result<StreamSetup, DomainError> {
        if content.trim().is_empty() {
            return Err(DomainError::EmptyContent);
        }
        self.mutate_stream(ctx, chat_id, request_id, MutationOp::Edit, Some(content))
            .await
    }

    /// Retry/edit shared pipeline: preview → preflight → mutation commit →
    /// assembly → reserve → stream.
    async fn mutate_stream(
        &self,
        ctx: &SecurityContext,
        chat_id: Uuid,
        request_id: Uuid,
        op: MutationOp,
        new_content: Option<String>,
    ) -> Result<StreamSetup, DomainError> {
        let started = std::time::Instant::now();
        let result = self
            .mutate_stream_inner(ctx, chat_id, request_id, op, new_content)
            .await;
        self.record_mutation_ok(op, started, result.is_ok());
        result
    }

    async fn mutate_stream_inner(
        &self,
        ctx: &SecurityContext,
        chat_id: Uuid,
        request_id: Uuid,
        op: MutationOp,
        new_content: Option<String>,
    ) -> Result<StreamSetup, DomainError> {
        let (scope, target) = self
            .preview(ctx, action_for(op), chat_id, request_id)
            .await?;
        let content = new_content
            .clone()
            .unwrap_or_else(|| target.user_message.content.clone());

        // Preflight (read-only): a rejection leaves the previous turn intact.
        let bundle = self
            .stream
            .preflight_phase(
                ctx,
                &scope,
                &target.chat,
                &content,
                &target.attachment_ids,
                target.turn.web_search_enabled,
            )
            .await?;

        // Mutation commit.
        let new_request_id = Uuid::new_v4();
        let new_turn_id = Uuid::new_v4();
        let committed = self
            .ctx
            .db
            .transaction(|tx| {
                let content = content.clone();
                let target = target.clone();
                let svc_ctx = Arc::clone(&self.ctx);
                Box::pin(async move {
                    Self::mutation_commit(&svc_ctx, tx, &target, Some((new_request_id, new_turn_id, content)), op)
                        .await
                })
            })
            .await;
        let (_user_msg_id, wakes) = committed?;
        for wake in wakes {
            wake.fire();
        }

        // Assembly + provider resolution + reserve + spawn. A failure here
        // marks the new turn failed and returns a JSON error.
        let prepared = match self
            .stream
            .assemble_phase(&target.chat, &user_msg_content(&target, new_content.as_ref()), target.turn.web_search_enabled, bundle)
            .await
        {
            Ok(prepared) => prepared,
            Err(err) => {
                self.mark_setup_failed(new_turn_id, &err).await;
                return Err(err);
            }
        };
        let mut prepared = prepared;
        prepared.turn_id = new_turn_id;
        self.stream
            .mutation_reserve_and_spawn(ctx, &target.chat, prepared, new_request_id)
            .await
    }

    /// The mutation commit transaction: soft-delete the old turn and its
    /// messages, insert the new user message (with copied attachment links)
    /// and the new running turn, invalidate a covering summary, bump the
    /// chat, and enqueue the mutation audit event.
    ///
    /// Returns the new user message id and the wakes to fire after commit.
    async fn mutation_commit(
        ctx: &Arc<ServiceContext>,
        tx: &impl toolkit_db::secure::DBRunner,
        target: &MutationTarget,
        replacement: Option<(Uuid, Uuid, String)>,
        op: MutationOp,
    ) -> Result<(Uuid, Vec<Wake>), DomainError> {
        let tenant_id = target.chat.tenant_id;
        let mut wakes = Vec::new();
        let scope = toolkit_db::secure::AccessScope::allow_all();
        // Soft-delete the previous turn (and mark the replacement).
        let replaced_by = replacement.as_ref().map(|(rid, _, _)| *rid);
        let deleted = ctx
            .repos
            .turns
            .soft_delete(tx, target.turn.id, replaced_by)
            .await?;
        if !deleted {
            // Lost the race against a concurrent mutation: a delete sees
            // NOT_LATEST_TURN; a retry/edit proceeds to the running-turn
            // insert, which fails the one-running-turn unique index ->
            // GENERATION_IN_PROGRESS.
            if replacement.is_none() {
                return Err(DomainError::NotLatestTurn);
            }
        }
        ctx
            .repos
            .messages
            .soft_delete_turn(tx, tenant_id, target.chat.id, target.turn.request_id)
            .await?;

        // Summary invalidation: if the summary covers the mutated turn's
        // user message, delete it and unmark compressed messages.
        let summary = ctx
            .repos
            .summaries
            .get(tx, tenant_id, target.chat.id)
            .await?;
        if let Some(summary) = summary {
            let covers = (summary.summarized_up_to_created_at, summary.summarized_up_to_message_id)
                >= (target.user_message.created_at, target.user_message.id);
            if covers {
                ctx
                    .repos
                    .summaries
                    .delete_for_chat(tx, tenant_id, target.chat.id)
                    .await?;
                ctx
                    .repos
                    .messages
                    .clear_compressed(tx, tenant_id, target.chat.id)
                    .await?;
            }
        }

        let mut user_msg_id = target.user_message.id;
        let audit_new_request_id = replacement.as_ref().map(|(rid, _, _)| *rid);
        if let Some((new_request_id, new_turn_id, content)) = replacement {
            // Copy the attachment associations (excluding attachments
            // deleted since).
            let attachment_ids = ctx
                .repos
                .attachments
                .filter_existing(tx, tenant_id, target.chat.id, &target.attachment_ids)
                .await?;
            user_msg_id = Uuid::new_v4();
            ctx
                .repos
                .messages
                .insert(
                    tx,
                    &scope,
                    crate::infra::storage::repo::messages::NewMessage {
                        id: user_msg_id,
                        tenant_id,
                        chat_id: target.chat.id,
                        request_id: Some(new_request_id),
                        role: "user".to_owned(),
                        content,
                        model: None,
                        provider_response_id: None,
                        input_tokens: 0,
                        output_tokens: 0,
                        cache_read_input_tokens: 0,
                        cache_write_input_tokens: 0,
                        reasoning_tokens: 0,
                    },
                )
                .await?;
            ctx
                .repos
                .message_attachments
                .insert_batch(
                    tx,
                    &scope,
                    tenant_id,
                    target.chat.id,
                    user_msg_id,
                    &attachment_ids,
                )
                .await?;
            // The new running turn (preflight fields NULL until the reserve
            // transaction).
            let insert = ctx
                .repos
                .turns
                .insert_unstarted(
                    tx,
                    &scope,
                    new_turn_id,
                    tenant_id,
                    target.chat.id,
                    new_request_id,
                    target.turn.requester_user_id.unwrap_or(Uuid::nil()),
                    target.turn.web_search_enabled,
                )
                .await;
            if let Err(err) = insert {
                if err.is_unique_violation() {
                    return Err(DomainError::GenerationInProgress);
                }
                return Err(err);
            }
        }

        // Bump the chat's updated_at.
        ctx
            .repos
            .chats
            .touch(tx, tenant_id, target.chat.id)
            .await?;

        // Mutation audit event.
        let event_type = match op {
            MutationOp::Retry => "turn_retry",
            MutationOp::Edit => "turn_edit",
            MutationOp::Delete => "turn_delete",
        };
        let audit = TurnAuditEvent {
            event_type: event_type.to_owned(),
            tenant_id,
            user_id: None,
            actor_user_id: target.turn.requester_user_id,
            chat_id: target.chat.id,
            turn_id: None,
            request_id: target.turn.request_id,
            original_request_id: (op != MutationOp::Delete).then_some(target.turn.request_id),
            new_request_id: audit_new_request_id,
            selected_model: String::new(),
            effective_model: String::new(),
            usage: None,
            latency_ms: None,
            tool_calls: ToolCalls::default(),
            policy_decisions: PolicyDecisions {
                quota: QuotaAuditDecision {
                    decision: String::new(),
                    downgrade_from: None,
                    downgrade_reason: None,
                },
                license: String::new(),
            },
            error_code: None,
            prompt: String::new(),
            response: String::new(),
            attachments: Vec::new(),
            quota_scope: String::new(),
            trace_id: crate::domain::finalize::FinalizeService::current_trace_id(),
            timestamp: fmt_ts(&now()),
        };
        wakes.push(ctx.outbox.enqueue_audit(tx, &audit).await?);
        Ok((user_msg_id, wakes))
    }

    /// Mark a mutation turn failed after the commit (setup failure before
    /// the provider call).
    async fn mark_setup_failed(&self, new_turn_id: Uuid, err: &DomainError) {
        let code = match err {
            DomainError::ContextBudgetExceeded => "context_length_exceeded",
            _ => "turn_setup_failed",
        };
        let Ok(conn) = self.ctx.db.conn() else { return };
        drop(self
            .ctx
            .repos
            .turns
            .finalize_failed(&conn, new_turn_id, code, Some(&err.to_string()))
            .await);
    }

    fn record_mutation_ok(&self, op: MutationOp, started: std::time::Instant, ok: bool) {
        self.ctx.metrics.turn_mutation.add(
            1,
            &[
                crate::infra::metrics::label("op", op.label()),
                crate::infra::metrics::label("result", if ok { "ok" } else { "error" }),
            ],
        );
        self.ctx
            .metrics
            .turn_mutation_latency_ms
            .record(started.elapsed().as_secs_f64() * 1000.0, &[crate::infra::metrics::label("op", op.label())]);
    }
}

/// The PDP action name of a mutation operation.
fn action_for(op: MutationOp) -> &'static str {
    match op {
        MutationOp::Retry => "retry_turn",
        MutationOp::Edit => "edit_turn",
        MutationOp::Delete => "delete_turn",
    }
}

/// The content of the new user message.
fn user_msg_content(target: &MutationTarget, new_content: Option<&String>) -> String {
    new_content
        .cloned()
        .unwrap_or_else(|| target.user_message.content.clone())
}
