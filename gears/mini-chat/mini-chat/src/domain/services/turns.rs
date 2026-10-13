//! Turn status and tail-only mutations (DESIGN §3.9): retry, edit, delete.

use std::sync::Arc;

use mini_chat_sdk::{AuditEvent, TurnMutationAuditEvent};
use sea_orm::ActiveValue::Set;
use toolkit_db::DbTx;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use super::MiniChatService;
use super::finalize::to_time;
use super::stream::{LiveStream, TurnSetup, chat_attachment_facts, new_message};
use crate::domain::authz::actions;
use crate::domain::clock;
use crate::domain::error::{DomainError, FeatureSubject};
use crate::domain::models::{TurnState, attachment_kind};
use crate::domain::quota::{PreflightRequest, message_tokens};
use crate::infra::db::entities::{chat_turns, chats};
use crate::infra::db::repo;
use crate::infra::db::repo::turns::{PreflightFields, TerminalUpdate, state};

/// Turn status view.
#[derive(Debug, Clone)]
pub struct TurnStatusView {
    pub request_id: Uuid,
    pub state: TurnState,
    pub error_code: Option<String>,
    pub assistant_message_id: Option<Uuid>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

/// Kind of mutation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MutationKind {
    Retry,
    Edit,
    Delete,
}

impl MutationKind {
    const fn action(self) -> &'static str {
        match self {
            Self::Retry => actions::RETRY_TURN,
            Self::Edit => actions::EDIT_TURN,
            Self::Delete => actions::DELETE_TURN,
        }
    }

    const fn event_type(self) -> &'static str {
        match self {
            Self::Retry => "turn_retry",
            Self::Edit => "turn_edit",
            Self::Delete => "turn_delete",
        }
    }

    const fn op(self) -> &'static str {
        match self {
            Self::Retry => "retry",
            Self::Edit => "edit",
            Self::Delete => "delete",
        }
    }
}

/// Mutation eligibility checks (rules 1–3).
fn check_target(
    target: Option<&chat_turns::Model>,
    latest: Option<&chat_turns::Model>,
    user_id: Uuid,
) -> Result<(), DomainError> {
    let Some(target) = target else {
        return Err(DomainError::TurnNotFound);
    };
    if target.deleted_at.is_some() {
        return Err(DomainError::NotLatestTurn);
    }
    if target.state == state::RUNNING {
        return Err(DomainError::TurnNotTerminal);
    }
    if latest.map(|l| l.id) != Some(target.id) {
        return Err(DomainError::NotLatestTurn);
    }
    if target.requester_user_id.is_some_and(|u| u != user_id) {
        return Err(DomainError::AccessDenied);
    }
    Ok(())
}

impl MiniChatService {
    /// Turn status by request id (404 for soft-deleted turns).
    ///
    /// # Errors
    /// 404 / authorization errors.
    pub async fn turn_status(
        self: &Arc<Self>,
        ctx: &SecurityContext,
        chat_id: Uuid,
        request_id: Uuid,
    ) -> Result<TurnStatusView, DomainError> {
        let (_, chat) = self.authorized_chat(ctx, actions::READ_TURN, chat_id).await?;
        let conn = self.db.conn()?;
        let t = repo::turns::find_by_request(&conn, chat.tenant_id, chat.id, request_id)
            .await?
            .filter(|t| t.deleted_at.is_none())
            .ok_or(DomainError::TurnNotFound)?;
        let st = TurnState::parse(&t.state);
        Ok(TurnStatusView {
            request_id: t.request_id,
            state: st,
            error_code: if st == TurnState::Failed { t.error_code } else { None },
            assistant_message_id: match st {
                TurnState::Completed | TurnState::Cancelled => t.assistant_message_id,
                _ => None,
            },
            updated_at: t.updated_at,
        })
    }

    async fn mutation_preview(
        self: &Arc<Self>,
        ctx: &SecurityContext,
        kind: MutationKind,
        chat_id: Uuid,
        request_id: Uuid,
    ) -> Result<(chats::Model, chat_turns::Model), DomainError> {
        let (_, chat) = self.authorized_chat(ctx, kind.action(), chat_id).await?;
        let conn = self.db.conn()?;
        let target = repo::turns::find_by_request(&conn, chat.tenant_id, chat.id, request_id).await?;
        let latest = repo::turns::latest(&conn, chat.tenant_id, chat.id).await?;
        check_target(target.as_ref(), latest.as_ref(), ctx.subject_id())?;
        let target = target.ok_or(DomainError::TurnNotFound)?;
        Ok((chat, target))
    }

