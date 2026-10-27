//! Turn finalization: the CAS-guarded settlement contract (DESIGN.md
//! section 5.7). Every terminal path of a turn that took a quota reserve
//! performs, in one DB transaction: the CAS guard, quota settlement, and
//! the outbox enqueue (usage + audit).

use std::sync::Arc;

use mini_chat_sdk::{ToolCalls, TurnAuditEvent, UsageEvent, UsageTokens};
use toolkit_db::outbox::Wake;
use uuid::Uuid;

use crate::api::rest::dto::fmt_ts;
use crate::domain::quota_service::{QuotaService, SettlementKind, compute_settlement};
use crate::domain::services::ServiceContext;
use crate::error::DomainError;
use crate::infra::storage::entity::chat_turns;
use crate::infra::storage::repo::messages::NewMessage;
use crate::infra::outbox::OutboxEnqueuer;
use crate::infra::storage::repo::quota::bucket;

/// The terminal classification of a stream.
#[derive(Debug, Clone)]
pub enum TerminalInput {
    /// Provider returned a terminal success (`completed`/`incomplete`).
    Completed {
        /// Full assistant text.
        text: String,
        /// Provider usage.
        usage: Option<UsageTokens>,
        /// Provider response id.
        provider_response_id: Option<String>,
        /// Normalized incomplete reason.
        incomplete_reason: Option<String>,
    },
    /// Terminal failure.
    Failed {
        /// Internal error code.
        error_code: String,
        /// Non-sensitive diagnostic detail.
        error_detail: Option<String>,
        /// Provider usage when known.
        usage: Option<UsageTokens>,
        /// Provider response id when known.
        provider_response_id: Option<String>,
    },
    /// Client disconnect.
    Cancelled {
        /// Partial text accumulated before the disconnect.
        partial_text: String,
    },
}

/// Completed tool-call counts of a turn.
#[derive(Debug, Clone, Copy, Default)]
pub struct ToolCallCounts {
    /// Completed web search calls.
    pub web_search: i32,
    /// Completed code interpreter calls.
    pub code_interpreter: i32,
    /// Completed file search calls (provider-native + knowledge
    /// retrievals).
    pub file_search: i32,
}

/// Quota/downgrade metadata carried from preflight.
#[derive(Debug, Clone)]
pub struct QuotaMeta {
    /// `allow` | `downgrade`.
    pub decision: &'static str,
    /// Downgrade source model (the selected model) when downgraded.
    pub downgrade_from: Option<String>,
    /// Downgrade reason when downgraded.
    pub downgrade_reason: Option<&'static str>,
    /// Daily period start at preflight.
    pub daily_start: time::Date,
    /// Monthly period start at preflight.
    pub monthly_start: time::Date,
}

/// The finalization outcome.
#[derive(Debug)]
pub enum FinalizeOutcome {
    /// This finalizer won the CAS; the flag reports whether a thread-summary
    /// work item was scheduled.
    Won(bool),
    /// Another finalizer won.
    Lost,
    /// The finalization transaction failed (turn stays running).
    TxFailed,
    /// The assistant message could not be persisted; the turn was finalized
    /// as `failed` (`message_persistence_failed`).
    PersistenceFailed,
}

/// The finalization service.
pub struct FinalizeService {
    ctx: Arc<ServiceContext>,
}

impl FinalizeService {
    /// Create the service.
    #[must_use]
    pub fn new(ctx: Arc<ServiceContext>) -> Self {
        Self { ctx }
    }

    /// Current `OTel` trace id of the caller span, when active.
    #[must_use]
    pub fn current_trace_id() -> Option<String> {
        use tracing_opentelemetry::OpenTelemetrySpanExt;
        let span = tracing::Span::current();
        let otel_ctx = span.context();
        let span_ref = opentelemetry::trace::TraceContextExt::span(&otel_ctx);
        let trace = span_ref.span_context().trace_id();
        let hex = format!("{trace:x}");
        if hex == "0" || hex.chars().all(|c| c == '0') {
            None
        } else {
            Some(hex)
        }
    }

