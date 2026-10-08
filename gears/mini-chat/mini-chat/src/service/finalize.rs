//! Turn finalization: CAS on `chat_turns.state`, assistant message
//! persistence, quota settlement and outbox emission in one transaction
//! (DESIGN §5.7).

use std::sync::Arc;

use mini_chat_sdk::{
    MiniChatAuditEvent, PolicyDecisions, QuotaPolicyDecision, ToolCalls, TurnAuditEvent,
    UsageEvent, UsageTokens,
};
use sea_orm::sea_query::Expr;
use sea_orm::{ColumnTrait, Condition, EntityTrait, Order, Set};
use time::OffsetDateTime;
use toolkit_db::outbox::Wake;
use toolkit_db::secure::{AccessScope, DBRunner, SecureEntityExt, SecureUpdateExt, secure_insert};
use uuid::Uuid;

use super::AppState;
use super::outbox::ThreadSummaryTask;
use super::quota::{ToolCallCounts, load_usage, settle_in_tx};
use super::stream::{QuotaWarningOut, TurnPlan};
use crate::domain::context::input_limit;
use crate::domain::error::{DomainError, DomainResult};
use crate::domain::quota::{Settlement, Terminal, quota_status, settle};
use crate::infra::db::entity::{chat_turn, message};
use crate::infra::repo::{self, now_utc};

#[derive(Debug, Clone)]
pub enum Outcome {
    Completed {
        text: String,
        usage: Option<UsageTokens>,
        response_id: Option<String>,
        incomplete_reason: Option<String>,
    },
    Failed {
        code: String,
        message: String,
        usage: Option<UsageTokens>,
    },
    Cancelled {
        text: String,
    },
}

/// Completed tool calls of the turn.
#[derive(Debug, Clone, Copy, Default)]
pub struct TurnCounts {
    pub web_search: u32,
    pub code_interpreter: u32,
    pub file_search: u32,
}

#[derive(Debug)]
pub enum FinalizeOutcome {
    Committed {
        state: &'static str,
        quota_warnings: Vec<QuotaWarningOut>,
    },
    CasLost,
    Error(DomainError),
}

/// Marker for a failed assistant message insert.
const MSG_PERSIST_MARKER: &str = "__assistant_message_persist_failed__";

/// Backend contention (busy / serialization) that the transaction retries.
fn is_contention(e: &DomainError) -> bool {
    use sea_orm::DbBackend;
    use toolkit_db::contention::is_retryable_contention;
    e.db_err().is_some_and(|d| {
        is_retryable_contention(DbBackend::Sqlite, d)
            || is_retryable_contention(DbBackend::Postgres, d)
    })
}

#[must_use]
pub fn dedupe_key(tenant: Uuid, turn: Uuid, request: Uuid) -> String {
    format!(
        "{}/{}/{}",
        tenant.as_simple(),
        turn.as_simple(),
        request.as_simple()
    )
}

struct TxInput {
    plan: TurnPlan,
    terminal: Terminal,
    usage: Option<UsageTokens>,
    text: Option<String>,
    response_id: Option<String>,
    error_code: Option<String>,
    error_detail: Option<String>,
    counts: TurnCounts,
    in_mult: i64,
    out_mult: i64,
    summary_enabled: bool,
    compress_pct: u32,
}

fn audit_for(
    plan: &TurnPlan,
    terminal: &Terminal,
    usage: Option<UsageTokens>,
    error_code: Option<String>,
    counts: TurnCounts,
) -> TurnAuditEvent {
    TurnAuditEvent {
        event_type: if matches!(terminal, Terminal::Completed) {
            "turn_completed".into()
        } else {
            "turn_failed".into()
        },
        tenant_id: plan.tenant_id,
        user_id: plan.user_id,
        chat_id: plan.chat_id,
        turn_id: plan.turn_id,
        request_id: plan.request_id,
        selected_model: plan.selected_model.clone(),
        effective_model: plan.effective.id.clone(),
        terminal_state: terminal.state().to_owned(),
        error_code,
        usage,
        latency_ms: u64::try_from(plan.started.elapsed().as_millis()).ok(),
        tool_calls: ToolCalls {
            web_search_calls: counts.web_search,
            file_search_calls: counts.file_search,
        },
        policy_decisions: PolicyDecisions {
            quota: QuotaPolicyDecision {
                decision: if plan.downgrade { "downgrade" } else { "allow" }.into(),
                downgrade_from: plan.downgrade.then(|| plan.selected_model.clone()),
                downgrade_reason: plan.downgrade_reason.clone(),
            },
            license: String::new(),
        },
        prompt: String::new(),
        response: String::new(),
        attachments: vec![],
        quota_scope: String::new(),
        trace_id: None,
        timestamp: OffsetDateTime::now_utc(),
    }
}