    /// Deletes the latest turn.
    ///
    /// # Errors
    /// Mutation errors (404/409/400/403).
    pub async fn delete_turn(
        self: &Arc<Self>,
        ctx: &SecurityContext,
        chat_id: Uuid,
        request_id: Uuid,
    ) -> Result<(), DomainError> {
        let started = std::time::Instant::now();
        let (chat, target) = self
            .mutation_preview(ctx, MutationKind::Delete, chat_id, request_id)
            .await?;
        let svc = Arc::clone(self);
        let user = ctx.subject_id();
        let res = self
            .transact(move |tx| {
                Box::pin(async move {
                    let now = clock::now();
                    svc.recheck_latest(tx, &chat, target.id).await?;
                    repo::turns::soft_delete(tx, chat.tenant_id, target.id, None, now).await?;
                    let user_msg = repo::messages::by_request(tx, chat.tenant_id, chat.id, target.request_id, "user", false).await?;
                    repo::messages::soft_delete_for_request(tx, chat.tenant_id, chat.id, target.request_id, now).await?;
                    svc.invalidate_summary(tx, &chat, user_msg.as_ref()).await?;
                    let wake = svc
                        .outbox
                        .audit(
                            tx,
                            chat.tenant_id,
                            &mutation_audit(MutationKind::Delete, &chat, user, target.request_id, None),
                        )
                        .await?;
                    Ok(((), wake))
                })
            })
            .await;
        self.record_mutation(MutationKind::Delete, res.is_ok(), started);
        res
    }

    fn record_mutation(&self, kind: MutationKind, ok: bool, started: std::time::Instant) {
        self.metrics.inc(
            "turn_mutation",
            1,
            &[("op", kind.op().to_owned()), ("result", if ok { "ok" } else { "error" }.to_owned())],
        );
        self.metrics.record(
            "turn_mutation_latency_ms",
            started.elapsed().as_secs_f64() * 1000.0,
            &[("op", kind.op().to_owned())],
        );
    }

    async fn recheck_latest(&self, tx: &DbTx<'_>, chat: &chats::Model, target_id: Uuid) -> Result<(), DomainError> {
        let latest = repo::turns::latest(tx, chat.tenant_id, chat.id).await?;
        match latest {
            Some(l) if l.id == target_id => {
                if l.state == state::RUNNING {
                    return Err(DomainError::TurnNotTerminal);
                }
                Ok(())
            }
            _ => Err(DomainError::NotLatestTurn),
        }
    }

    /// Deletes the summary when it covers the mutated turn's user message.
    async fn invalidate_summary(
        &self,
        tx: &DbTx<'_>,
        chat: &chats::Model,
        user_msg: Option<&crate::infra::db::entities::messages::Model>,
    ) -> Result<(), DomainError> {
        let Some(summary) = repo::summaries::find(tx, chat.tenant_id, chat.id).await? else {
            return Ok(());
        };
        let covers = user_msg.is_some_and(|m| {
            (summary.summarized_up_to_created_at, summary.summarized_up_to_message_id) >= (m.created_at, m.id)
        });
        if covers {
            repo::summaries::delete(tx, chat.tenant_id, chat.id).await?;
            repo::messages::clear_compressed(tx, chat.tenant_id, chat.id).await?;
        }
        Ok(())
    }

