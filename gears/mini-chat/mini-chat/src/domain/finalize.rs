//! Turn finalization (DESIGN §5.7): CAS on `chat_turns.state`, assistant
//! message persistence, quota settlement and usage/audit outbox events in one
//! transaction.

use std::sync::Arc;

use mini_chat_sdk::audit::{PolicyDecisions, QuotaPolicyDecision, ToolCalls, TurnAuditEvent};
use mini_chat_sdk::usage::turn_dedupe_key;
use mini_chat_sdk::{MiniChatAuditEvent, ModelTier, UsageEvent, UsageTokens};
use sea_orm::sea_query::Expr;
use sea_orm::{ActiveValue, ColumnTrait, Condition, EntityTrait, Order, QueryFilter};
use time::OffsetDateTime;
use toolkit_db::DbTx;
use toolkit_db::outbox::Wake;
use toolkit_db::secure::{SecureEntityExt, SecureInsertExt, SecureUpdateExt};
use uuid::Uuid;

use super::app::{App, fire, now, owner_scope, tenant_scope};
use super::error::DomainError;
use super::quota::{self, PeriodStarts, SettlementMethod, TurnReserve};
use super::sanitize::sanitize;
use crate::infra::db::entity::{chat_turns, messages, thread_summaries};
use crate::infra::llm::ProviderUsage;
use crate::infra::outbox::ThreadSummaryPayload;

/// Immutable facts of a running turn needed to finalize it.
#[derive(Debug, Clone)]
pub struct TurnRecord {
    pub tenant_id: Uuid,
    pub user_id: Uuid,
    pub chat_id: Uuid,
    pub turn_id: Uuid,
    pub request_id: Uuid,
    pub message_id: Uuid,
    pub selected_model: String,
    pub effective_model: String,
    pub policy_version: u64,
    pub reserve: TurnReserve,
    pub started_at: OffsetDateTime,
    pub downgrade_reason: Option<String>,
}

/// Terminal outcome of the provider stream.
#[derive(Debug, Clone)]
pub enum Outcome {
    Completed {
        text: String,
        usage: Option<ProviderUsage>,
        response_id: Option<String>,
        incomplete_reason: Option<String>,
    },
    Failed {
        code: String,
        message: String,
        usage: Option<ProviderUsage>,
        response_id: Option<String>,
    },
    Cancelled {
        text: String,
        response_id: Option<String>,
    },
}

/// Completed tool calls of the turn.
#[derive(Debug, Clone, Copy, Default)]
pub struct ToolCounts {
    pub web_search: i64,
    pub code_interpreter: i64,
    pub file_search: i64,
}

/// Result of a finalization attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Finalized {
    /// Committed with the given internal state and error code.
    Committed { state: &'static str, error_code: Option<String> },
    /// Another finalizer already moved the turn out of `running`.
    CasLost,
}

/// Billing outcome derivation (DESIGN §5.8, normative table).
#[must_use]
pub fn billing_for(state: &str, error_code: Option<&str>, usage_known: bool) -> (&'static str, SettlementMethod) {
    match (state, error_code) {
        ("completed", _) => ("completed", SettlementMethod::Actual),
        ("cancelled", _) | ("failed", Some("orphan_timeout")) => ("aborted", SettlementMethod::Estimated),
        ("failed", Some("context_length_exceeded" | "validation_error" | "input_too_long" | "turn_setup_failed")) => {
            ("failed", SettlementMethod::Released)
        }
        (
            "failed",
            Some(
                "provider_error" | "provider_timeout" | "rate_limited" | "web_search_calls_exceeded"
                | "code_interpreter_calls_exceeded" | "agentic_iterations_exceeded" | "unexpected_tool_use"
                | "message_persistence_failed",
            ),
        ) => (
            "failed",
            if usage_known {
                SettlementMethod::Actual
            } else {
                SettlementMethod::Estimated
            },
        ),
        _ => {
            tracing::error!(state, ?error_code, "unknown terminal error code; estimated settlement");
            ("failed", SettlementMethod::Estimated)
        }
    }
}

fn tokens(u: Option<ProviderUsage>) -> Option<UsageTokens> {
    u.map(|u| UsageTokens {
        input_tokens: u.input_tokens,
        output_tokens: u.output_tokens,
        cache_read_input_tokens: u.cache_read_input_tokens,
        cache_write_input_tokens: u.cache_write_input_tokens,
        reasoning_tokens: u.reasoning_tokens,
    })
}

