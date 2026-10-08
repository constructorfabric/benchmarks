//! `messages:stream` setup: idempotency, the parallel-turn guard, preflight,
//! context assembly, the reserve transaction and the provider task launch.
//! Replay of a completed turn is a separate read-only path.

use std::collections::HashSet;
use std::sync::Arc;

use mini_chat_sdk::PolicySnapshot;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use super::finalize::{FinalizeCtx, ReserveFields, apply_reserve};
use super::plan::{TurnInputs, TurnPlan, image_attachments};
use super::run::TurnRun;
use super::{DeltaKind, DoneView, LiveTurn, SendRequest, StreamEvent, TurnStart};
use crate::domain::error::{DomainError, Res};
use crate::domain::quota::multipliers;
use crate::domain::service::Services;
use crate::infra::db::entities::{chat, chat_turn};
use crate::infra::db::repo::{attachments, chats, messages, turns};
use crate::infra::db::{now_ts, tenant_scope, with_retry};

/// Validate message content (non-empty after trim).
pub fn validate_content(content: &str) -> Result<(), DomainError> {
    if content.trim().is_empty() {
        return Err(DomainError::invalid(
            Res::Message,
            "content",
            "EMPTY_CONTENT",
            "Message content must not be empty",
        ));
    }
    Ok(())
}

/// The chat's selected model must still be in the catalog (it may be
/// disabled: the cascade then downgrades with `model_disabled`).
pub fn check_chat_model(snapshot: &PolicySnapshot, chat: &chat::Model) -> Result<(), DomainError> {
    if snapshot.find_model(&chat.model).is_none() {
        return Err(DomainError::invalid_model(Res::Chat));
    }
    Ok(())
}

impl Services {
    /// Replay of a completed, non-deleted turn (side-effect-free).
    pub async fn replay(
        &self,
        chat: &chat::Model,
        turn: &chat_turn::Model,
    ) -> Result<Vec<StreamEvent>, DomainError> {
        let conn = self.db.conn()?;
        let scope = tenant_scope(chat.tenant_id);
        let msg = match turn.assistant_message_id {
            Some(id) => messages::find_by_id(&conn, &scope, chat.id, id).await?,
            None => None,
        };
        let msg = match msg {
            Some(m) => m,
            None => messages::find_by_request(&conn, &scope, chat.id, turn.request_id, "assistant")
                .await?
                .ok_or_else(|| DomainError::internal("completed turn has no assistant message"))?,
        };
        let effective = turn
            .effective_model
            .clone()
            .or_else(|| msg.model.clone())
            .unwrap_or_else(|| chat.model.clone());
        let downgrade = effective != chat.model;
        Ok(vec![
            StreamEvent::StreamStarted {
                request_id: turn.request_id,
                message_id: msg.id,
                is_new_turn: false,
                thread_summary_tokens: None,
            },
            StreamEvent::Delta {
                kind: DeltaKind::Text,
                content: msg.content.clone(),
            },
            StreamEvent::Done(DoneView {
                input_tokens: msg.input_tokens,
                output_tokens: msg.output_tokens,
                effective_model: effective,
                selected_model: chat.model.clone(),
                downgrade,
                downgrade_from: downgrade.then(|| chat.model.clone()),
                downgrade_reason: None,
                quota_warnings: None,
            }),
        ])
    }

