//! Turn finalization contract (DESIGN §5.7–5.9): CAS on `chat_turns.state`, assistant message,
//! quota settlement and outbox emission in one transaction; terminal SSE only after commit.

use crate::infra::db::WriteTransaction;
use std::collections::HashMap;
use std::hash::BuildHasher;
use std::sync::Arc;

use mini_chat_sdk::{
    AuditEvent, ModelTier, PolicyDecisions, QuotaDecisionAudit, ToolCalls, TurnAuditEvent,
    UsageEvent, UsageTokens,
};
use sea_orm::sea_query::Expr;
use sea_orm::{
    ActiveValue, ColumnTrait, Condition, EntityTrait, Order, QueryFilter, QueryOrder, QuerySelect,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use time::{Date, OffsetDateTime};
use toolkit_db::outbox::Wake;
use toolkit_db::secure::{DBRunner, SecureEntityExt, SecureInsertExt, SecureUpdateExt};
use uuid::Uuid;

use super::chats::tenant_scope;
use super::quota::{QuotaDecision, ReserveSpec, Settlement, SettlementMethod, settle_in_tx};
use super::stream::{SseEvent, ToolCounters, load_summary};
use super::{Core, now};
use crate::domain::error::DomainError;
use crate::domain::quota_math::{Period, credits_micro_checked, mult};
use crate::infra::db::entities::{message, turn};
use crate::infra::llm::responses::{Completion, RawCitation};
use crate::infra::outbox::QueueKind;

/// Immutable facts of a running turn needed to finalize it.
#[derive(Debug, Clone)]
pub struct TurnMeta {
    pub turn_id: Uuid,
    pub chat_id: Uuid,
    pub tenant_id: Uuid,
    pub user_id: Uuid,
    pub request_id: Uuid,
    pub assistant_message_id: Uuid,
    pub user_message_id: Uuid,
    pub user_message_created_at: OffsetDateTime,
    pub selected_model: String,
    pub effective_model: String,
    pub effective_tier: ModelTier,
    pub decision: QuotaDecision,
    pub downgrade_reason: Option<&'static str>,
    pub reserve: Option<ReserveSpec>,
    pub reserve_tokens: i64,
    pub max_output_tokens_applied: u32,
    pub floor_applied: u32,
    pub policy_version: u64,
    pub assembled_tokens: i64,
    pub effective_budget: i64,
    pub messages_truncated: bool,
    pub summary_exists: bool,
    pub provider_id: String,
}

/// How the provider stream ended.
#[derive(Debug, Clone)]
pub enum Terminal {
    Completed(Completion),
    Failed {
        code: String,
        message: String,
        usage: Option<UsageTokens>,
    },
    Cancelled,
}

pub struct FinalizeInput {
    pub meta: TurnMeta,
    pub terminal: Terminal,
    pub text: String,
    pub counts: ToolCounters,
    pub citation_map: HashMap<String, (Uuid, String)>,
    pub latency_ms: u64,
    pub ttft_ms: Option<u64>,
}

/// Thread-summary outbox payload (DESIGN §3.6 "Durable scheduling").
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThreadSummaryPayload {
    pub tenant_id: Uuid,
    pub chat_id: Uuid,
    pub system_request_id: Uuid,
    #[serde(default, with = "time::serde::rfc3339::option")]
    pub base_frontier_created_at: Option<OffsetDateTime>,
    pub base_frontier_message_id: Option<Uuid>,
    #[serde(with = "time::serde::rfc3339")]
    pub frozen_target_created_at: OffsetDateTime,
    pub frozen_target_message_id: Uuid,
    pub system_task_type: String,
}

/// Computed settlement figures.
#[derive(Debug, Clone, Copy)]
pub struct SettlementCalc {
    pub method: SettlementMethod,
    pub committed_credits_micro: i64,
    pub actual_tokens: Option<(i64, i64)>,
    /// Usage reported in the outbox event (`None` on estimated paths).
    pub event_usage: Option<UsageTokens>,
}

fn usage_known(u: Option<UsageTokens>) -> Option<UsageTokens> {
    u.filter(|u| u.input_tokens > 0 || u.output_tokens > 0)
}

/// Usage reported by the provider stream, as an input to settlement.
#[derive(Debug, Clone, Copy)]
pub enum ReportedUsage {
    /// The stream completed; the usage block may still be missing.
    Completed(Option<UsageTokens>),
    /// The stream did not complete; usage counts only when it is known (non-zero).
    Partial(Option<UsageTokens>),
}

/// Settlement per DESIGN §5.7 / §5.8 / §5.9 (actual with overshoot cap, or estimated).
///
/// # Errors
/// Credit computation errors (finalization must fail).
#[allow(
    clippy::too_many_arguments,
    reason = "pure settlement formula; each figure is an independent input from the turn and policy"
)]
pub fn compute_settlement(
    reported: ReportedUsage,
    reserve_tokens: i64,
    max_output_tokens_applied: u32,
    floor_applied: u32,
    reserved_credits_micro: i64,
    in_mult: i64,
    out_mult: i64,
    overshoot_tolerance: f64,
) -> Result<SettlementCalc, DomainError> {
    let actual = match reported {
        ReportedUsage::Completed(u) => Some((u.unwrap_or_default(), u)),
        ReportedUsage::Partial(u) => usage_known(u).map(|u| (u, Some(u))),
    };
    if let Some((u, event_usage)) = actual {
        let actual_credits =
            credits_micro_checked(u.input_tokens, u.output_tokens, in_mult, out_mult)
                .map_err(|e| DomainError::internal(format!("credit computation failed: {e}")))?;
        let actual_tokens = u.input_tokens.saturating_add(u.output_tokens);
        let mut committed = actual_credits;
        if reserve_tokens > 0 && actual_tokens > reserve_tokens {
            #[allow(clippy::cast_precision_loss)]
            let factor = actual_tokens as f64 / reserve_tokens as f64;
            if factor > overshoot_tolerance {
                committed = reserved_credits_micro;
            }
        }
        return Ok(SettlementCalc {
            method: SettlementMethod::Actual,
            committed_credits_micro: committed,
            actual_tokens: Some((u.input_tokens, u.output_tokens)),
            event_usage,
        });
    }
    let est_input = (reserve_tokens - i64::from(max_output_tokens_applied)).max(0);
    let committed =
        credits_micro_checked(est_input, i64::from(floor_applied), in_mult, out_mult)
            .map_err(|e| DomainError::internal(format!("credit computation failed: {e}")))?;
    Ok(SettlementCalc {
        method: SettlementMethod::Estimated,
        committed_credits_micro: committed,
        actual_tokens: None,
        event_usage: None,
    })
}