#[allow(clippy::too_many_lines)]
async fn run_tx(
    state: Arc<AppState>,
    inp: Arc<TxInput>,
    with_message: bool,
) -> DomainResult<Option<(Wake, Settlement, Vec<QuotaWarningOut>)>> {
    let outbox = state.outbox.get().await?;
    let st = Arc::clone(&state);
    state
        .write_tx(move |tx| {
            let inp = Arc::clone(&inp);
            let outbox = Arc::clone(&outbox);
            let state = Arc::clone(&st);
            Box::pin(async move {
                let plan = &inp.plan;
                let scope = AccessScope::for_tenant(plan.tenant_id);
                let now = now_utc();
                let msg_id = if with_message && let Some(text) = &inp.text {
                    let created = repo::next_message_time(tx, &scope, plan.chat_id).await?;
                    let u = inp.usage.unwrap_or_default();
                    let am = message::ActiveModel {
                        id: Set(plan.assistant_message_id),
                        tenant_id: Set(plan.tenant_id),
                        chat_id: Set(plan.chat_id),
                        request_id: Set(Some(plan.request_id)),
                        role: Set("assistant".to_owned()),
                        content: Set(text.clone()),
                        content_type: Set("text".to_owned()),
                        token_estimate: Set(0),
                        provider_response_id: Set(inp.response_id.clone()),
                        request_kind: Set("chat".to_owned()),
                        features_used: Set(serde_json::json!([])),
                        input_tokens: Set(u.input_tokens.max(0)),
                        output_tokens: Set(u.output_tokens.max(0)),
                        cache_read_input_tokens: Set(u.cache_read_input_tokens.max(0)),
                        cache_write_input_tokens: Set(u.cache_write_input_tokens.max(0)),
                        reasoning_tokens: Set(u.reasoning_tokens.max(0)),
                        model: Set(Some(plan.effective.id.clone())),
                        is_compressed: Set(false),
                        created_at: Set(created),
                        deleted_at: Set(None),
                    };
                    if let Err(e) = secure_insert::<message::Entity>(am, &scope, tx).await {
                        let e = DomainError::from(e);
                        if is_contention(&e) {
                            // Let the transaction retry.
                            return Err(e);
                        }
                        tracing::warn!(error = %e, "assistant message persistence failed");
                        return Err(DomainError::internal(MSG_PERSIST_MARKER));
                    }
                    Some(plan.assistant_message_id)
                } else {
                    None
                };
                // CAS: first terminal wins.
                let mut upd = chat_turn::Entity::update_many()
                    .secure()
                    .scope_with(&scope)
                    .col_expr(chat_turn::Column::State, Expr::value(inp.terminal.state()))
                    .col_expr(chat_turn::Column::CompletedAt, Expr::value(Some(now)))
                    .col_expr(chat_turn::Column::UpdatedAt, Expr::value(now))
                    .col_expr(chat_turn::Column::AssistantMessageId, Expr::value(msg_id))
                    .col_expr(
                        chat_turn::Column::ErrorCode,
                        Expr::value(inp.error_code.clone()),
                    )
                    .col_expr(
                        chat_turn::Column::ErrorDetail,
                        Expr::value(inp.error_detail.clone()),
                    );
                if inp.response_id.is_some() {
                    upd = upd.col_expr(
                        chat_turn::Column::ProviderResponseId,
                        Expr::value(inp.response_id.clone()),
                    );
                }
                let res = upd
                    .filter(
                        Condition::all()
                            .add(chat_turn::Column::Id.eq(plan.turn_id))
                            .add(chat_turn::Column::State.eq("running")),
                    )
                    .exec(tx)
                    .await?;
                if res.rows_affected == 0 {
                    // Another finalizer won; roll back (the message insert too).
                    return Err(DomainError::aborted("cas_lost", "turn already finalized"));
                }
                let settlement = settle(
                    &inp.terminal,
                    inp.usage.as_ref(),
                    &plan.reserve,
                    inp.in_mult,
                    inp.out_mult,
                    state.cfg.quota.overshoot_tolerance_factor,
                )
                .map_err(|e| {
                    DomainError::internal(format!("settlement credit computation: {e}"))
                })?;
                settle_in_tx(
                    tx,
                    &scope,
                    plan.tenant_id,
                    plan.user_id,
                    &plan.periods,
                    plan.premium,
                    plan.reserve.reserved_credits_micro,
                    &settlement,
                    ToolCallCounts {
                        web_search: i64::from(inp.counts.web_search),
                        code_interpreter: i64::from(inp.counts.code_interpreter),
                    },
                )
                .await?;
                let usage_out = if settlement.settlement_method == "actual" {
                    inp.usage
                } else {
                    None
                };
                let ev = UsageEvent {
                    tenant_id: plan.tenant_id,
                    user_id: Some(plan.user_id),
                    chat_id: plan.chat_id,
                    turn_id: Some(plan.turn_id),
                    request_id: plan.request_id,
                    effective_model: plan.effective.id.clone(),
                    selected_model: plan.selected_model.clone(),
                    terminal_state: inp.terminal.state().to_owned(),
                    billing_outcome: settlement.billing_outcome.to_owned(),
                    usage: usage_out,
                    actual_credits_micro: settlement.committed_credits_micro,
                    settlement_method: settlement.settlement_method.to_owned(),
                    policy_version_applied: plan.policy_version,
                    web_search_calls: inp.counts.web_search,
                    code_interpreter_calls: inp.counts.code_interpreter,
                    file_search_calls: inp.counts.file_search,
                    timestamp: OffsetDateTime::now_utc(),
                    requester_type: "user".into(),
                    dedupe_key: dedupe_key(plan.tenant_id, plan.turn_id, plan.request_id),
                    system_task_type: None,
                };
                let mut wake = state.enqueue_usage(&outbox, tx, &ev).await?;
                let audit = MiniChatAuditEvent::Turn(audit_for(
                    plan,
                    &inp.terminal,
                    inp.usage,
                    inp.error_code.clone(),
                    inp.counts,
                ));
                wake += state
                    .enqueue_audit(&outbox, tx, plan.tenant_id, &audit)
                    .await?;
                let mut warnings = Vec::new();
                if matches!(inp.terminal, Terminal::Completed) {
                    if inp.summary_enabled
                        && let Some(w) =
                            schedule_summary(&state, &outbox, tx, &scope, plan, inp.compress_pct)
                                .await?
                    {
                        wake += w;
                    }
                    let usage =
                        load_usage(tx, &scope, plan.tenant_id, plan.user_id, &plan.periods).await?;
                    let now_ts = OffsetDateTime::now_utc();
                    for t in quota_status(
                        &plan.limits,
                        &usage,
                        state.cfg.quota.warning_threshold_pct,
                        now_ts,
                    ) {
                        for p in t.periods {
                            warnings.push(QuotaWarningOut {
                                tier: t.tier,
                                period: p.period.as_str(),
                                remaining_percentage: p.remaining_percentage,
                                warning: p.warning,
                                exhausted: p.exhausted,
                                next_reset: (p.warning || p.exhausted).then_some(p.next_reset),
                            });
                        }
                    }
                }
                Ok(Some((wake, settlement, warnings)))
            })
        })
        .await
}