    /// Retries or edits the latest turn; streams the new answer.
    ///
    /// # Errors
    /// Mutation and preflight errors (JSON Problem responses).
    #[allow(clippy::too_many_lines)]
    pub async fn mutate_turn(
        self: &Arc<Self>,
        ctx: &SecurityContext,
        kind: MutationKind,
        chat_id: Uuid,
        request_id: Uuid,
        new_content: Option<String>,
    ) -> Result<LiveStream, DomainError> {
        let started = std::time::Instant::now();
        if kind == MutationKind::Edit && new_content.as_deref().is_none_or(|c| c.trim().is_empty()) {
            return Err(DomainError::EmptyContent);
        }
        // 1. Preview (read-only).
        let (chat, target) = self.mutation_preview(ctx, kind, chat_id, request_id).await?;
        let user = ctx.subject_id();
        let conn = self.db.conn()?;
        // 2. Model, original message, attachments, preflight.
        let snapshot = self.policy.current_snapshot(user).await?;
        if snapshot.find(&chat.model).is_none() {
            return Err(DomainError::InvalidModel);
        }
        let original = repo::messages::by_request(&conn, chat.tenant_id, chat.id, target.request_id, "user", false)
            .await?
            .ok_or_else(|| DomainError::internal("turn without user message"))?;
        let content = match kind {
            MutationKind::Edit => new_content.unwrap_or_default(),
            _ => original.content.clone(),
        };
        let links = repo::messages::links_for_messages(&conn, chat.tenant_id, chat.id, &[original.id]).await?;
        let att_ids: Vec<Uuid> = links.iter().map(|l| l.attachment_id).collect();
        let atts: Vec<_> = repo::attachments::find_many(&conn, chat.tenant_id, chat.id, &att_ids)
            .await?
            .into_iter()
            .filter(|a| a.deleted_at.is_none())
            .collect();
        let kept_ids: Vec<Uuid> = att_ids
            .iter()
            .copied()
            .filter(|id| atts.iter().any(|a| a.id == *id))
            .collect();
        let image_file_ids = Self::image_file_ids(&atts, &kept_ids);
        let image_count = atts.iter().filter(|a| a.attachment_kind == attachment_kind::IMAGE).count();
        if image_count > usize::try_from(self.cfg.rag.max_images_per_message).unwrap_or(usize::MAX) {
            return Err(DomainError::TooManyImages);
        }
        if target.web_search_enabled && snapshot.kill_switches.disable_web_search {
            return Err(DomainError::FeatureDisabled(FeatureSubject::WebSearch));
        }
        let prior = repo::messages::latest_assistant_with_usage(&conn, chat.tenant_id, chat.id)
            .await?
            .map_or(0, |m| m.input_tokens + m.output_tokens);
        let all = repo::attachments::list_for_chat(&conn, chat.tenant_id, chat.id).await?;
        let facts = chat_attachment_facts(&all);
        let limits = self.policy.user_limits(user, snapshot.policy_version).await?;
        let preq = PreflightRequest {
            selected_model: &chat.model,
            message_bytes: content.len(),
            prior_context_tokens: prior,
            image_count: u32::try_from(image_count).unwrap_or(u32::MAX),
            has_ready_documents: facts.ready_documents,
            has_ready_code_interpreter: !facts.code_interpreter_file_ids.is_empty(),
            web_search_requested: target.web_search_enabled,
        };
        let decision = self
            .quota
            .preflight(&conn, chat.tenant_id, user, &snapshot, &limits, &preq, clock::now())
            .await?;
        let eff = &decision.effective;
        if eff.max_input_tokens > 0 && message_tokens(&content, eff) > i64::from(eff.max_input_tokens) {
            return Err(DomainError::InputTooLong);
        }
        Self::image_guards(&snapshot, eff, image_count)?;
        // 3. Mutation commit.
        let new_request_id = Uuid::new_v4();
        let new_turn_id = Uuid::new_v4();
        let svc = Arc::clone(self);
        let c2 = chat.clone();
        let t2 = target.clone();
        let content2 = content.clone();
        let kept2 = kept_ids.clone();
        let web_search = target.web_search_enabled;
        let commit = self
            .transact(move |tx| {
                Box::pin(async move {
                    let now = clock::now();
                    svc.recheck_latest(tx, &c2, t2.id).await?;
                    repo::turns::soft_delete(tx, c2.tenant_id, t2.id, Some(new_request_id), now).await?;
                    let old_user = repo::messages::by_request(tx, c2.tenant_id, c2.id, t2.request_id, "user", false).await?;
                    repo::messages::soft_delete_for_request(tx, c2.tenant_id, c2.id, t2.request_id, now).await?;
                    let msg_id = Uuid::new_v4();
                    repo::messages::insert(tx, c2.tenant_id, new_message(msg_id, c2.tenant_id, c2.id, new_request_id, "user", &content2, None, now)).await?;
                    if !kept2.is_empty() {
                        repo::messages::link_attachments(tx, c2.tenant_id, c2.id, msg_id, &kept2, now).await?;
                    }
                    let am = chat_turns::ActiveModel {
                        id: Set(new_turn_id),
                        tenant_id: Set(c2.tenant_id),
                        chat_id: Set(c2.id),
                        request_id: Set(new_request_id),
                        requester_type: Set("user".to_owned()),
                        requester_user_id: Set(Some(user)),
                        state: Set(state::RUNNING.to_owned()),
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
                    repo::turns::insert(tx, c2.tenant_id, am).await.map_err(|e| match e {
                        DomainError::UniqueViolation => DomainError::GenerationInProgress,
                        other => other,
                    })?;
                    svc.invalidate_summary(tx, &c2, old_user.as_ref()).await?;
                    repo::chats::touch(tx, c2.tenant_id, c2.id, now).await?;
                    let wake = svc
                        .outbox
                        .audit(tx, c2.tenant_id, &mutation_audit(kind, &c2, user, t2.request_id, Some(new_request_id)))
                        .await?;
                    Ok(((), wake))
                })
            })
            .await;
        self.record_mutation(kind, commit.is_ok(), started);
        commit?;
        // 4. Context, provider resolution and reserve (failures mark the turn failed).
        let setup = TurnSetup {
            chat: chat.clone(),
            request_id: new_request_id,
            turn_id: new_turn_id,
            user_text: content,
            image_file_ids,
            decision,
            limits,
        };
        let prepared = match self.prepare_request(ctx, &setup).await {
            Ok(p) => p,
            Err(e) => {
                let code = if matches!(e, DomainError::ContextBudgetExceeded) {
                    "context_length_exceeded"
                } else {
                    "turn_setup_failed"
                };
                self.fail_unstarted(&chat, new_turn_id, code).await;
                return Err(e);
            }
        };
        let svc = Arc::clone(self);
        let d = setup.decision.clone();
        let l = setup.limits.clone();
        let tenant = chat.tenant_id;
        let reserve = self
            .transact(move |tx| {
                Box::pin(async move {
                    let now = clock::now();
                    svc.quota.reserve(tx, tenant, user, &d, &l, now).await?;
                    repo::turns::set_preflight_fields(
                        tx,
                        tenant,
                        new_turn_id,
                        &PreflightFields {
                            reserve_tokens: d.reserve_tokens,
                            max_output_tokens_applied: i32::try_from(d.max_output_tokens_applied).unwrap_or(i32::MAX),
                            reserved_credits_micro: d.reserved_credits_micro,
                            policy_version_applied: i64::try_from(d.policy_version).unwrap_or(i64::MAX),
                            effective_model: d.effective.id.clone(),
                            minimal_generation_floor_applied: i32::try_from(d.minimal_generation_floor_applied).unwrap_or(i32::MAX),
                        },
                        now,
                    )
                    .await?;
                    Ok(((), toolkit_db::outbox::Wake::empty()))
                })
            })
            .await;
        if let Err(e) = reserve {
            let code = if matches!(e, DomainError::QuotaExceeded(_)) {
                "quota_exceeded"
            } else {
                "turn_setup_failed"
            };
            self.fail_unstarted(&chat, new_turn_id, code).await;
            return Err(e);
        }
        Ok(self.spawn_turn(ctx.clone(), setup, prepared))
    }

    /// Marks an unstarted retry/edit turn failed (no settlement, no outbox).
    async fn fail_unstarted(&self, chat: &chats::Model, turn_id: Uuid, code: &str) {
        let Ok(conn) = self.db.conn() else {
            return;
        };
        repo::turns::cas_finalize(
            &conn,
            chat.tenant_id,
            turn_id,
            &TerminalUpdate {
                state: state::FAILED,
                error_code: Some(code.to_owned()),
                ..TerminalUpdate::default()
            },
            clock::now(),
        )
        .await
        .ok();
    }
}

fn mutation_audit(
    kind: MutationKind,
    chat: &chats::Model,
    actor: Uuid,
    original: Uuid,
    new: Option<Uuid>,
) -> AuditEvent {
    let (original_request_id, new_request_id, request_id) = match kind {
        MutationKind::Delete => (None, None, Some(original)),
        _ => (Some(original), new, None),
    };
    AuditEvent::TurnMutation(TurnMutationAuditEvent {
        event_type: kind.event_type().to_owned(),
        timestamp: to_time(clock::now()),
        tenant_id: chat.tenant_id,
        actor_user_id: actor,
        chat_id: chat.id,
        original_request_id,
        new_request_id,
        request_id,
    })
}