    /// Finalize a turn from the stream path. Returns the outcome and the
    /// quota warnings computed after commit (on the won-completed path).
    ///
    /// The `settle` flag distinguishes stream finalization (true) from
    /// retry/edit unstarted turns (false — no reserve, no settlement, no
    /// outbox event).
    pub async fn finalize(
        &self,
        turn: &chat_turns::Model,
        selected_model: &str,
        quota: &QuotaMeta,
        input: TerminalInput,
        counts: ToolCallCounts,
        latency_ms: u64,
        #[allow(clippy::option_option, reason = "None = not evaluated; Some(None) = fired with no base frontier")]
        summary_trigger: Option<Option<crate::infra::storage::repo::MessageKey>>,
    ) -> (FinalizeOutcome, Option<Vec<crate::api::rest::dto::QuotaWarning>>) {
        let kind = match &input {
            TerminalInput::Completed { .. } => SettlementKind::Completed,
            TerminalInput::Failed { .. } => SettlementKind::Failed,
            TerminalInput::Cancelled { .. } => SettlementKind::Aborted,
        };
        let ctx = Arc::clone(&self.ctx);
        let turn_id = turn.id;
        let result = self
            .ctx
            .db
            .transaction(|tx| {
                let ctx = Arc::clone(&ctx);
                let turn = turn.clone();
                let input = input.clone();
                let quota = quota.clone();
                let selected_model = selected_model.to_owned();
                Box::pin(async move {
                    Self::finalize_tx(&ctx, tx, &turn, &selected_model, &quota, input, counts, kind, latency_ms, summary_trigger)
                        .await
                })
            })
            .await;
        match result {
            Ok((outcome, wakes)) => {
                for wake in wakes {
                    wake.fire();
                }
                match outcome {
                    FinalizeOutcome::Won(_) => {
                        let warnings = if matches!(input, TerminalInput::Completed { .. }) {
                            self.quota_warnings(turn).await.ok().flatten()
                        } else {
                            None
                        };
                        (outcome, warnings)
                    }
                    _ => (outcome, None),
                }
            }
            Err(err) => {
                tracing::error!("mini-chat: finalization transaction failed for turn {turn_id}: {err}");
                (FinalizeOutcome::TxFailed, None)
            }
        }
    }

#[allow(clippy::cognitive_complexity, reason = "the settlement contract is one normative transaction")]
#[allow(clippy::too_many_lines, reason = "the settlement contract is one normative transaction")]
    #[allow(clippy::too_many_arguments)]
    async fn finalize_tx(
        ctx: &Arc<ServiceContext>,
        tx: &impl toolkit_db::secure::DBRunner,
        turn: &chat_turns::Model,
        selected_model: &str,
        quota: &QuotaMeta,
        input: TerminalInput,
        counts: ToolCallCounts,
        kind: SettlementKind,
        latency_ms: u64,
        summary_trigger: Option<Option<crate::infra::storage::repo::MessageKey>>,
    ) -> Result<(FinalizeOutcome, Vec<Wake>), DomainError> {
        let scope = toolkit_db::secure::AccessScope::allow_all();
        let mut wakes: Vec<Wake> = Vec::new();

        // Settlement computation needs the policy snapshot of the turn's
        // applied version (inside the finalization transaction).
        let requester_user_id = turn.requester_user_id.unwrap_or(Uuid::nil());
        let snapshot = QuotaService::new(Arc::clone(ctx))
            .snapshot_for_settlement(
                requester_user_id,
                turn.policy_version_applied.unwrap_or(0),
            )
            .await?;
        let model_entry = turn
            .effective_model
            .as_deref()
            .and_then(|m| snapshot.model(m));
        let (in_mult, out_mult) = model_entry.map_or((1, 1), |m| {
            (
                m.input_tokens_credit_multiplier_micro,
                m.output_tokens_credit_multiplier_micro,
            )
        });
        let effective_tier = model_entry.map(|m| m.tier);

        let usage = match &input {
            TerminalInput::Completed { usage, .. } | TerminalInput::Failed { usage, .. } => *usage,
            TerminalInput::Cancelled { .. } => None,
        };
        let settlement = compute_settlement(
            kind,
            turn.reserve_tokens.unwrap_or(0),
            i64::from(turn.max_output_tokens_applied.unwrap_or(0)),
            i64::from(turn.minimal_generation_floor_applied.unwrap_or(0)),
            turn.reserved_credits_micro.unwrap_or(0),
            usage,
            in_mult,
            out_mult,
            ctx.config.quota.overshoot_tolerance_factor,
        )
        .map_err(|e| {
            tracing::warn!("mini-chat: credit computation failed at settlement: {e}");
            DomainError::Internal
        })?;

        // Persist the assistant message (completed always; cancelled with
        // non-empty partial text).
        let mut assistant_message_id: Option<Uuid> = None;
        match &input {
            TerminalInput::Completed {
                text,
                usage,
                provider_response_id,
                ..
            } => {
                let msg_id = Uuid::new_v4();
                let insert = ctx
                    .repos
                    .messages
                    .insert(
                        tx,
                        &scope,
                        NewMessage {
                            id: msg_id,
                            tenant_id: turn.tenant_id,
                            chat_id: turn.chat_id,
                            request_id: Some(turn.request_id),
                            role: "assistant".to_owned(),
                            content: text.clone(),
                            model: turn.effective_model.clone(),
                            provider_response_id: provider_response_id.clone(),
                            input_tokens: usage.map_or(0, |u| u.input_tokens),
                            output_tokens: usage.map_or(0, |u| u.output_tokens),
                            cache_read_input_tokens: usage.map_or(0, |u| u.cache_read_input_tokens),
                            cache_write_input_tokens: usage
                                .map_or(0, |u| u.cache_write_input_tokens),
                            reasoning_tokens: usage.map_or(0, |u| u.reasoning_tokens),
                        },
                    )
                    .await;
                match insert {
                    Ok(_) => {}
                    Err(err) => {
                        tracing::error!(
                            "mini-chat: assistant message persistence failed for turn {}: {err}",
                            turn.id
                        );
                        // Downgrade to failed: message_persistence_failed.
                        let won = ctx
                            .repos
                            .turns
                            .finalize_failed(
                                tx,
                                turn.id,
                                "message_persistence_failed",
                                Some("assistant message persistence failed"),
                            )
                            .await?;
                        if !won {
                            return Ok((FinalizeOutcome::Lost, wakes));
                        }
                        Self::settle_and_enqueue(
                            ctx,
                            tx,
                            turn,
                            selected_model,
                            quota,
                            settlement,
                            *usage,
                            counts,
                            latency_ms,
                            "failed",
                            Some("message_persistence_failed"),
                            effective_tier,
                            &mut wakes,
                        )
                        .await?;
                        return Ok((FinalizeOutcome::PersistenceFailed, wakes));
                    }
                }
                let won = ctx
                    .repos
                    .turns
                    .finalize_completed(tx, turn.id, msg_id, provider_response_id.clone())
                    .await?;
                if !won {
                    return Ok((FinalizeOutcome::Lost, wakes));
                }
            }
            TerminalInput::Failed {
                error_code,
                error_detail,
                provider_response_id,
                ..
            } => {
                let won = ctx
                    .repos
                    .turns
                    .finalize_failed(tx, turn.id, error_code, error_detail.as_deref())
                    .await?;
                if !won {
                    return Ok((FinalizeOutcome::Lost, wakes));
                }
                if let Some(prid) = provider_response_id {
                    drop(prid);
                }
            }
            TerminalInput::Cancelled { partial_text } => {
                if !partial_text.is_empty() {
                    let msg_id = Uuid::new_v4();
                    let insert = ctx
                        .repos
                        .messages
                        .insert(
                            tx,
                            &scope,
                            NewMessage {
                                id: msg_id,
                                tenant_id: turn.tenant_id,
                                chat_id: turn.chat_id,
                                request_id: Some(turn.request_id),
                                role: "assistant".to_owned(),
                                content: partial_text.clone(),
                                model: turn.effective_model.clone(),
                                provider_response_id: None,
                                input_tokens: 0,
                                output_tokens: 0,
                                cache_read_input_tokens: 0,
                                cache_write_input_tokens: 0,
                                reasoning_tokens: 0,
                            },
                        )
                        .await;
                    match insert {
                        Ok(_) => assistant_message_id = Some(msg_id),
                        Err(err) => {
                            // Best-effort only: log and finalize cancelled
                            // without an assistant message.
                            tracing::warn!(
                                "mini-chat: partial message persistence failed for turn {}: {err}",
                                turn.id
                            );
                        }
                    }
                }
                let won = ctx
                    .repos
                    .turns
                    .finalize_cancelled(tx, turn.id, assistant_message_id)
                    .await?;
                if !won {
                    return Ok((FinalizeOutcome::Lost, wakes));
                }
            }
        }

        Self::settle_and_enqueue(
            ctx,
            tx,
            turn,
            selected_model,
            quota,
            settlement,
            usage,
            counts,
            latency_ms,
            match kind {
                SettlementKind::Completed => "completed",
                SettlementKind::Failed => "failed",
                SettlementKind::Aborted => "cancelled",
            },
            match &input {
                TerminalInput::Failed { error_code, .. } => Some(error_code.as_str()),
                _ => None,
            },
            effective_tier,
            &mut wakes,
        )
        .await?;

        // Thread summary scheduling (completed turns only): durable
        // enqueue in the finalization transaction.
        let mut summary_scheduled = false;
        if kind == SettlementKind::Completed
            && let Some(base_frontier) = summary_trigger
        {
            let target = ctx
                .repos
                .messages
                .latest_excluding_turn(tx, turn.tenant_id, turn.chat_id, turn.request_id)
                .await?;
            if let Some(target) = target
                && base_frontier != Some(target)
            {
                let payload = crate::infra::outbox::ThreadSummaryPayload {
                    tenant_id: turn.tenant_id,
                    chat_id: turn.chat_id,
                    system_request_id: Uuid::new_v4(),
                    base_frontier_created_at: base_frontier.map(|b| fmt_ts(&b.0)),
                    base_frontier_message_id: base_frontier.map(|b| b.1),
                    frozen_target_created_at: fmt_ts(&target.0),
                    frozen_target_message_id: target.1,
                    system_task_type: "thread_summary_update".to_owned(),
                };
                wakes.push(ctx.outbox.enqueue_thread_summary(tx, &payload).await?);
                summary_scheduled = true;
            }
        }
        Ok((FinalizeOutcome::Won(summary_scheduled), wakes))
    }