/// Thread summary trigger, evaluated in the finalization transaction of a
/// completed turn.
#[allow(
    clippy::integer_division,
    reason = "intentional integer arithmetic (explicit rounding)"
)]
async fn schedule_summary(
    state: &AppState,
    outbox: &toolkit_db::outbox::Outbox,
    tx: &toolkit_db::DbTx<'_>,
    scope: &AccessScope,
    plan: &TurnPlan,
    pct: u32,
) -> DomainResult<Option<Wake>> {
    let Some(budget) = input_limit(
        plan.effective.context_window,
        plan.effective.max_input_tokens,
        plan.reserve.max_output_tokens_applied,
    ) else {
        return Ok(None);
    };
    let threshold = budget.saturating_mul(i64::from(pct)) / 100;
    let summary = repo::find_summary(tx, scope, plan.chat_id).await?;
    let trigger =
        plan.messages_truncated || (summary.is_none() && plan.assembled_tokens >= threshold);
    if !trigger {
        return Ok(None);
    }
    let target = message::Entity::find()
        .secure()
        .scope_with(scope)
        .filter(
            Condition::all()
                .add(message::Column::ChatId.eq(plan.chat_id))
                .add(message::Column::DeletedAt.is_null())
                .add(
                    Condition::any()
                        .add(message::Column::RequestId.ne(plan.request_id))
                        .add(message::Column::RequestId.is_null()),
                ),
        )
        .order_by(message::Column::CreatedAt, Order::Desc)
        .order_by(message::Column::Id, Order::Desc)
        .limit(1)
        .one(tx)
        .await?;
    let Some(target) = target else {
        return Ok(None);
    };
    if let Some(s) = &summary
        && (target.created_at, target.id)
            <= (s.summarized_up_to_created_at, s.summarized_up_to_message_id)
    {
        return Ok(None);
    }
    let task = ThreadSummaryTask {
        tenant_id: plan.tenant_id,
        chat_id: plan.chat_id,
        system_request_id: Uuid::new_v4(),
        base_frontier_created_at: summary.as_ref().map(|s| s.summarized_up_to_created_at),
        base_frontier_message_id: summary.as_ref().map(|s| s.summarized_up_to_message_id),
        frozen_target_created_at: target.created_at,
        frozen_target_message_id: target.id,
        system_task_type: "thread_summary_update".into(),
    };
    Ok(Some(state.enqueue_thread_summary(outbox, tx, &task).await?))
}