    /// Set up a `messages:stream` turn.
    #[allow(clippy::too_many_lines)]
    pub async fn start_send(
        self: &Arc<Self>,
        ctx: &SecurityContext,
        chat_id: Uuid,
        req: SendRequest,
    ) -> Result<TurnStart, DomainError> {
        validate_content(&req.content)?;
        let chat = self.load_chat(ctx, "send_message", chat_id).await?;
        let snapshot = self.policy.current_snapshot(ctx.subject_id()).await?;
        check_chat_model(&snapshot, &chat)?;
        let scope = tenant_scope(chat.tenant_id);
        let request_id = req.request_id.unwrap_or_else(Uuid::new_v4);
        {
            let conn = self.db.conn()?;
            if let Some(t) = turns::find_by_request(&conn, &scope, chat.id, request_id).await? {
                if t.deleted_at.is_none() && t.state == turns::STATE_COMPLETED {
                    return Ok(TurnStart::Replay(self.replay(&chat, &t).await?));
                }
                tracing::info!(turn_id = %t.id, state = %t.state, "request_id reused");
                return Err(DomainError::request_id_conflict());
            }
            if turns::find_running(&conn, &scope, chat.id).await?.is_some() {
                return Err(DomainError::turn_already_running());
            }
        }
        let max_ids =
            (self.cfg.rag.max_documents_per_chat + self.cfg.rag.max_images_per_message) as usize;
        if req.attachment_ids.len() > max_ids {
            return Err(DomainError::invalid_attachment(format!(
                "At most {max_ids} attachment ids per message"
            )));
        }
        let unique: HashSet<Uuid> = req.attachment_ids.iter().copied().collect();
        if unique.len() != req.attachment_ids.len() {
            return Err(DomainError::invalid_attachment("Duplicate attachment ids"));
        }
        let images = image_attachments(self, &chat, &req.attachment_ids).await?;
        let inputs = TurnInputs {
            chat: chat.clone(),
            content: req.content.clone(),
            images,
            web_search: req.web_search,
        };
        let pre = self.preflight(ctx, &inputs, snapshot).await?;
        let plan = self
            .build_plan(ctx, &inputs, pre)
            .await
            .map_err(super::plan::PlanError::into_domain)?;

        // Reserve transaction: quota reserve + re-check, user message,
        // attachment validation and links, running turn.
        let turn_id = Uuid::new_v4();
        let user = ctx.subject_id();
        let tenant = chat.tenant_id;
        let att_ids = req.attachment_ids.clone();
        let web_search = req.web_search;
        let content = req.content.clone();
        let res = with_retry(|| {
            let plan = plan.clone();
            let att_ids = att_ids.clone();
            let content = content.clone();
            let scope = scope.clone();
            self.db.transaction(move |tx| {
                Box::pin(async move {
                    let now = now_ts();
                    let r = &plan.pre.decision.reserve;
                    apply_reserve(
                        tx,
                        tenant,
                        user,
                        plan.pre.periods,
                        plan.pre.decision.tier,
                        r.reserved_credits_micro,
                        &plan.pre.limits,
                        now,
                    )
                    .await?;
                    let user_msg = messages::new_model(
                        Uuid::new_v4(),
                        tenant,
                        chat_id,
                        request_id,
                        "user",
                        content,
                        now,
                    );
                    messages::insert(tx, &scope, &user_msg).await?;
                    chats::touch(tx, &scope, chat_id, now).await?;
                    validate_attachment_ids(tx, &scope, chat_id, user, &att_ids).await?;
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
                    let turn = chat_turn::Model {
                        id: turn_id,
                        tenant_id: tenant,
                        chat_id,
                        request_id,
                        requester_type: "user".to_owned(),
                        requester_user_id: Some(user),
                        state: turns::STATE_RUNNING.to_owned(),
                        provider_name: None,
                        provider_response_id: None,
                        assistant_message_id: None,
                        error_code: None,
                        reserve_tokens: Some(r.reserve_tokens),
                        max_output_tokens_applied: Some(
                            i32::try_from(r.max_output_tokens_applied).unwrap_or(i32::MAX),
                        ),
                        reserved_credits_micro: Some(r.reserved_credits_micro),
                        policy_version_applied: Some(
                            i64::try_from(plan.pre.snapshot.policy_version).unwrap_or(i64::MAX),
                        ),
                        effective_model: Some(plan.pre.decision.effective.id.clone()),
                        minimal_generation_floor_applied: Some(
                            i32::try_from(plan.pre.floor_applied).unwrap_or(i32::MAX),
                        ),
                        error_detail: None,
                        deleted_at: None,
                        replaced_by_request_id: None,
                        started_at: now,
                        last_progress_at: Some(now),
                        web_search_enabled: web_search,
                        web_search_completed_count: 0,
                        code_interpreter_completed_count: 0,
                        file_search_completed_count: 0,
                        completed_at: None,
                        updated_at: now,
                    };
                    turns::insert(tx, &scope, &turn).await?;
                    Ok(())
                })
            })
        })
        .await;
        if let Err(e) = res {
            if e.is_unique_violation() {
                let conn = self.db.conn()?;
                if turns::find_by_request(&conn, &scope, chat.id, request_id)
                    .await?
                    .is_some()
                {
                    return Err(DomainError::request_id_conflict());
                }
                return Err(DomainError::turn_already_running());
            }
            if let DomainError::QuotaExceeded(_) = &e {
                self.metrics.inc(
                    "quota_preflight",
                    &[
                        ("decision", "reject"),
                        ("model", &chat.model),
                        ("tier", "recheck"),
                    ],
                );
            }
            return Err(e);
        }
        for (pt, _) in plan.pre.periods.list() {
            self.metrics.inc("quota_reserve", &[("period", pt)]);
        }
        #[allow(clippy::cast_precision_loss)]
        self.metrics
            .record("image_inputs_per_turn", inputs.images.len() as f64, &[]);
        Ok(TurnStart::Live(
            self.launch(ctx, &chat, turn_id, request_id, plan),
        ))
    }

