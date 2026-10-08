//! Turn status and tail-only turn mutations (retry, edit, delete; DESIGN
//! §3.9).

use std::sync::Arc;

use mini_chat_sdk::{AuditEvent, TurnMutationAuditEvent};
use time::OffsetDateTime;
use toolkit_security::{AccessScope, SecurityContext};
use uuid::Uuid;

use super::error::{DomainError, Res};
use super::service::Services;
use super::stream::TurnStart;
use super::stream::finalize::apply_reserve;
use super::stream::plan::{PlanError, TurnInputs};
use super::stream::send::{check_chat_model, validate_content};
use crate::infra::db::entities::{attachment, chat, chat_turn, message};
use crate::infra::db::repo::turns::{PreflightFields, Terminal};
use crate::infra::db::repo::{attachments, chats, messages, summaries, turns};
use crate::infra::db::{now_ts, tenant_scope, with_retry};
use crate::infra::outbox::{self, Queue};

/// Turn status (Turn Status API).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnStatusView {
    pub request_id: Uuid,
    /// `running` | `done` | `error` | `cancelled`.
    pub state: &'static str,
    pub error_code: Option<String>,
    pub assistant_message_id: Option<Uuid>,
    pub updated_at: OffsetDateTime,
}

/// Mutation kind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mutation {
    Retry,
    Edit(String),
    Delete,
}

impl Mutation {
    const fn op(&self) -> &'static str {
        match self {
            Self::Retry => "retry",
            Self::Edit(_) => "edit",
            Self::Delete => "delete",
        }
    }

    const fn action(&self) -> &'static str {
        match self {
            Self::Retry => "retry_turn",
            Self::Edit(_) => "edit_turn",
            Self::Delete => "delete_turn",
        }
    }
}

fn not_latest() -> DomainError {
    DomainError::aborted(
        Res::Turn,
        "NOT_LATEST_TURN",
        "Only the latest turn can be modified",
    )
}

/// Delete the thread summary when it covers the mutated turn's user message.
async fn invalidate_summary(
    runner: &(impl toolkit_db::secure::DBRunner + Sync),
    scope: &AccessScope,
    chat_id: Uuid,
    user_msg: Option<&message::Model>,
) -> Result<(), DomainError> {
    let Some(summary) = summaries::find(runner, scope, chat_id).await? else {
        return Ok(());
    };
    let covers = user_msg.is_none_or(|m| {
        (
            summary.summarized_up_to_created_at,
            summary.summarized_up_to_message_id,
        ) >= (m.created_at, m.id)
    });
    if covers {
        summaries::delete(runner, scope, chat_id).await?;
        messages::clear_compressed(runner, scope, chat_id).await?;
    }
    Ok(())
}

impl Services {
    pub async fn turn_status(
        &self,
        ctx: &SecurityContext,
        chat_id: Uuid,
        request_id: Uuid,
    ) -> Result<TurnStatusView, DomainError> {
        let chat = self.load_chat(ctx, "read_turn", chat_id).await?;
        let conn = self.db.conn()?;
        let t = turns::find_by_request(&conn, &tenant_scope(chat.tenant_id), chat_id, request_id)
            .await?
            .filter(|t| t.deleted_at.is_none())
            .ok_or_else(|| DomainError::not_found(Res::Turn, request_id.to_string()))?;
        let state = match t.state.as_str() {
            turns::STATE_RUNNING => "running",
            turns::STATE_COMPLETED => "done",
            turns::STATE_CANCELLED => "cancelled",
            _ => "error",
        };
        Ok(TurnStatusView {
            request_id: t.request_id,
            state,
            error_code: if state == "error" {
                t.error_code.clone()
            } else {
                None
            },
            assistant_message_id: if state == "done" || state == "cancelled" {
                t.assistant_message_id
            } else {
                None
            },
            updated_at: t.updated_at,
        })
    }