impl AppState {
    /// Finalize a turn that holds a reserve.
    #[allow(
        clippy::cognitive_complexity,
        reason = "sequential orchestration steps; splitting would obscure the flow"
    )]
    pub async fn finalize_turn(
        self: &Arc<Self>,
        plan: &TurnPlan,
        outcome: Outcome,
        counts: TurnCounts,
    ) -> FinalizeOutcome {
        let (in_mult, out_mult) = match self
            .policy
            .snapshot(plan.user_id, plan.policy_version)
            .await
        {
            Ok(s) => s.find(&plan.effective.id).map_or(
                (
                    plan.effective.input_tokens_credit_multiplier_micro,
                    plan.effective.output_tokens_credit_multiplier_micro,
                ),
                |m| {
                    (
                        m.input_tokens_credit_multiplier_micro,
                        m.output_tokens_credit_multiplier_micro,
                    )
                },
            ),
            Err(_) => (
                plan.effective.input_tokens_credit_multiplier_micro,
                plan.effective.output_tokens_credit_multiplier_micro,
            ),
        };
        let (terminal, usage, text, response_id, error_code, error_detail) = match outcome {
            Outcome::Completed {
                text,
                usage,
                response_id,
                incomplete_reason,
            } => {
                if let Some(r) = incomplete_reason {
                    tracing::warn!(reason = %r, request_id = %plan.request_id, "stream incomplete");
                }
                (
                    Terminal::Completed,
                    usage,
                    Some(text),
                    response_id,
                    None,
                    None,
                )
            }
            Outcome::Failed {
                code,
                message,
                usage,
            } => (
                Terminal::Failed {
                    error_code: code.clone(),
                },
                usage,
                None,
                None,
                Some(code),
                Some(message),
            ),
            Outcome::Cancelled { text } => (
                Terminal::Cancelled,
                None,
                (!text.is_empty()).then_some(text),
                None,
                None,
                None,
            ),
        };
        let summary_enabled = self.cfg.thread_summary_worker.enabled;
        let compress_pct = self.cfg.thread_summary_worker.compression_threshold_pct;
        let mk = |terminal: Terminal,
                  text: Option<String>,
                  error_code: Option<String>,
                  error_detail: Option<String>| {
            Arc::new(TxInput {
                plan: plan.clone(),
                terminal,
                usage,
                text,
                response_id: response_id.clone(),
                error_code,
                error_detail,
                counts,
                in_mult,
                out_mult,
                summary_enabled,
                compress_pct,
            })
        };
        let first = mk(
            terminal.clone(),
            text.clone(),
            error_code.clone(),
            error_detail.clone(),
        );
        match run_tx(Arc::clone(self), first, true).await {
            Ok(Some((wake, _s, warnings))) => {
                wake.fire();
                FinalizeOutcome::Committed {
                    state: terminal.state(),
                    quota_warnings: warnings,
                }
            }
            Ok(None) => FinalizeOutcome::CasLost,
            Err(e) if e.is_aborted_reason("cas_lost") => FinalizeOutcome::CasLost,
            Err(DomainError::Internal(m)) if m == MSG_PERSIST_MARKER => {
                // Completed: downgrade to failed; cancelled: finalize without text.
                let retry = match terminal {
                    Terminal::Completed => mk(
                        Terminal::Failed {
                            error_code: "message_persistence_failed".into(),
                        },
                        None,
                        Some("message_persistence_failed".into()),
                        Some("assistant message could not be persisted".into()),
                    ),
                    other => mk(other, None, error_code, error_detail),
                };
                let st = retry.terminal.state();
                match run_tx(Arc::clone(self), retry, false).await {
                    Ok(Some((wake, _, _))) => {
                        wake.fire();
                        FinalizeOutcome::Committed {
                            state: st,
                            quota_warnings: vec![],
                        }
                    }
                    Ok(None) => FinalizeOutcome::CasLost,
                    Err(e) if e.is_aborted_reason("cas_lost") => FinalizeOutcome::CasLost,
                    Err(e) => FinalizeOutcome::Error(e),
                }
            }
            Err(e) => {
                tracing::error!(error = %e, request_id = %plan.request_id, "turn finalization failed");
                FinalizeOutcome::Error(e)
            }
        }
    }

    /// Mark an unstarted retry/edit turn (no reserve) as failed.
    pub async fn fail_unstarted_turn(&self, turn_id: Uuid, tenant: Uuid, code: &str) {
        let now = now_utc();
        let Ok(conn) = self.conn() else {
            return;
        };
        let res = chat_turn::Entity::update_many()
            .secure()
            .scope_with(&AccessScope::for_tenant(tenant))
            .col_expr(chat_turn::Column::State, Expr::value("failed"))
            .col_expr(
                chat_turn::Column::ErrorCode,
                Expr::value(Some(code.to_owned())),
            )
            .col_expr(chat_turn::Column::CompletedAt, Expr::value(Some(now)))
            .col_expr(chat_turn::Column::UpdatedAt, Expr::value(now))
            .filter(
                Condition::all()
                    .add(chat_turn::Column::Id.eq(turn_id))
                    .add(chat_turn::Column::State.eq("running")),
            )
            .exec(&conn)
            .await;
        if let Err(e) = res {
            tracing::error!(error = %e, "failed to mark unstarted turn as failed");
        }
    }
}