#[must_use]
pub fn dedupe_key(tenant: Uuid, turn_id: Uuid, request_id: Uuid) -> String {
    format!(
        "{}/{}/{}",
        tenant.simple(),
        turn_id.simple(),
        request_id.simple()
    )
}

struct TxPlan {
    state: &'static str,
    error_code: Option<String>,
    insert_message: bool,
    billing_outcome: &'static str,
}

impl Core {
    async fn multipliers(&self, meta: &TurnMeta) -> Result<(i64, i64), DomainError> {
        let snap = self
            .policy
            .snapshot(meta.user_id, meta.policy_version)
            .await?;
        let m = snap.find_model(&meta.effective_model).ok_or_else(|| {
            DomainError::internal("effective model missing from the policy snapshot")
        })?;
        Ok((
            mult(m.input_tokens_credit_multiplier_micro),
            mult(m.output_tokens_credit_multiplier_micro),
        ))
    }

    /// Finalizes a turn and returns the terminal SSE events to send (empty after a cancel).
    #[allow(
        clippy::cognitive_complexity,
        reason = "exhaustive terminal-state x settlement/transaction-outcome matrix kept in one place"
    )]
    pub async fn finalize_turn(self: &Arc<Self>, inp: FinalizeInput) -> Vec<SseEvent> {
        let reserved = inp.meta.reserve.map_or(0, |r| r.reserved_credits_micro);
        let settlement = match self.multipliers(&inp.meta).await {
            Ok((im, om)) => {
                let reported = match &inp.terminal {
                    Terminal::Completed(c) => ReportedUsage::Completed(c.usage),
                    Terminal::Failed { usage, .. } => ReportedUsage::Partial(*usage),
                    Terminal::Cancelled => ReportedUsage::Partial(None),
                };
                compute_settlement(
                    reported,
                    inp.meta.reserve_tokens,
                    inp.meta.max_output_tokens_applied,
                    inp.meta.floor_applied,
                    reserved,
                    im,
                    om,
                    self.cfg.quota.overshoot_tolerance_factor,
                )
            }
            Err(e) => Err(e),
        };
        let plan = match &inp.terminal {
            Terminal::Completed(_) => TxPlan {
                state: "completed",
                error_code: None,
                insert_message: true,
                billing_outcome: "completed",
            },
            Terminal::Failed { code, .. } => TxPlan {
                state: "failed",
                error_code: Some(code.clone()),
                insert_message: false,
                billing_outcome: "failed",
            },
            Terminal::Cancelled => TxPlan {
                state: "cancelled",
                error_code: None,
                insert_message: !inp.text.is_empty(),
                billing_outcome: "aborted",
            },
        };
        let original_error = match &inp.terminal {
            Terminal::Failed { code, message, .. } => Some(SseEvent::error(code, message)),
            _ => None,
        };
        let settlement = match settlement {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(error = %e, turn_id = %inp.meta.turn_id, "mini-chat: settlement computation failed");
                return match &inp.terminal {
                    Terminal::Cancelled => Vec::new(),
                    Terminal::Failed { .. } => original_error.into_iter().collect(),
                    Terminal::Completed(_) => vec![SseEvent::error(
                        "finalization_failed",
                        "The turn could not be finalized",
                    )],
                };
            }
        };
        let result = self.finalize_tx(&inp, &plan, settlement).await;
        match result {
            Ok(true) => {}
            Ok(false) => {
                return if matches!(inp.terminal, Terminal::Cancelled) {
                    Vec::new()
                } else {
                    vec![SseEvent::error(
                        "stream_interrupted",
                        "The stream was interrupted",
                    )]
                };
            }
            Err(e) => {
                tracing::warn!(error = %e, turn_id = %inp.meta.turn_id, "mini-chat: finalization transaction failed");
                match &inp.terminal {
                    Terminal::Completed(_) => {
                        let fallback = TxPlan {
                            state: "failed",
                            error_code: Some("message_persistence_failed".to_owned()),
                            insert_message: false,
                            billing_outcome: "failed",
                        };
                        return match self.finalize_tx(&inp, &fallback, settlement).await {
                            Ok(true) => vec![SseEvent::error(
                                "message_persistence_failed",
                                "The answer could not be saved",
                            )],
                            Ok(false) => vec![SseEvent::error(
                                "stream_interrupted",
                                "The stream was interrupted",
                            )],
                            Err(_) => vec![SseEvent::error(
                                "finalization_failed",
                                "The turn could not be finalized",
                            )],
                        };
                    }
                    Terminal::Cancelled => {
                        let fallback = TxPlan {
                            state: "cancelled",
                            error_code: None,
                            insert_message: false,
                            billing_outcome: "aborted",
                        };
                        if let Err(e) = self.finalize_tx(&inp, &fallback, settlement).await {
                            tracing::debug!(error = %e, turn_id = %inp.meta.turn_id, "mini-chat: fallback cancel finalization failed");
                        }
                        return Vec::new();
                    }
                    Terminal::Failed { .. } => return original_error.into_iter().collect(),
                }
            }
        }
        match &inp.terminal {
            Terminal::Cancelled => Vec::new(),
            Terminal::Failed { .. } => original_error.into_iter().collect(),
            Terminal::Completed(c) => {
                let mut out = Vec::new();
                let items = map_citations(&c.citations, &inp.citation_map);
                if !items.is_empty() {
                    out.push(SseEvent::new("citations", json!({"items": items})));
                }
                let u = c.usage.unwrap_or_default();
                let mut done = json!({
                    "usage": {"input_tokens": u.input_tokens, "output_tokens": u.output_tokens},
                    "effective_model": inp.meta.effective_model,
                    "selected_model": inp.meta.selected_model,
                    "quota_decision": inp.meta.decision.as_str(),
                });
                if inp.meta.decision == QuotaDecision::Downgrade {
                    done["downgrade_from"] = json!(inp.meta.selected_model);
                    if let Some(r) = inp.meta.downgrade_reason {
                        done["downgrade_reason"] = json!(r);
                    }
                }
                if let Ok(periods) = self
                    .quota_periods(inp.meta.tenant_id, inp.meta.user_id)
                    .await
                {
                    let warnings: Vec<Value> = periods
                        .iter()
                        .map(|p| {
                            let mut w = json!({
                                "tier": p.tier,
                                "period": p.period.as_str(),
                                "remaining_percentage": p.remaining_percentage,
                                "warning": p.warning,
                                "exhausted": p.exhausted,
                            });
                            if p.warning || p.exhausted {
                                w["next_reset"] = json!(rfc3339(p.next_reset));
                            }
                            w
                        })
                        .collect();
                    done["quota_warnings"] = Value::Array(warnings);
                }
                out.push(SseEvent::new("done", done));
                out
            }
        }
    }

    /// Runs the finalization transaction. `Ok(false)` = CAS lost.
    #[allow(
        clippy::too_many_lines,
        reason = "single finalization transaction: CAS, message, settlement and outbox writes"
    )]
    async fn finalize_tx(
        self: &Arc<Self>,
        inp: &FinalizeInput,
        plan: &TxPlan,
        s: SettlementCalc,
    ) -> Result<bool, DomainError> {
        let core = Arc::clone(self);
        let meta = inp.meta.clone();
        let text = inp.text.clone();
        let counts = inp.counts;
        let state = plan.state;
        let error_code = plan.error_code.clone();
        let insert_message = plan.insert_message;
        let billing_outcome = plan.billing_outcome;
        let (usage_tokens, response_id) = match &inp.terminal {
            Terminal::Completed(c) => (c.usage, c.response_id.clone()),
            Terminal::Failed { usage, .. } => (*usage, None),
            Terminal::Cancelled => (None, None),
        };
        let latency = inp.latency_ms;
        let ttft = inp.ttft_ms;
        let summary_enabled = self.cfg.thread_summary_worker.enabled && state == "completed";
        let threshold = self.cfg.thread_summary_worker.compression_threshold_pct;
        let res = self
            .db
            .write_transaction(move |tx| {
                Box::pin(async move {
                    let ts = now();
                    let scope = tenant_scope(meta.tenant_id);
                    let mut upd = turn::Entity::update_many()
                        .col_expr(turn::Column::State, Expr::value(state))
                        .col_expr(turn::Column::ErrorCode, Expr::value(error_code.clone()))
                        .col_expr(turn::Column::CompletedAt, Expr::value(Some(ts)))
                        .col_expr(turn::Column::UpdatedAt, Expr::value(ts))
                        .col_expr(
                            turn::Column::WebSearchCompletedCount,
                            Expr::value(counts.web_search_completed),
                        )
                        .col_expr(
                            turn::Column::CodeInterpreterCompletedCount,
                            Expr::value(counts.code_interpreter_completed),
                        )
                        .col_expr(
                            turn::Column::FileSearchCompletedCount,
                            Expr::value(counts.file_search_completed),
                        );
                    if insert_message {
                        upd = upd.col_expr(
                            turn::Column::AssistantMessageId,
                            Expr::value(Some(meta.assistant_message_id)),
                        );
                    }
                    if let Some(r) = &response_id {
                        upd = upd.col_expr(
                            turn::Column::ProviderResponseId,
                            Expr::value(Some(r.clone())),
                        );
                    }
                    let won = upd
                        .filter(
                            Condition::all()
                                .add(turn::Column::Id.eq(meta.turn_id))
                                .add(turn::Column::State.eq("running")),
                        )
                        .secure()
                        .scope_with(&scope)
                        .exec(tx)
                        .await?
                        .rows_affected
                        == 1;
                    if !won {
                        return Ok((false, Wake::empty()));
                    }
                    if insert_message {
                        let u = usage_tokens.unwrap_or_default();
                        let am = message::ActiveModel {
                            id: ActiveValue::Set(meta.assistant_message_id),
                            tenant_id: ActiveValue::Set(meta.tenant_id),
                            chat_id: ActiveValue::Set(meta.chat_id),
                            request_id: ActiveValue::Set(Some(meta.request_id)),
                            role: ActiveValue::Set("assistant".to_owned()),
                            content: ActiveValue::Set(text.clone()),
                            content_type: ActiveValue::Set("text".to_owned()),
                            token_estimate: ActiveValue::Set(0),
                            provider_response_id: ActiveValue::Set(response_id.clone()),
                            request_kind: ActiveValue::Set("chat".to_owned()),
                            features_used: ActiveValue::Set(json!([])),
                            input_tokens: ActiveValue::Set(u.input_tokens.max(0)),
                            output_tokens: ActiveValue::Set(u.output_tokens.max(0)),
                            cache_read_input_tokens: ActiveValue::Set(
                                u.cache_read_input_tokens.max(0),
                            ),
                            cache_write_input_tokens: ActiveValue::Set(
                                u.cache_write_input_tokens.max(0),
                            ),
                            reasoning_tokens: ActiveValue::Set(u.reasoning_tokens.max(0)),
                            model: ActiveValue::Set(Some(meta.effective_model.clone())),
                            is_compressed: ActiveValue::Set(false),
                            created_at: ActiveValue::Set(ts),
                            deleted_at: ActiveValue::Set(None),
                        };
                        message::Entity::insert(am)
                            .secure()
                            .scope_unchecked(&scope)?
                            .exec(tx)
                            .await?;
                    }
                    if let Some(r) = meta.reserve {
                        settle_in_tx(
                            tx,
                            Settlement {
                                reserve: r,
                                committed_credits_micro: s.committed_credits_micro,
                                method: s.method,
                                actual_tokens: s.actual_tokens,
                                web_search_calls: counts.web_search_completed,
                                code_interpreter_calls: counts.code_interpreter_completed,
                            },
                        )
                        .await?;
                    }
                    let usage_event = UsageEvent {
                        tenant_id: meta.tenant_id,
                        user_id: Some(meta.user_id),
                        chat_id: Some(meta.chat_id),
                        turn_id: Some(meta.turn_id),
                        request_id: meta.request_id,
                        effective_model: meta.effective_model.clone(),
                        selected_model: meta.selected_model.clone(),
                        terminal_state: state.to_owned(),
                        billing_outcome: billing_outcome.to_owned(),
                        usage: s.event_usage,
                        actual_credits_micro: s.committed_credits_micro,
                        settlement_method: s.method.as_str().to_owned(),
                        policy_version_applied: meta.policy_version,
                        web_search_calls: u32::try_from(counts.web_search_completed).unwrap_or(0),
                        code_interpreter_calls: u32::try_from(counts.code_interpreter_completed)
                            .unwrap_or(0),
                        file_search_calls: u32::try_from(counts.file_search_completed).unwrap_or(0),
                        timestamp: ts,
                        requester_type: "user".to_owned(),
                        dedupe_key: dedupe_key(meta.tenant_id, meta.turn_id, meta.request_id),
                        system_task_type: None,
                    };
                    let mut wake = core
                        .outbox
                        .enqueue(tx, QueueKind::Usage, meta.tenant_id, &usage_event)
                        .await?;
                    let audit = AuditEvent::Turn(TurnAuditEvent {
                        event_type: if state == "completed" {
                            "turn_completed"
                        } else {
                            "turn_failed"
                        }
                        .to_owned(),
                        tenant_id: meta.tenant_id,
                        user_id: meta.user_id,
                        chat_id: meta.chat_id,
                        turn_id: meta.turn_id,
                        request_id: meta.request_id,
                        selected_model: meta.selected_model.clone(),
                        effective_model: meta.effective_model.clone(),
                        terminal_state: state.to_owned(),
                        error_code: error_code.clone(),
                        usage: usage_tokens,
                        latency_ms: Some(latency),
                        ttft_ms: ttft,
                        tool_calls: ToolCalls {
                            web_search_calls: u32::try_from(counts.web_search_completed)
                                .unwrap_or(0),
                            file_search_calls: u32::try_from(counts.file_search_completed)
                                .unwrap_or(0),
                        },
                        policy_decisions: PolicyDecisions {
                            quota: QuotaDecisionAudit {
                                decision: meta.decision.as_str().to_owned(),
                                downgrade_from: (meta.decision == QuotaDecision::Downgrade)
                                    .then(|| meta.selected_model.clone()),
                                downgrade_reason: meta.downgrade_reason.map(str::to_owned),
                            },
                            license: None,
                        },
                        prompt: String::new(),
                        response: String::new(),
                        attachments: Vec::new(),
                        quota_scope: String::new(),
                        trace_id: None,
                        timestamp: ts,
                    });
                    wake += core
                        .outbox
                        .enqueue(tx, QueueKind::Audit, meta.tenant_id, &audit)
                        .await?;
                    if summary_enabled {
                        let pct = i64::from(threshold);
                        let proactive = !meta.summary_exists
                            && meta.assembled_tokens * 100 >= meta.effective_budget * pct;
                        if (meta.messages_truncated || proactive)
                            && let Some(payload) = summary_payload(tx, &meta).await?
                        {
                            wake += core
                                .outbox
                                .enqueue(tx, QueueKind::ThreadSummary, meta.chat_id, &payload)
                                .await?;
                        }
                    }
                    Ok((true, wake))
                })
            })
            .await;
        match res {
            Ok((won, wake)) => {
                wake.fire();
                if won {
                    self.metrics
                        .turn_finalized(plan.state, plan.error_code.as_deref());
                }
                Ok(won)
            }
            Err(e) => Err(e),
        }
    }

    /// Orphan finalization (watchdog): its own CAS with the stale-progress predicate,
    /// estimated settlement, usage + audit events. Returns `true` when this call won.
    ///
    /// # Errors
    /// DB / policy errors.
    #[allow(
        clippy::cognitive_complexity,
        reason = "single CAS + settlement + event emission transaction; splitting would scatter the invariants"
    )]
    pub async fn finalize_orphan(
        self: &Arc<Self>,
        t: turn::Model,
        cutoff: OffsetDateTime,
    ) -> Result<bool, DomainError> {
        let user_id = t.requester_user_id;
        let reserve_fields = match (
            t.reserve_tokens,
            t.max_output_tokens_applied,
            t.reserved_credits_micro,
            t.policy_version_applied,
            t.effective_model.clone(),
            t.minimal_generation_floor_applied,
            user_id,
        ) {
            (Some(rt), Some(mo), Some(rc), Some(pv), Some(em), Some(fl), Some(uid)) => {
                Some((rt, mo, rc, pv, em, fl, uid))
            }
            _ => None,
        };
        let mut calc: Option<(SettlementCalc, ReserveSpec, ModelTier)> = None;
        if let Some((rt, mo, rc, pv, em, fl, uid)) = &reserve_fields {
            let version = u64::try_from(*pv).unwrap_or(0);
            let snap = self.policy.snapshot(*uid, version).await?;
            if let Some(m) = snap.find_model(em) {
                let s = compute_settlement(
                    ReportedUsage::Partial(None),
                    *rt,
                    u32::try_from(*mo).unwrap_or(0),
                    u32::try_from(*fl).unwrap_or(0),
                    *rc,
                    mult(m.input_tokens_credit_multiplier_micro),
                    mult(m.output_tokens_credit_multiplier_micro),
                    self.cfg.quota.overshoot_tolerance_factor,
                )?;
                let started = t.started_at;
                let spec = ReserveSpec {
                    tenant_id: t.tenant_id,
                    user_id: *uid,
                    tier: m.tier,
                    reserved_credits_micro: *rc,
                    daily_start: Period::Daily.start(started),
                    monthly_start: Period::Monthly.start(started),
                };
                calc = Some((s, spec, m.tier));
            } else {
                tracing::warn!(turn_id = %t.id, "mini-chat: orphan turn's model missing from snapshot; skipping settlement");
            }
        } else {
            tracing::warn!(turn_id = %t.id, "mini-chat: orphan turn without reserve fields; skipping settlement");
        }
        let core = Arc::clone(self);
        let row = t.clone();
        let res = self
            .db
            .write_transaction(move |tx| {
                Box::pin(async move {
                    let ts = now();
                    let scope = tenant_scope(row.tenant_id);
                    let stale = Condition::any()
                        .add(turn::Column::LastProgressAt.lte(cutoff))
                        .add(
                            Condition::all()
                                .add(turn::Column::LastProgressAt.is_null())
                                .add(turn::Column::StartedAt.lte(cutoff)),
                        );
                    let won = turn::Entity::update_many()
                        .col_expr(turn::Column::State, Expr::value("failed"))
                        .col_expr(
                            turn::Column::ErrorCode,
                            Expr::value(Some("orphan_timeout".to_owned())),
                        )
                        .col_expr(turn::Column::CompletedAt, Expr::value(Some(ts)))
                        .col_expr(turn::Column::UpdatedAt, Expr::value(ts))
                        .filter(
                            Condition::all()
                                .add(turn::Column::Id.eq(row.id))
                                .add(turn::Column::State.eq("running"))
                                .add(turn::Column::DeletedAt.is_null())
                                .add(stale),
                        )
                        .secure()
                        .scope_with(&scope)
                        .exec(tx)
                        .await?
                        .rows_affected
                        == 1;
                    if !won {
                        return Ok((false, Wake::empty()));
                    }
                    let (credits, method, effective, version) = if let Some((s, spec, _)) = calc {
                        settle_in_tx(
                            tx,
                            Settlement {
                                reserve: spec,
                                committed_credits_micro: s.committed_credits_micro,
                                method: s.method,
                                actual_tokens: None,
                                web_search_calls: row.web_search_completed_count,
                                code_interpreter_calls: row.code_interpreter_completed_count,
                            },
                        )
                        .await?;
                        (
                            s.committed_credits_micro,
                            s.method.as_str(),
                            row.effective_model.clone().unwrap_or_default(),
                            u64::try_from(row.policy_version_applied.unwrap_or(0)).unwrap_or(0),
                        )
                    } else {
                        (0, "estimated", String::new(), 0)
                    };
                    let usage_event = UsageEvent {
                        tenant_id: row.tenant_id,
                        user_id: row.requester_user_id,
                        chat_id: Some(row.chat_id),
                        turn_id: Some(row.id),
                        request_id: row.request_id,
                        effective_model: effective.clone(),
                        selected_model: effective.clone(),
                        terminal_state: "failed".to_owned(),
                        billing_outcome: "aborted".to_owned(),
                        usage: None,
                        actual_credits_micro: credits,
                        settlement_method: method.to_owned(),
                        policy_version_applied: version,
                        web_search_calls: u32::try_from(row.web_search_completed_count)
                            .unwrap_or(0),
                        code_interpreter_calls: u32::try_from(row.code_interpreter_completed_count)
                            .unwrap_or(0),
                        file_search_calls: u32::try_from(row.file_search_completed_count)
                            .unwrap_or(0),
                        timestamp: ts,
                        requester_type: row.requester_type.clone(),
                        dedupe_key: dedupe_key(row.tenant_id, row.id, row.request_id),
                        system_task_type: None,
                    };
                    let mut wake = core
                        .outbox
                        .enqueue(tx, QueueKind::Usage, row.tenant_id, &usage_event)
                        .await?;
                    if let Some(uid) = row.requester_user_id {
                        let audit = AuditEvent::Turn(TurnAuditEvent {
                            event_type: "turn_failed".to_owned(),
                            tenant_id: row.tenant_id,
                            user_id: uid,
                            chat_id: row.chat_id,
                            turn_id: row.id,
                            request_id: row.request_id,
                            selected_model: effective.clone(),
                            effective_model: effective,
                            terminal_state: "failed".to_owned(),
                            error_code: Some("orphan_timeout".to_owned()),
                            usage: None,
                            latency_ms: None,
                            ttft_ms: None,
                            tool_calls: ToolCalls {
                                web_search_calls: u32::try_from(row.web_search_completed_count)
                                    .unwrap_or(0),
                                file_search_calls: u32::try_from(row.file_search_completed_count)
                                    .unwrap_or(0),
                            },
                            policy_decisions: PolicyDecisions {
                                quota: QuotaDecisionAudit {
                                    decision: "unknown".to_owned(),
                                    downgrade_from: None,
                                    downgrade_reason: None,
                                },
                                license: None,
                            },
                            prompt: String::new(),
                            response: String::new(),
                            attachments: Vec::new(),
                            quota_scope: String::new(),
                            trace_id: None,
                            timestamp: ts,
                        });
                        wake += core
                            .outbox
                            .enqueue(tx, QueueKind::Audit, row.tenant_id, &audit)
                            .await?;
                    }
                    Ok((true, wake))
                })
            })
            .await?;
        let (won, wake) = res;
        wake.fire();
        if won {
            self.metrics
                .turn_finalized("failed", Some("orphan_timeout"));
        }
        Ok(won)
    }
}