    /// Spawn the provider task of a turn whose reserve is booked.
    pub fn launch(
        self: &Arc<Self>,
        ctx: &SecurityContext,
        chat: &chat::Model,
        turn_id: Uuid,
        request_id: Uuid,
        plan: TurnPlan,
    ) -> LiveTurn {
        let message_id = Uuid::new_v4();
        let (tx, rx) = mpsc::channel(usize::from(self.cfg.streaming.sse_channel_capacity));
        let cancel = CancellationToken::new();
        let d = &plan.pre.decision;
        let (in_mult, out_mult) = multipliers(&d.effective);
        let fc = FinalizeCtx {
            tenant_id: chat.tenant_id,
            user_id: ctx.subject_id(),
            chat_id: chat.id,
            turn_id,
            request_id,
            message_id,
            selected_model: chat.model.clone(),
            effective_model: d.effective.id.clone(),
            tier: d.tier,
            in_mult,
            out_mult,
            policy_version: plan.pre.snapshot.policy_version,
            reserve: ReserveFields {
                reserve_tokens: d.reserve.reserve_tokens,
                max_output_tokens_applied: d.reserve.max_output_tokens_applied,
                reserved_credits_micro: d.reserve.reserved_credits_micro,
                floor_applied: plan.pre.floor_applied,
            },
            periods: plan.pre.periods,
            limits: plan.pre.limits.clone(),
            downgrade: d.downgrade,
            downgrade_reason: d.downgrade_reason.map(str::to_owned),
            started: std::time::Instant::now(),
            summary_trigger: crate::domain::context::summary_trigger(
                &plan.context,
                plan.summary.is_some(),
                self.cfg.thread_summary_worker.compression_threshold_pct,
            ),
        };
        let thread_summary_tokens = plan.context.summary_token_estimate.map(|est| {
            plan.summary
                .as_ref()
                .map(|s| i64::from(s.token_estimate))
                .filter(|t| *t > 0)
                .unwrap_or(est)
        });
        let run = TurnRun {
            svc: self.clone(),
            fc,
            request: plan.request,
            target: plan.target,
            knowledge: plan.knowledge,
            file_map: plan.pre.attachments.file_map,
            tx,
            cancel: cancel.clone(),
        };
        tokio::spawn(run.run());
        LiveTurn {
            started: StreamEvent::StreamStarted {
                request_id,
                message_id,
                is_new_turn: true,
                thread_summary_tokens,
            },
            rx,
            cancel,
            ping_interval_secs: u64::from(self.cfg.streaming.sse_ping_interval_seconds),
        }
    }
}

/// Validate `attachment_ids` inside the reserve transaction: same tenant (by
/// scope), same chat, uploaded by the user, `ready`.
pub async fn validate_attachment_ids(
    runner: &(impl toolkit_db::secure::DBRunner + Sync),
    scope: &toolkit_security::AccessScope,
    chat_id: Uuid,
    user: Uuid,
    ids: &[Uuid],
) -> Result<(), DomainError> {
    if ids.is_empty() {
        return Ok(());
    }
    let found = attachments::find_many_in_chat(runner, scope, chat_id, ids).await?;
    for id in ids {
        let Some(a) = found.iter().find(|a| a.id == *id) else {
            return Err(DomainError::invalid_attachment(format!(
                "Unknown attachment {id}"
            )));
        };
        if a.uploaded_by_user_id != user {
            return Err(DomainError::invalid_attachment(format!(
                "Unknown attachment {id}"
            )));
        }
        if a.status != attachments::STATUS_READY {
            return Err(DomainError::invalid_attachment(format!(
                "Attachment {id} is not ready"
            )));
        }
    }
    Ok(())
}