fn to_u32(v: i64) -> u32 {
    u32::try_from(v.max(0)).unwrap_or(u32::MAX)
}

/// Data of one settlement + event emission.
pub struct SettleSpec<'a> {
    pub tenant_id: Uuid,
    pub user_id: Uuid,
    pub chat_id: Uuid,
    pub turn_id: Uuid,
    pub request_id: Uuid,
    pub selected_model: &'a str,
    pub effective_model: &'a str,
    pub policy_version: u64,
    pub tier: Option<ModelTier>,
    pub mults: (i64, i64),
    pub reserve: Option<TurnReserve>,
    pub periods: PeriodStarts,
    pub terminal_state: &'static str,
    pub error_code: Option<&'a str>,
    pub usage: Option<ProviderUsage>,
    pub counts: ToolCounts,
    pub overshoot_tolerance: f64,
    pub quota_decision: QuotaPolicyDecision,
    pub latency_ms: Option<u64>,
}

/// Shared steps of every finalization path: billing outcome derivation,
/// quota settlement and the usage + audit outbox events.
///
/// # Errors
/// Credit, database or outbox errors.
pub async fn settle_and_emit(app: &App, tx: &DbTx<'_>, s: &SettleSpec<'_>, at: OffsetDateTime) -> Result<Vec<Wake>, DomainError> {
    let usage_known = s.usage.is_some_and(|u| u.input_tokens > 0 || u.output_tokens > 0);
    let (billing, method) = billing_for(s.terminal_state, s.error_code, usage_known);
    let mut actual_credits = 0;
    let mut settlement_method = method;
    if let (Some(reserve), Some(tier)) = (s.reserve, s.tier) {
        let usage_pair = s.usage.map(|u| (u.input_tokens, u.output_tokens));
        let st = quota::compute_settlement(
            method,
            reserve,
            usage_pair,
            s.mults,
            s.overshoot_tolerance,
            (s.counts.web_search, s.counts.code_interpreter),
        )
        .map_err(|e| DomainError::internal(format!("settlement credits: {e}")))?;
        actual_credits = st.committed_credits_micro;
        quota::apply_settlement(
            tx,
            &owner_scope(s.tenant_id, s.user_id),
            s.tenant_id,
            s.user_id,
            tier,
            reserve.reserved_credits_micro,
            s.periods,
            &st,
            at,
        )
        .await?;
    } else {
        tracing::warn!(turn_id = %s.turn_id, "turn has no reserve; settlement skipped");
        settlement_method = SettlementMethod::Estimated;
    }
    let usage_out = if settlement_method == SettlementMethod::Actual {
        Some(tokens(s.usage).unwrap_or_default())
    } else {
        None
    };
    let ev = UsageEvent {
        tenant_id: s.tenant_id,
        user_id: Some(s.user_id),
        chat_id: s.chat_id,
        turn_id: Some(s.turn_id),
        request_id: s.request_id,
        effective_model: s.effective_model.to_owned(),
        selected_model: s.selected_model.to_owned(),
        terminal_state: s.terminal_state.to_owned(),
        billing_outcome: billing.to_owned(),
        usage: usage_out,
        actual_credits_micro: actual_credits,
        settlement_method: settlement_method.as_str().to_owned(),
        policy_version_applied: s.policy_version,
        web_search_calls: to_u32(s.counts.web_search),
        code_interpreter_calls: to_u32(s.counts.code_interpreter),
        file_search_calls: to_u32(s.counts.file_search),
        timestamp: at,
        requester_type: "user".into(),
        dedupe_key: turn_dedupe_key(s.tenant_id, s.turn_id, s.request_id),
        system_task_type: None,
    };
    let mut wakes = vec![app.outbox.usage(tx, &ev).await?];
    let audit = MiniChatAuditEvent::Turn(TurnAuditEvent {
        event_type: if s.terminal_state == "completed" { "turn_completed" } else { "turn_failed" }.into(),
        tenant_id: s.tenant_id,
        requester_type: "user".into(),
        actor_user_id: Some(s.user_id),
        chat_id: s.chat_id,
        turn_id: s.turn_id,
        request_id: s.request_id,
        selected_model: s.selected_model.to_owned(),
        effective_model: s.effective_model.to_owned(),
        terminal_state: s.terminal_state.to_owned(),
        error_code: s.error_code.map(str::to_owned),
        usage: tokens(s.usage),
        latency_ms: s.latency_ms,
        tool_calls: ToolCalls {
            web_search_calls: to_u32(s.counts.web_search),
            file_search_calls: to_u32(s.counts.file_search),
        },
        policy_decisions: PolicyDecisions { quota: s.quota_decision.clone() },
        prompt: String::new(),
        response: String::new(),
        attachments: Vec::new(),
        license: String::new(),
        quota_scope: String::new(),
        trace_id: None,
        timestamp: at,
    });
    wakes.push(app.outbox.audit(tx, &audit).await?);
    Ok(wakes)
}