/// Builds the thread-summary payload when there is something to summarize.
async fn summary_payload(
    tx: &impl DBRunner,
    meta: &TurnMeta,
) -> Result<Option<ThreadSummaryPayload>, DomainError> {
    let scope = tenant_scope(meta.tenant_id);
    let target = message::Entity::find()
        .filter(
            Condition::all()
                .add(message::Column::ChatId.eq(meta.chat_id))
                .add(message::Column::DeletedAt.is_null())
                .add(
                    Condition::any()
                        .add(message::Column::RequestId.is_null())
                        .add(message::Column::RequestId.ne(meta.request_id)),
                ),
        )
        .order_by(message::Column::CreatedAt, Order::Desc)
        .order_by(message::Column::Id, Order::Desc)
        .limit(1)
        .secure()
        .scope_with(&scope)
        .one(tx)
        .await?;
    let Some(target) = target else {
        return Ok(None);
    };
    let summary = load_summary(tx, meta.tenant_id, meta.chat_id).await?;
    if let Some(s) = &summary
        && (target.created_at, target.id)
            <= (s.summarized_up_to_created_at, s.summarized_up_to_message_id)
    {
        return Ok(None);
    }
    Ok(Some(ThreadSummaryPayload {
        tenant_id: meta.tenant_id,
        chat_id: meta.chat_id,
        system_request_id: Uuid::new_v4(),
        base_frontier_created_at: summary.as_ref().map(|s| s.summarized_up_to_created_at),
        base_frontier_message_id: summary.as_ref().map(|s| s.summarized_up_to_message_id),
        frozen_target_created_at: target.created_at,
        frozen_target_message_id: target.id,
        system_task_type: "thread_summary_update".to_owned(),
    }))
}

/// Maps raw citations; file citations resolve to non-deleted attachments of the chat (others omitted).
#[must_use]
pub fn map_citations<S: BuildHasher>(
    raw: &[RawCitation],
    map: &HashMap<String, (Uuid, String), S>,
) -> Vec<Value> {
    raw.iter()
        .filter_map(|c| match c {
            RawCitation::Web { url, title, snippet, span } => {
                let mut v = json!({"source": "web", "title": title, "url": url, "snippet": snippet});
                if let Some((s, e)) = span {
                    v["span"] = json!({"start": s, "end": e});
                }
                Some(v)
            }
            RawCitation::File { file_id, .. } => map.get(file_id).map(|(id, filename)| {
                json!({"source": "file", "title": filename, "attachment_id": id, "snippet": ""})
            }),
        })
        .collect()
}

#[must_use]
pub fn rfc3339(t: OffsetDateTime) -> String {
    t.format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default()
}

/// Period starts of a date (helper for tests).
#[must_use]
pub fn period_starts(t: OffsetDateTime) -> (Date, Date) {
    (Period::Daily.start(t), Period::Monthly.start(t))
}
