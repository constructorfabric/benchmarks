//! `POST /v1/chats/{id}/messages:stream` setup and the shared reserve /
//! provider-task launch used by send, retry and edit.

use std::sync::Arc;
use std::time::{Duration, Instant};

use sea_orm::sea_query::Expr;
use sea_orm::{ColumnTrait, Condition, EntityTrait, QueryFilter, Set};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use toolkit_db::secure::{DBRunner, SecureEntityExt, SecureInsertExt, SecureUpdateExt};
use toolkit_security::{AccessScope, SecurityContext};
use uuid::Uuid;

use super::preflight::{QuotaPlan, RequestPlan};
use super::task::TurnRun;
use super::{LiveStream, StreamEvent, StreamStart};
use crate::domain::authz::actions;
use crate::domain::billing::Reserve;
use crate::domain::error::DomainError;
use crate::domain::quota::reserve_and_recheck;
use crate::domain::service::MiniChat;
use crate::infra::db::entity::{attachments, chat_turns, chats, message_attachments, messages};
use crate::infra::db::now;

/// Parsed send-message request.
#[derive(Debug, Clone)]
pub struct SendRequest {
    pub content: String,
    pub request_id: Option<Uuid>,
    pub attachment_ids: Vec<Uuid>,
    pub web_search: bool,
}

/// Find a turn by `(chat_id, request_id)` including soft-deleted turns.
///
/// # Errors
/// Database failure.
pub async fn find_turn(
    runner: &impl DBRunner,
    tenant: Uuid,
    chat_id: Uuid,
    request_id: Uuid,
) -> Result<Option<chat_turns::Model>, DomainError> {
    Ok(chat_turns::Entity::find()
        .filter(
            Condition::all()
                .add(chat_turns::Column::ChatId.eq(chat_id))
                .add(chat_turns::Column::RequestId.eq(request_id)),
        )
        .secure()
        .scope_with(&AccessScope::for_tenant(tenant))
        .one(runner)
        .await?)
}

/// Whether a non-deleted turn of the chat is running.
///
/// # Errors
/// Database failure.
pub async fn has_running_turn(runner: &impl DBRunner, tenant: Uuid, chat_id: Uuid) -> Result<bool, DomainError> {
    Ok(chat_turns::Entity::find()
        .filter(
            Condition::all()
                .add(chat_turns::Column::ChatId.eq(chat_id))
                .add(chat_turns::Column::State.eq("running"))
                .add(chat_turns::Column::DeletedAt.is_null()),
        )
        .secure()
        .scope_with(&AccessScope::for_tenant(tenant))
        .count(runner)
        .await?
        > 0)
}

/// Insert a user message and its attachment links.
///
/// # Errors
/// Database failure.
pub async fn insert_user_message(
    tx: &impl DBRunner,
    chat: &chats::Model,
    request_id: Uuid,
    content: &str,
    attachment_ids: &[Uuid],
    ts: time::OffsetDateTime,
) -> Result<Uuid, DomainError> {
    let id = Uuid::new_v4();
    let scope = AccessScope::for_tenant(chat.tenant_id);
    let am = messages::ActiveModel {
        id: Set(id),
        tenant_id: Set(chat.tenant_id),
        chat_id: Set(chat.id),
        request_id: Set(Some(request_id)),
        role: Set("user".into()),
        content: Set(content.to_owned()),
        content_type: Set("text".into()),
        token_estimate: Set(0),
        provider_response_id: Set(None),
        request_kind: Set("chat".into()),
        features_used: Set(serde_json::json!([])),
        input_tokens: Set(0),
        output_tokens: Set(0),
        cache_read_input_tokens: Set(0),
        cache_write_input_tokens: Set(0),
        reasoning_tokens: Set(0),
        model: Set(None),
        is_compressed: Set(false),
        created_at: Set(ts),
        deleted_at: Set(None),
    };
    messages::Entity::insert(am)
        .secure()
        .scope_unchecked(&scope)?
        .exec(tx)
        .await?;
    for aid in attachment_ids {
        let link = message_attachments::ActiveModel {
            tenant_id: Set(chat.tenant_id),
            chat_id: Set(chat.id),
            message_id: Set(id),
            attachment_id: Set(*aid),
            created_at: Set(ts),
        };
        message_attachments::Entity::insert(link)
            .secure()
            .scope_unchecked(&scope)?
            .exec(tx)
            .await?;
    }
    Ok(id)
}

/// Bump `chats.updated_at`.
///
/// # Errors
/// Database failure.
pub async fn touch_chat(tx: &impl DBRunner, chat: &chats::Model, ts: time::OffsetDateTime) -> Result<(), DomainError> {
    chats::Entity::update_many()
        .col_expr(chats::Column::UpdatedAt, Expr::value(ts))
        .filter(chats::Column::Id.eq(chat.id))
        .secure()
        .scope_with(&AccessScope::for_tenant(chat.tenant_id))
        .exec(tx)
        .await?;
    Ok(())
}