/// Enqueues thread-summary work for a completed turn when needed.
///
/// # Errors
/// Database or outbox errors.
pub async fn enqueue_summary_if_needed(app: &App, tx: &DbTx<'_>, rec: &TurnRecord) -> Result<(Option<Wake>, bool), DomainError> {
    let scope = tenant_scope(rec.tenant_id);
    let target = messages::Entity::find()
        .secure()
        .scope_with(&scope)
        .filter(
            Condition::all()
                .add(messages::Column::ChatId.eq(rec.chat_id))
                .add(messages::Column::DeletedAt.is_null())
                .add(
                    Condition::any()
                        .add(messages::Column::RequestId.ne(rec.request_id))
                        .add(messages::Column::RequestId.is_null()),
                ),
        )
        .order_by(messages::Column::CreatedAt, Order::Desc)
        .order_by(messages::Column::Id, Order::Desc)
        .limit(1)
        .one(tx)
        .await?;
    let Some(target) = target else {
        return Ok((None, false));
    };
    let summary = thread_summaries::Entity::find()
        .secure()
        .scope_with(&scope)
        .filter(Condition::all().add(thread_summaries::Column::ChatId.eq(rec.chat_id)))
        .one(tx)
        .await?;
    if let Some(s) = &summary
        && s.summarized_up_to_message_id == target.id
    {
        return Ok((None, false));
    }
    let payload = ThreadSummaryPayload {
        tenant_id: rec.tenant_id,
        chat_id: rec.chat_id,
        system_request_id: Uuid::new_v4(),
        base_frontier_created_at: summary.as_ref().map(|s| s.summarized_up_to_created_at),
        base_frontier_message_id: summary.as_ref().map(|s| s.summarized_up_to_message_id),
        frozen_target_created_at: target.created_at,
        frozen_target_message_id: target.id,
        system_task_type: "thread_summary_update".into(),
    };
    Ok((Some(app.outbox.thread_summary(tx, &payload).await?), true))
}

fn cas_filter(turn_id: Uuid) -> Condition {
    Condition::all()
        .add(chat_turns::Column::Id.eq(turn_id))
        .add(chat_turns::Column::State.eq("running"))
}