    /// Read-only mutation preview: the target exists, is terminal, is the
    /// latest turn and belongs to the caller.
    async fn mutation_preview(
        &self,
        ctx: &SecurityContext,
        chat_id: Uuid,
        request_id: Uuid,
        m: &Mutation,
    ) -> Result<(chat::Model, chat_turn::Model), DomainError> {
        let chat = self.load_chat(ctx, m.action(), chat_id).await?;
        let scope = tenant_scope(chat.tenant_id);
        let conn = self.db.conn()?;
        let target = turns::find_by_request(&conn, &scope, chat_id, request_id)
            .await?
            .ok_or_else(|| DomainError::not_found(Res::Turn, request_id.to_string()))?;
        if target.deleted_at.is_none() && target.state == turns::STATE_RUNNING {
            return Err(DomainError::precondition(
                Res::Turn,
                "turn_state",
                "STATE",
                "The turn is still running",
            ));
        }
        if target.deleted_at.is_some() {
            return Err(not_latest());
        }
        let latest = turns::latest(&conn, &scope, chat_id).await?;
        if latest.as_ref().is_none_or(|l| l.id != target.id) {
            return Err(not_latest());
        }
        if target.requester_user_id != Some(ctx.subject_id()) {
            return Err(DomainError::PermissionDenied);
        }
        Ok((chat, target))
    }

    /// `DELETE /chats/{id}/turns/{request_id}`.
    pub async fn delete_turn(
        &self,
        ctx: &SecurityContext,
        chat_id: Uuid,
        request_id: Uuid,
    ) -> Result<(), DomainError> {
        let started = std::time::Instant::now();
        let res = self.delete_turn_inner(ctx, chat_id, request_id).await;
        self.record_mutation("delete", &res, started);
        res
    }

    async fn delete_turn_inner(
        &self,
        ctx: &SecurityContext,
        chat_id: Uuid,
        request_id: Uuid,
    ) -> Result<(), DomainError> {
        let (chat, target) = self
            .mutation_preview(ctx, chat_id, request_id, &Mutation::Delete)
            .await?;
        let scope = tenant_scope(chat.tenant_id);
        let user = ctx.subject_id();
        let wakes = with_retry(|| {
            let scope = scope.clone();
            let target = target.clone();
            let ob = self.outbox.clone();
            self.db.transaction(move |tx| {
                Box::pin(async move {
                    let now = now_ts();
                    let latest = turns::latest(tx, &scope, chat_id).await?;
                    if latest.as_ref().is_none_or(|l| l.id != target.id) {
                        return Err(not_latest());
                    }
                    let user_msg =
                        messages::find_by_request(tx, &scope, chat_id, request_id, "user").await?;
                    if !turns::soft_delete(tx, &scope, target.id, None, now).await? {
                        return Err(not_latest());
                    }
                    messages::soft_delete_by_request(tx, &scope, chat_id, request_id, now).await?;
                    invalidate_summary(tx, &scope, chat_id, user_msg.as_ref()).await?;
                    let ev = AuditEvent::Mutation(TurnMutationAuditEvent {
                        event_type: "turn_delete".to_owned(),
                        timestamp: OffsetDateTime::now_utc(),
                        tenant_id: target.tenant_id,
                        actor_user_id: user,
                        chat_id,
                        original_request_id: None,
                        new_request_id: None,
                        request_id: Some(request_id),
                    });
                    let wake = ob
                        .enqueue(tx, Queue::Audit, target.tenant_id, &ev)
                        .await
                        .map_err(|e| match e {
                            DomainError::InvalidFormat(m) => DomainError::internal(m),
                            other => other,
                        })?;
                    Ok(vec![wake])
                })
            })
        })
        .await?;
        outbox::fire(wakes);
        Ok(())
    }

    fn record_mutation<T>(
        &self,
        op: &str,
        res: &Result<T, DomainError>,
        started: std::time::Instant,
    ) {
        let result = match res {
            Ok(_) => "ok",
            Err(DomainError::Aborted { .. }) => "conflict",
            Err(DomainError::FailedPrecondition { .. }) => "precondition",
            Err(DomainError::QuotaExceeded(_)) => "quota_exceeded",
            Err(_) => "error",
        };
        let op_owned = op.to_owned();
        self.metrics.inc(
            "turn_mutation",
            &[("op", op_owned.as_str()), ("result", result)],
        );
        #[allow(clippy::cast_precision_loss)]
        self.metrics.record(
            "turn_mutation_latency_ms",
            started.elapsed().as_millis() as f64,
            &[("op", op_owned.as_str())],
        );
    }

    /// `POST …/retry` and `PATCH …/turns/{request_id}`.
    pub async fn start_mutation(
        self: &Arc<Self>,
        ctx: &SecurityContext,
        chat_id: Uuid,
        request_id: Uuid,
        m: Mutation,
    ) -> Result<TurnStart, DomainError> {
        let started = std::time::Instant::now();
        let res = self
            .start_mutation_inner(ctx, chat_id, request_id, &m)
            .await;
        self.record_mutation(m.op(), &res, started);
        res
    }