/// Re-validate attachment readiness inside a transaction.
///
/// # Errors
/// `invalid_attachment` when an attachment is no longer usable.
pub async fn recheck_attachments(tx: &impl DBRunner, chat: &chats::Model, ids: &[Uuid]) -> Result<(), DomainError> {
    if ids.is_empty() {
        return Ok(());
    }
    let n = attachments::Entity::find()
        .filter(
            Condition::all()
                .add(attachments::Column::ChatId.eq(chat.id))
                .add(attachments::Column::Id.is_in(ids.to_vec()))
                .add(attachments::Column::Status.eq("ready"))
                .add(attachments::Column::DeletedAt.is_null()),
        )
        .secure()
        .scope_with(&AccessScope::for_tenant(chat.tenant_id))
        .count(tx)
        .await?;
    if usize::try_from(n).unwrap_or(0) != ids.len() {
        return Err(DomainError::InvalidAttachment("attachment is no longer usable".into()));
    }
    Ok(())
}

/// Preflight values persisted on the turn.
#[must_use]
pub fn reserve_of(plan: &QuotaPlan) -> Reserve {
    Reserve {
        reserve_tokens: plan.decision.reserve.reserve_tokens,
        max_output_tokens_applied: plan.decision.reserve.max_output_tokens_applied,
        reserved_credits_micro: plan.decision.reserve.reserved_credits_micro,
        minimal_generation_floor_applied: plan.floor_applied,
    }
}

impl MiniChat {
    /// Build the run descriptor of a committed turn.
    #[must_use]
    // reason: kept as a method for call-site symmetry with `launch`/`start_send`
    #[allow(clippy::too_many_arguments, clippy::unused_self)]
    pub fn turn_run(
        &self,
        ctx: &SecurityContext,
        chat: &chats::Model,
        turn_id: Uuid,
        request_id: Uuid,
        assistant_message_id: Uuid,
        plan: &QuotaPlan,
        req: RequestPlan,
    ) -> TurnRun {
        let eff = &plan.decision.effective;
        TurnRun {
            turn_id,
            request_id,
            chat_id: chat.id,
            tenant_id: chat.tenant_id,
            user_id: ctx.subject_id(),
            assistant_message_id,
            selected_model: plan.selected_model.clone(),
            effective_model: eff.id.clone(),
            tier: plan.decision.tier,
            in_mult: eff.input_tokens_credit_multiplier_micro,
            out_mult: eff.output_tokens_credit_multiplier_micro,
            downgraded: plan.decision.downgraded,
            downgrade_reason: plan.decision.downgrade_reason.map(ToOwned::to_owned),
            reserve: reserve_of(plan),
            periods: plan.periods,
            policy_version: plan.snapshot.policy_version,
            provider: req.provider,
            request: req.request,
            knowledge: req.knowledge,
            file_map: plan.facts.file_map.clone(),
            assembled_tokens: req.context.assembled_tokens,
            effective_budget: req.context.effective_budget,
            messages_truncated: req.context.messages_truncated,
            has_summary: req.has_summary,
            started: Instant::now(),
        }
    }

    /// Spawn the provider task and build the live stream.
    #[must_use]
    pub fn launch(self: &Arc<Self>, run: TurnRun, summary_applied: Option<i64>) -> LiveStream {
        let cap = usize::from(self.cfg.streaming.sse_channel_capacity);
        let (tx, rx) = mpsc::channel(cap);
        let cancel = CancellationToken::new();
        let started = StreamEvent::StreamStarted {
            request_id: run.request_id,
            message_id: run.assistant_message_id,
            is_new_turn: true,
            thread_summary_applied: summary_applied,
        };
        let svc = Arc::clone(self);
        let c = cancel.clone();
        tokio::spawn(async move { svc.run_provider_task(run, tx, c).await });
        LiveStream {
            started,
            rx,
            cancel_guard: cancel.drop_guard(),
            ping_interval: Duration::from_secs(u64::from(self.cfg.streaming.sse_ping_interval_seconds)),
        }
    }