    /// Quota settlement + usage and audit outbox enqueue (CAS-winner only).
    #[allow(clippy::too_many_arguments)]
    async fn settle_and_enqueue(
        ctx: &Arc<ServiceContext>,
        tx: &impl toolkit_db::secure::DBRunner,
        turn: &chat_turns::Model,
        selected_model: &str,
        quota: &QuotaMeta,
        settlement: crate::domain::quota_service::Settlement,
        usage: Option<UsageTokens>,
        counts: ToolCallCounts,
        latency_ms: u64,
        terminal_state: &str,
        error_code: Option<&str>,
        effective_tier: Option<mini_chat_sdk::ModelTier>,
        wakes: &mut Vec<Wake>,
    ) -> Result<(), DomainError> {
        let requester_user_id = turn.requester_user_id.unwrap_or(Uuid::nil());
        let effective_is_premium = effective_tier == Some(mini_chat_sdk::ModelTier::Premium);
        let keys = [
            (quota.daily_start, period_daily()),
            (quota.monthly_start, period_monthly()),
        ];
        for (start, ptype) in keys {
            ctx.repos
                .quota
                .settle(
                    tx,
                    turn.tenant_id,
                    requester_user_id,
                    crate::infra::storage::repo::quota::BucketKey {
                        period_type: ptype,
                        period_start: start,
                        bucket: bucket::TOTAL,
                    },
                    turn.reserved_credits_micro.unwrap_or(0),
                    settlement.committed_credits_micro,
                    usage.map_or(0, |u| u.input_tokens),
                    usage.map_or(0, |u| u.output_tokens),
                    counts.web_search,
                    counts.code_interpreter,
                )
                .await?;
            if effective_is_premium {
                ctx.repos
                    .quota
                    .settle(
                        tx,
                        turn.tenant_id,
                        requester_user_id,
                        crate::infra::storage::repo::quota::BucketKey {
                            period_type: ptype,
                            period_start: start,
                            bucket: bucket::PREMIUM,
                        },
                        turn.reserved_credits_micro.unwrap_or(0),
                        settlement.committed_credits_micro,
                        0,
                        0,
                        0,
                        0,
                    )
                    .await?;
            }
        }

        // Quota metrics: reserve/commit/overshoot per period.
        for ptype in [
            crate::infra::storage::repo::quota::period::DAILY,
            crate::infra::storage::repo::quota::period::MONTHLY,
        ] {
            ctx.metrics
                .quota_reserve
                .add(1, &[crate::infra::metrics::label("period", ptype)]);
            if settlement.settlement_method == "actual" {
                ctx.metrics
                    .quota_commit
                    .add(1, &[crate::infra::metrics::label("period", ptype)]);
                if settlement.overshoot {
                    ctx.metrics
                        .quota_overshoot
                        .add(1, &[crate::infra::metrics::label("period", ptype)]);
                }
            }
        }
        if let Some(u) = settlement.usage {
            ctx.metrics
                .quota_actual_tokens
                .record((u.input_tokens + u.output_tokens) as f64, &[]);
        }
        let usage_event = UsageEvent {
            tenant_id: turn.tenant_id,
            user_id: turn.requester_user_id,
            chat_id: turn.chat_id,
            turn_id: Some(turn.id),
            request_id: turn.request_id,
            effective_model: turn.effective_model.clone().unwrap_or_default(),
            selected_model: selected_model.to_owned(),
            terminal_state: terminal_state.to_owned(),
            billing_outcome: settlement.billing_outcome.to_owned(),
            usage: settlement.usage,
            actual_credits_micro: settlement.committed_credits_micro,
            settlement_method: settlement.settlement_method.to_owned(),
            policy_version_applied: turn.policy_version_applied.unwrap_or(0),
            web_search_calls: u32::try_from(counts.web_search).unwrap_or(0),
            code_interpreter_calls: u32::try_from(counts.code_interpreter).unwrap_or(0),
            file_search_calls: u32::try_from(counts.file_search).unwrap_or(0),
            timestamp: fmt_ts(&now()),
            requester_type: turn.requester_type.clone(),
            dedupe_key: dedupe_key(turn.tenant_id, turn.id, turn.request_id),
            system_task_type: None,
        };
        wakes.push(ctx.outbox.enqueue_usage(tx, &usage_event).await?);

        let audit = TurnAuditEvent {
            event_type: if terminal_state == "completed" {
                "turn_completed".to_owned()
            } else {
                "turn_failed".to_owned()
            },
            tenant_id: turn.tenant_id,
            user_id: turn.requester_user_id,
            actor_user_id: None,
            chat_id: turn.chat_id,
            turn_id: Some(turn.id),
            request_id: turn.request_id,
            original_request_id: None,
            new_request_id: None,
            selected_model: selected_model.to_owned(),
            effective_model: turn.effective_model.clone().unwrap_or_default(),
            usage: settlement.usage,
            latency_ms: Some(latency_ms),
            tool_calls: ToolCalls {
                web_search_calls: u64::from(u32::try_from(counts.web_search).unwrap_or(0)),
                file_search_calls: u64::from(u32::try_from(counts.file_search).unwrap_or(0)),
            },
            policy_decisions: mini_chat_sdk::PolicyDecisions {
                quota: mini_chat_sdk::QuotaAuditDecision {
                    decision: quota.decision.to_owned(),
                    downgrade_from: quota.downgrade_from.clone(),
                    downgrade_reason: quota.downgrade_reason.map(str::to_owned),
                },
                license: String::new(),
            },
            error_code: error_code.map(str::to_owned),
            prompt: String::new(),
            response: String::new(),
            attachments: Vec::new(),
            quota_scope: String::new(),
            trace_id: Self::current_trace_id(),
            timestamp: fmt_ts(&now()),
        };
        wakes.push(ctx.outbox.enqueue_audit(tx, &audit).await?);
        if settlement.settlement_method == "actual" && counts.code_interpreter > 0 {
            ctx.metrics.code_interpreter_calls.add(
                u64::try_from(counts.code_interpreter).unwrap_or(0),
                &[crate::infra::metrics::label(
                    "model",
                    turn.effective_model.clone().unwrap_or_default(),
                )],
            );
        }
        Ok(())
    }