    #[allow(clippy::too_many_lines)]
    async fn start_mutation_inner(
        self: &Arc<Self>,
        ctx: &SecurityContext,
        chat_id: Uuid,
        request_id: Uuid,
        m: &Mutation,
    ) -> Result<TurnStart, DomainError> {
        if let Mutation::Edit(content) = m {
            validate_content(content)?;
        }
        let (chat, target) = self.mutation_preview(ctx, chat_id, request_id, m).await?;
        let scope = tenant_scope(chat.tenant_id);
        let (original, att_ids, images) = {
            let conn = self.db.conn()?;
            let original = messages::find_by_request(&conn, &scope, chat_id, request_id, "user")
                .await?
                .ok_or_else(|| DomainError::internal("turn has no user message"))?;
            let linked = attachments::linked_ids(&conn, &scope, chat_id, original.id).await?;
            let live = attachments::find_many_in_chat(&conn, &scope, chat_id, &linked).await?;
            let att_ids: Vec<Uuid> = linked
                .iter()
                .filter(|id| live.iter().any(|a| a.id == **id))
                .copied()
                .collect();
            let images: Vec<attachment::Model> = att_ids
                .iter()
                .filter_map(|id| live.iter().find(|a| a.id == *id))
                .filter(|a| a.attachment_kind == attachments::KIND_IMAGE)
                .cloned()
                .collect();
            (original, att_ids, images)
        };
        let content = match m {
            Mutation::Edit(c) => c.clone(),
            _ => original.content.clone(),
        };
        let snapshot = self.policy.current_snapshot(ctx.subject_id()).await?;
        check_chat_model(&snapshot, &chat)?;
        let inputs = TurnInputs {
            chat: chat.clone(),
            content: content.clone(),
            images,
            web_search: target.web_search_enabled,
        };
        // Preflight before the mutation: a rejection leaves the turn intact.
        let pre = self.preflight(ctx, &inputs, snapshot).await?;

        // Mutation commit.
        let new_request_id = Uuid::new_v4();
        let new_turn_id = Uuid::new_v4();
        let user = ctx.subject_id();
        let tenant = chat.tenant_id;
        let event_type = if matches!(m, Mutation::Retry) {
            "turn_retry"
        } else {
            "turn_edit"
        };
        let commit = with_retry(|| {
            let scope = scope.clone();
            let target = target.clone();
            let original = original.clone();
            let content = content.clone();
            let att_ids = att_ids.clone();
            let ob = self.outbox.clone();
            self.db.transaction(move |tx| {
                Box::pin(async move {
                    let now = now_ts();
                    let latest = turns::latest(tx, &scope, chat_id).await?;
                    if latest.as_ref().is_none_or(|l| l.id != target.id) {
                        return Err(not_latest());
                    }
                    if !turns::soft_delete(tx, &scope, target.id, Some(new_request_id), now).await?
                    {
                        return Err(not_latest());
                    }
                    messages::soft_delete_by_request(tx, &scope, chat_id, request_id, now).await?;
                    invalidate_summary(tx, &scope, chat_id, Some(&original)).await?;
                    let user_msg = messages::new_model(
                        Uuid::new_v4(),
                        tenant,
                        chat_id,
                        new_request_id,
                        "user",
                        content,
                        now,
                    );
                    messages::insert(tx, &scope, &user_msg).await?;
                    attachments::link_to_message(
                        tx,
                        &scope,
                        tenant,
                        chat_id,
                        user_msg.id,
                        &att_ids,
                        now,
                    )
                    .await?;
                    chats::touch(tx, &scope, chat_id, now).await?;
                    let turn = chat_turn::Model {
                        id: new_turn_id,
                        tenant_id: tenant,
                        chat_id,
                        request_id: new_request_id,
                        requester_type: "user".to_owned(),
                        requester_user_id: Some(user),
                        state: turns::STATE_RUNNING.to_owned(),
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
                        last_progress_at: Some(now),
                        web_search_enabled: target.web_search_enabled,
                        web_search_completed_count: 0,
                        code_interpreter_completed_count: 0,
                        file_search_completed_count: 0,
                        completed_at: None,
                        updated_at: now,
                    };
                    turns::insert(tx, &scope, &turn).await.map_err(|e| {
                        if e.is_unique_violation() {
                            DomainError::aborted(
                                Res::Turn,
                                "GENERATION_IN_PROGRESS",
                                "Another generation is in progress",
                            )
                        } else {
                            e
                        }
                    })?;
                    let ev = AuditEvent::Mutation(TurnMutationAuditEvent {
                        event_type: event_type.to_owned(),
                        timestamp: OffsetDateTime::now_utc(),
                        tenant_id: tenant,
                        actor_user_id: user,
                        chat_id,
                        original_request_id: Some(request_id),
                        new_request_id: Some(new_request_id),
                        request_id: None,
                    });
                    let wake =
                        ob.enqueue(tx, Queue::Audit, tenant, &ev)
                            .await
                            .map_err(|e| match e {
                                DomainError::InvalidFormat(m) => DomainError::internal(m),
                                other => other,
                            })?;
                    Ok(vec![wake])
                })
            })
        })
        .await;
        let wakes = match commit {
            Ok(w) => w,
            Err(e) if e.is_unique_violation() => {
                return Err(DomainError::aborted(
                    Res::Turn,
                    "GENERATION_IN_PROGRESS",
                    "Another generation is in progress",
                ));
            }
            Err(e) => return Err(e),
        };
        outbox::fire(wakes);

        // After the commit: context assembly, provider resolution, reserve.
        let plan = match self.build_plan(ctx, &inputs, pre).await {
            Ok(p) => p,
            Err(PlanError::ContextBudget) => {
                self.fail_unstarted(tenant, new_turn_id, "context_length_exceeded")
                    .await;
                return Err(PlanError::ContextBudget.into_domain());
            }
            Err(PlanError::Other(e)) => {
                self.fail_unstarted(tenant, new_turn_id, "turn_setup_failed")
                    .await;
                return Err(e);
            }
        };
        let fields = PreflightFields {
            reserve_tokens: plan.pre.decision.reserve.reserve_tokens,
            max_output_tokens_applied: i32::try_from(
                plan.pre.decision.reserve.max_output_tokens_applied,
            )
            .unwrap_or(i32::MAX),
            reserved_credits_micro: plan.pre.decision.reserve.reserved_credits_micro,
            policy_version_applied: i64::try_from(plan.pre.snapshot.policy_version)
                .unwrap_or(i64::MAX),
            effective_model: plan.pre.decision.effective.id.clone(),
            minimal_generation_floor_applied: i32::try_from(plan.pre.floor_applied)
                .unwrap_or(i32::MAX),
        };
        let reserve = with_retry(|| {
            let scope = scope.clone();
            let fields = fields.clone();
            let plan = plan.clone();
            self.db.transaction(move |tx| {
                Box::pin(async move {
                    let now = now_ts();
                    apply_reserve(
                        tx,
                        tenant,
                        user,
                        plan.pre.periods,
                        plan.pre.decision.tier,
                        plan.pre.decision.reserve.reserved_credits_micro,
                        &plan.pre.limits,
                        now,
                    )
                    .await?;
                    if !turns::fill_preflight(tx, &scope, new_turn_id, &fields, now).await? {
                        return Err(DomainError::internal(
                            "retry/edit turn is no longer running",
                        ));
                    }
                    Ok(())
                })
            })
        })
        .await;
        if let Err(e) = reserve {
            let code = if matches!(e, DomainError::QuotaExceeded(_)) {
                "quota_exceeded"
            } else {
                "turn_setup_failed"
            };
            self.fail_unstarted(tenant, new_turn_id, code).await;
            return Err(e);
        }
        Ok(TurnStart::Live(self.launch(
            ctx,
            &chat,
            new_turn_id,
            new_request_id,
            plan,
        )))
    }

    /// Fail a retry/edit turn whose setup failed before the reserve (no
    /// settlement, no outbox event).
    async fn fail_unstarted(&self, tenant: Uuid, turn_id: Uuid, code: &str) {
        let Ok(conn) = self.db.conn() else { return };
        let res = turns::cas_finalize(
            &conn,
            &tenant_scope(tenant),
            turn_id,
            &Terminal {
                state: turns::STATE_FAILED,
                error_code: Some(code.to_owned()),
                ..Terminal::default()
            },
            now_ts(),
        )
        .await;
        if let Err(e) = res {
            tracing::warn!(error = %e, %turn_id, "failed to mark the unstarted turn failed");
        }
    }
}