/// Increment a completed-tool counter on the turn row (best effort).
pub async fn bump_counter(r: &impl DBRunner, tenant: Uuid, turn_id: Uuid, col: chat_turn::Column) {
    let res = chat_turn::Entity::update_many()
        .secure()
        .scope_with(&AccessScope::for_tenant(tenant))
        .col_expr(col, sea_orm::ExprTrait::add(Expr::col(col), 1))
        .filter(
            Condition::all()
                .add(chat_turn::Column::Id.eq(turn_id))
                .add(chat_turn::Column::State.eq("running")),
        )
        .exec(r)
        .await;
    if let Err(e) = res {
        tracing::warn!(error = %e, "failed to bump turn counter");
    }
}

/// Refresh `last_progress_at` of a running turn (best effort).
pub async fn touch_progress(r: &impl DBRunner, tenant: Uuid, turn_id: Uuid) {
    let now = now_utc();
    let res = chat_turn::Entity::update_many()
        .secure()
        .scope_with(&AccessScope::for_tenant(tenant))
        .col_expr(chat_turn::Column::LastProgressAt, Expr::value(Some(now)))
        .filter(
            Condition::all()
                .add(chat_turn::Column::Id.eq(turn_id))
                .add(chat_turn::Column::State.eq("running")),
        )
        .exec(r)
        .await;
    if let Err(e) = res {
        tracing::warn!(error = %e, "failed to refresh turn progress");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dedupe_key_format() {
        let k = dedupe_key(Uuid::from_u128(1), Uuid::from_u128(2), Uuid::from_u128(3));
        let parts: Vec<&str> = k.split('/').collect();
        assert_eq!(parts.len(), 3);
        assert!(parts.iter().all(|p| p.len() == 32 && !p.contains('-')));
    }
}