impl App {
    /// Finalizes a turn after the provider stream ended.
    ///
    /// # Errors
    /// The finalization transaction failed; the turn stays `running`.
    #[allow(clippy::too_many_lines)]
    pub async fn finalize_turn(
        self: &Arc<Self>,
        rec: &TurnRecord,
        outcome: Outcome,
        counts: ToolCounts,
        summary_trigger: bool,
    ) -> Result<Finalized, DomainError> {
        let snapshot = self.policy.snapshot(rec.user_id, rec.policy_version).await?;
        let entry = snapshot
            .find(&rec.effective_model)
            .ok_or_else(|| DomainError::internal("effective model missing from the applied policy snapshot"))?;
        let tier = entry.tier;
        let mults = quota::multipliers(entry);
        let periods = PeriodStarts::at(rec.started_at);
        let latency = u64::try_from((now() - rec.started_at).whole_milliseconds()).ok();
        let decision = QuotaPolicyDecision {
            decision: if rec.effective_model == rec.selected_model && rec.downgrade_reason.is_none() {
                "allow"
            } else {
                "downgrade"
            }
            .into(),
            downgrade_from: (rec.effective_model != rec.selected_model || rec.downgrade_reason.is_some())
                .then(|| rec.selected_model.clone()),
            downgrade_reason: rec.downgrade_reason.clone(),
        };
        let attempt = |state: &'static str,
                       error_code: Option<String>,
                       detail: Option<String>,
                       message_text: Option<String>,
                       usage: Option<ProviderUsage>,
                       response_id: Option<String>,
                       with_summary: bool| {
            let app = Arc::clone(self);
            let rec = rec.clone();
            let decision = decision.clone();
            async move {
                let db = app.db.clone();
                db.transaction(move |tx| {
                        Box::pin(async move {
                            let at = now();
                            let scope = tenant_scope(rec.tenant_id);
                            let mut upd = chat_turns::Entity::update_many()
                                .col_expr(chat_turns::Column::State, Expr::value(state))
                                .col_expr(chat_turns::Column::CompletedAt, Expr::value(Some(at)))
                                .col_expr(chat_turns::Column::UpdatedAt, Expr::value(at))
                                .col_expr(chat_turns::Column::ErrorCode, Expr::value(error_code.clone()))
                                .col_expr(chat_turns::Column::ErrorDetail, Expr::value(detail.clone()))
                                .col_expr(chat_turns::Column::ProviderResponseId, Expr::value(response_id.clone()))
                                .col_expr(
                                    chat_turns::Column::WebSearchCompletedCount,
                                    Expr::value(i32::try_from(counts.web_search).unwrap_or(i32::MAX)),
                                )
                                .col_expr(
                                    chat_turns::Column::CodeInterpreterCompletedCount,
                                    Expr::value(i32::try_from(counts.code_interpreter).unwrap_or(i32::MAX)),
                                )
                                .col_expr(
                                    chat_turns::Column::FileSearchCompletedCount,
                                    Expr::value(i32::try_from(counts.file_search).unwrap_or(i32::MAX)),
                                );
                            if message_text.is_some() {
                                upd = upd.col_expr(chat_turns::Column::AssistantMessageId, Expr::value(Some(rec.message_id)));
                            }
                            let rows = upd
                                .filter(cas_filter(rec.turn_id))
                                .secure()
                                .scope_with(&scope)
                                .exec(tx)
                                .await?
                                .rows_affected;
                            if rows == 0 {
                                return Ok(None);
                            }
                            if let Some(text) = message_text {
                                let u = usage.unwrap_or_default();
                                let am = messages::ActiveModel {
                                    id: ActiveValue::Set(rec.message_id),
                                    tenant_id: ActiveValue::Set(rec.tenant_id),
                                    chat_id: ActiveValue::Set(rec.chat_id),
                                    request_id: ActiveValue::Set(Some(rec.request_id)),
                                    role: ActiveValue::Set("assistant".into()),
                                    content: ActiveValue::Set(text),
                                    content_type: ActiveValue::Set("text".into()),
                                    token_estimate: ActiveValue::Set(0),
                                    provider_response_id: ActiveValue::Set(response_id.clone()),
                                    request_kind: ActiveValue::Set("chat".into()),
                                    features_used: ActiveValue::Set(serde_json::json!([])),
                                    input_tokens: ActiveValue::Set(u.input_tokens.max(0)),
                                    output_tokens: ActiveValue::Set(u.output_tokens.max(0)),
                                    cache_read_input_tokens: ActiveValue::Set(u.cache_read_input_tokens.max(0)),
                                    cache_write_input_tokens: ActiveValue::Set(u.cache_write_input_tokens.max(0)),
                                    reasoning_tokens: ActiveValue::Set(u.reasoning_tokens.max(0)),
                                    model: ActiveValue::Set(Some(rec.effective_model.clone())),
                                    is_compressed: ActiveValue::Set(false),
                                    created_at: ActiveValue::Set(at),
                                    deleted_at: ActiveValue::Set(None),
                                };
                                messages::Entity::insert(am).secure().scope_unchecked(&scope)?.exec(tx).await?;
                            }
                            let spec = SettleSpec {
                                tenant_id: rec.tenant_id,
                                user_id: rec.user_id,
                                chat_id: rec.chat_id,
                                turn_id: rec.turn_id,
                                request_id: rec.request_id,
                                selected_model: &rec.selected_model,
                                effective_model: &rec.effective_model,
                                policy_version: rec.policy_version,
                                tier: Some(tier),
                                mults,
                                reserve: Some(rec.reserve),
                                periods,
                                terminal_state: state,
                                error_code: error_code.as_deref(),
                                usage: if state == "cancelled" { None } else { usage },
                                counts,
                                overshoot_tolerance: app.cfg.quota.overshoot_tolerance_factor,
                                quota_decision: decision,
                                latency_ms: latency,
                            };
                            let mut wakes = settle_and_emit(&app, tx, &spec, at).await?;
                            let mut scheduled = None;
                            if with_summary {
                                let (wake, s) = enqueue_summary_if_needed(&app, tx, &rec).await?;
                                if let Some(w) = wake {
                                    wakes.push(w);
                                }
                                scheduled = Some(s);
                            }
                            Ok(Some((wakes, scheduled)))
                        })
                    })
                    .await
            }
        };

        match outcome {
            Outcome::Completed { text, usage, response_id, incomplete_reason } => {
                if let Some(r) = &incomplete_reason {
                    tracing::warn!(turn_id = %rec.turn_id, reason = %r, "stream incomplete");
                }
                match attempt("completed", None, None, Some(text), usage, response_id.clone(), summary_trigger).await {
                    Ok(Some((wakes, scheduled))) => {
                        fire(wakes);
                        if let Some(s) = scheduled {
                            tracing::debug!(turn_id = %rec.turn_id, scheduled = s, "thread summary trigger evaluated");
                        }
                        Ok(Finalized::Committed { state: "completed", error_code: None })
                    }
                    Ok(None) => Ok(Finalized::CasLost),
                    Err(e) => {
                        tracing::warn!(turn_id = %rec.turn_id, error = %e, "completed finalization failed; marking failed");
                        match attempt(
                            "failed",
                            Some("message_persistence_failed".into()),
                            Some("assistant message could not be persisted".into()),
                            None,
                            usage,
                            response_id,
                            false,
                        )
                        .await
                        {
                            Ok(Some((wakes, _))) => {
                                fire(wakes);
                                Ok(Finalized::Committed {
                                    state: "failed",
                                    error_code: Some("message_persistence_failed".into()),
                                })
                            }
                            Ok(None) => Ok(Finalized::CasLost),
                            Err(e2) => Err(e2),
                        }
                    }
                }
            }
            Outcome::Failed { code, message, usage, response_id } => {
                let detail = Some(sanitize(&message));
                match attempt("failed", Some(code.clone()), detail, None, usage, response_id, false).await? {
                    Some((wakes, _)) => {
                        fire(wakes);
                        Ok(Finalized::Committed { state: "failed", error_code: Some(code) })
                    }
                    None => Ok(Finalized::CasLost),
                }
            }
            Outcome::Cancelled { text, response_id } => {
                let msg = (!text.is_empty()).then_some(text);
                let first = attempt("cancelled", None, None, msg.clone(), None, response_id.clone(), false).await;
                let res = match first {
                    Err(e) if msg.is_some() => {
                        tracing::warn!(turn_id = %rec.turn_id, error = %e, "partial message not persisted; finalizing without it");
                        attempt("cancelled", None, None, None, None, response_id, false).await?
                    }
                    other => other?,
                };
                match res {
                    Some((wakes, _)) => {
                        fire(wakes);
                        Ok(Finalized::Committed { state: "cancelled", error_code: None })
                    }
                    None => Ok(Finalized::CasLost),
                }
            }
        }
    }