    /// Set up a send-message stream (replay, conflict checks, preflight,
    /// reserve transaction, provider task).
    ///
    /// # Errors
    /// Every pre-stream error (JSON response, no SSE stream).
    #[allow(clippy::too_many_lines)]
    pub async fn start_send(
        self: &Arc<Self>,
        ctx: &SecurityContext,
        chat_id: Uuid,
        req: SendRequest,
    ) -> Result<StreamStart, DomainError> {
        if req.content.trim().is_empty() {
            return Err(DomainError::EmptyContent);
        }
        let (_, chat) = self.authorize_chat(ctx, actions::SEND_MESSAGE, chat_id).await?;
        let conn = self.db.conn()?;
        let request_id = req.request_id.unwrap_or_else(Uuid::new_v4);
        if let Some(t) = find_turn(&conn, chat.tenant_id, chat.id, request_id).await? {
            if t.state == "completed" && t.deleted_at.is_none() {
                return self.replay(&chat, &t).await.map(StreamStart::Replay);
            }
            return Err(DomainError::RequestIdConflict(format!(
                "turn {} is {} (deleted: {})",
                t.id,
                t.state,
                t.deleted_at.is_some()
            )));
        }
        if has_running_turn(&conn, chat.tenant_id, chat.id).await? {
            return Err(DomainError::TurnAlreadyRunning);
        }
        let atts = self.validate_attachments(&conn, ctx, &chat, &req.attachment_ids).await?;
        let images: Vec<attachments::Model> =
            atts.iter().filter(|a| a.attachment_kind == "image").cloned().collect();
        let plan = self
            .quota_preflight(ctx, &chat, &req.content, images.len(), req.web_search)
            .await?;
        let rplan = self
            .build_request(ctx, &chat, &plan, &req.content, &images, Some(request_id))
            .await?;
        let turn_id = Uuid::new_v4();
        let assistant_message_id = Uuid::new_v4();
        let reserve = reserve_of(&plan);
        let floor = plan.floor_applied;
        let eff_model = plan.decision.effective.id.clone();
        let premium = plan.decision.tier == mini_chat_sdk::ModelTier::Premium;
        let periods = plan.periods;
        let limits = plan.limits.clone();
        let policy_version = plan.snapshot.policy_version;
        let ctx2 = ctx.clone();
        let chat2 = chat.clone();
        let content = req.content.clone();
        let att_ids = req.attachment_ids.clone();
        let web_search = req.web_search;
        let res = crate::infra::db::tx_retry(&self.db, move |tx| {
                let att_ids = att_ids.clone();
                let chat2 = chat2.clone();
                let content = content.clone();
                let ctx2 = ctx2.clone();
                let eff_model = eff_model.clone();
                let limits = limits.clone();
                Box::pin(async move {
                    let ts = now();
                    reserve_and_recheck(
                        tx,
                        chat2.tenant_id,
                        ctx2.subject_id(),
                        periods,
                        premium,
                        reserve.reserved_credits_micro,
                        &limits,
                    )
                    .await?;
                    insert_user_message(tx, &chat2, request_id, &content, &att_ids, ts).await?;
                    touch_chat(tx, &chat2, ts).await?;
                    recheck_attachments(tx, &chat2, &att_ids).await?;
                    let turn = chat_turns::ActiveModel {
                        id: Set(turn_id),
                        tenant_id: Set(chat2.tenant_id),
                        chat_id: Set(chat2.id),
                        request_id: Set(request_id),
                        requester_type: Set("user".into()),
                        requester_user_id: Set(Some(ctx2.subject_id())),
                        state: Set("running".into()),
                        provider_name: Set(None),
                        provider_response_id: Set(None),
                        assistant_message_id: Set(None),
                        error_code: Set(None),
                        error_detail: Set(None),
                        reserve_tokens: Set(Some(reserve.reserve_tokens)),
                        max_output_tokens_applied: Set(Some(
                            i32::try_from(reserve.max_output_tokens_applied).unwrap_or(i32::MAX),
                        )),
                        reserved_credits_micro: Set(Some(reserve.reserved_credits_micro)),
                        policy_version_applied: Set(Some(i64::try_from(policy_version).unwrap_or(i64::MAX))),
                        effective_model: Set(Some(eff_model)),
                        minimal_generation_floor_applied: Set(Some(i32::try_from(floor).unwrap_or(i32::MAX))),
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
                    chat_turns::Entity::insert(turn)
                        .secure()
                        .scope_unchecked(&AccessScope::for_tenant(chat2.tenant_id))?
                        .exec(tx)
                        .await?;
                    Ok(())
                })
            })
            .await;
        if let Err(e) = res {
            if e.is_unique_violation() {
                let conn = self.db.conn()?;
                if find_turn(&conn, chat.tenant_id, chat.id, request_id).await?.is_some() {
                    return Err(DomainError::RequestIdConflict("insert race on request_id".into()));
                }
                return Err(DomainError::TurnAlreadyRunning);
            }
            return Err(e);
        }
        let summary_applied = rplan.summary_token_estimate;
        let run = self.turn_run(ctx, &chat, turn_id, request_id, assistant_message_id, &plan, rplan);
        Ok(StreamStart::Live(self.launch(run, summary_applied)))
    }
}