    /// Compute quota warnings after a completed settlement.
    async fn quota_warnings(
        &self,
        turn: &chat_turns::Model,
    ) -> Result<Option<Vec<crate::api::rest::dto::QuotaWarning>>, DomainError> {
        let requester = turn.requester_user_id.unwrap_or(Uuid::nil());
        let version = turn.policy_version_applied.unwrap_or(0);
        let limits = self
            .ctx
            .policy
            .user_limits(requester, version)
            .await
            .map_err(|e| {
                tracing::error!("mini-chat: limits lookup for warnings failed: {e}");
                DomainError::Internal
            })?;
        let now = time::OffsetDateTime::now_utc();
        let statuses =
            crate::domain::quota_warnings::compute(&self.ctx, turn.tenant_id, requester, &limits, now)
                .await?;
        Ok(Some(crate::domain::quota_warnings::to_warnings(&statuses)))
    }
}

fn period_daily() -> &'static str {
    crate::infra::storage::repo::quota::period::DAILY
}

fn period_monthly() -> &'static str {
    crate::infra::storage::repo::quota::period::MONTHLY
}

/// Current UTC time.
#[must_use]
pub fn now() -> time::OffsetDateTime {
    time::OffsetDateTime::now_utc()
}

/// Canonical outbox dedupe key: `{tenant}/{turn}/{request}` as 32-char hex.
#[must_use]
pub fn dedupe_key(tenant_id: Uuid, turn_id: Uuid, request_id: Uuid) -> String {
    format!(
        "{}/{}/{}",
        tenant_id.as_simple(),
        turn_id.as_simple(),
        request_id.as_simple()
    )
}