    /// Marks a retry/edit turn whose setup failed before the reserve as
    /// `failed` (no settlement, no outbox event).
    ///
    /// # Errors
    /// Database errors.
    pub async fn fail_unstarted_turn(&self, tenant_id: Uuid, turn_id: Uuid, code: &str) -> Result<(), DomainError> {
        let at = now();
        let conn = self.db.conn()?;
        chat_turns::Entity::update_many()
            .col_expr(chat_turns::Column::State, Expr::value("failed"))
            .col_expr(chat_turns::Column::ErrorCode, Expr::value(Some(code.to_owned())))
            .col_expr(chat_turns::Column::CompletedAt, Expr::value(Some(at)))
            .col_expr(chat_turns::Column::UpdatedAt, Expr::value(at))
            .filter(cas_filter(turn_id))
            .secure()
            .scope_with(&tenant_scope(tenant_id))
            .exec(&conn)
            .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn billing_table() {
        assert_eq!(billing_for("completed", None, false), ("completed", SettlementMethod::Actual));
        assert_eq!(billing_for("cancelled", None, true), ("aborted", SettlementMethod::Estimated));
        assert_eq!(billing_for("failed", Some("orphan_timeout"), false), ("aborted", SettlementMethod::Estimated));
        assert_eq!(billing_for("failed", Some("provider_error"), true), ("failed", SettlementMethod::Actual));
        assert_eq!(billing_for("failed", Some("provider_error"), false), ("failed", SettlementMethod::Estimated));
        assert_eq!(
            billing_for("failed", Some("web_search_calls_exceeded"), false),
            ("failed", SettlementMethod::Estimated)
        );
        assert_eq!(billing_for("failed", Some("turn_setup_failed"), false), ("failed", SettlementMethod::Released));
        assert_eq!(billing_for("failed", Some("mystery"), true), ("failed", SettlementMethod::Estimated));
    }
}
