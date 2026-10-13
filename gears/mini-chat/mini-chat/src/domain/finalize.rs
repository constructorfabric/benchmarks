//! Turn finalization contract (DESIGN §5.7–§5.9): CAS on `chat_turns.state`,
//! quota settlement and outbox emission (usage + audit, optional thread
//! summary) in one transaction. Shared by the stream terminal paths and the
//! orphan watchdog.

use mini_chat_sdk::{
    AuditEvent, AuditUsage, LatencyMs, ModelTier, PolicyDecisions, QuotaPolicyDecision, ToolCalls,
    TurnAuditEvent, UsageEvent, UsageTokens,
};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use toolkit_db::secure::DBRunner;
use toolkit_security::AccessScope;
use uuid::Uuid;

use crate::domain::credits::credits_micro_checked;
use crate::domain::errors::{DomainError, DomainResult};
use crate::domain::quota::{BUCKET_TOTAL, PERIOD_DAILY, PERIOD_MONTHLY, Periods, buckets_for};
use crate::domain::state::AppState;
use crate::infra::db::repo::{self, BucketDelta, NewMessage, TerminalUpdate};
use crate::infra::llm::Usage;
use crate::infra::outbox::{Queue, Wakes};

pub const MESSAGE_PERSISTENCE_MARKER: &str = "__message_persistence_failed__";

/// Persisted preflight data of a turn needed for settlement.
#[derive(Debug, Clone)]
pub struct TurnBilling {
    pub tenant_id: Uuid,
    pub user_id: Option<Uuid>,
    pub chat_id: Uuid,
    pub turn_id: Uuid,
    pub request_id: Uuid,
    pub selected_model: String,
    pub effective_model: String,
    pub tier: ModelTier,
    pub policy_version: u64,
    pub reserve_tokens: i64,
    pub max_output_tokens_applied: i64,
    pub reserved_credits_micro: i64,
    pub floor: i64,
    pub periods: Periods,
    pub in_mult: i64,
    pub out_mult: i64,
    /// `false` when the turn never booked a reserve (settlement skipped).
    pub has_reserve: bool,
}

/// Billing outcome derivation (DESIGN §5.8, normative mapping).
#[must_use]
pub fn derive_billing(state: &str, error_code: Option<&str>, usage: Option<&Usage>) -> (&'static str, &'static str) {
    match state {
        repo::STATE_COMPLETED => ("completed", "actual"),
        repo::STATE_CANCELLED => ("aborted", "estimated"),
        _ => match error_code {
            Some("orphan_timeout") => ("aborted", "estimated"),
            Some("context_length_exceeded" | "validation_error" | "input_too_long" | "turn_setup_failed") => {
                ("failed", "released")
            }
            Some(
                "provider_error"
                | "provider_timeout"
                | "rate_limited"
                | "web_search_calls_exceeded"
                | "code_interpreter_calls_exceeded"
                | "agentic_iterations_exceeded"
                | "unexpected_tool_use"
                | "message_persistence_failed",
            ) => {
                if usage.is_some_and(Usage::is_nonzero) {
                    ("failed", "actual")
                } else {
                    ("failed", "estimated")
                }
            }
            other => {
                tracing::error!(error_code = ?other, "unknown error code at settlement");
                ("failed", "estimated")
            }
        },
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Settlement {
    pub credits: i64,
    pub tel_input: i64,
    pub tel_output: i64,
    pub capped: bool,
}

/// Committed credits for a settlement method.
///
/// # Errors
/// Credit computation failure (out-of-range tokens or multipliers).
pub fn compute_settlement(
    b: &TurnBilling,
    method: &str,
    usage: Option<&Usage>,
    tolerance: f64,
) -> DomainResult<Settlement> {
    let map = |e| DomainError::internal(format!("credit computation failed: {e}"));
    match method {
        "actual" => {
            let u = usage.copied().unwrap_or_default();
            let actual = credits_micro_checked(u.input_tokens, u.output_tokens, b.in_mult, b.out_mult).map_err(map)?;
            let actual_tokens = u.input_tokens.saturating_add(u.output_tokens);
            let mut credits = actual;
            let mut capped = false;
            #[allow(clippy::cast_precision_loss)]
            if b.reserve_tokens > 0 && actual_tokens > b.reserve_tokens {
                let factor = actual_tokens as f64 / b.reserve_tokens as f64;
                if factor > tolerance {
                    credits = b.reserved_credits_micro;
                    capped = true;
                }
            }
            Ok(Settlement {
                credits,
                tel_input: u.input_tokens,
                tel_output: u.output_tokens,
                capped,
            })
        }
        "estimated" => {
            let est_in = (b.reserve_tokens - b.max_output_tokens_applied).max(0);
            let credits = credits_micro_checked(est_in, b.floor.max(0), b.in_mult, b.out_mult).map_err(map)?;
            Ok(Settlement {
                credits,
                tel_input: 0,
                tel_output: 0,
                capped: false,
            })
        }
        _ => Ok(Settlement {
            credits: 0,
            tel_input: 0,
            tel_output: 0,
            capped: false,
        }),
    }
}

/// Apply a settlement to the turn's bucket rows (same periods as the reserve).
pub async fn apply_settlement(
    runner: &impl DBRunner,
    b: &TurnBilling,
    method: &str,
    s: Settlement,
    web_search_calls: i64,
    code_interpreter_calls: i64,
) -> DomainResult<()> {
    let Some(user_id) = b.user_id else {
        return Ok(());
    };
    if !b.has_reserve {
        return Ok(());
    }
    let count_tools = method != "released";
    for bucket in buckets_for(b.tier) {
        for period in [PERIOD_DAILY, PERIOD_MONTHLY] {
            let total = *bucket == BUCKET_TOTAL;
            let d = BucketDelta {
                reserved: -b.reserved_credits_micro,
                spent: s.credits,
                calls: 1,
                input_tokens: if total && method == "actual" { s.tel_input } else { 0 },
                output_tokens: if total && method == "actual" { s.tel_output } else { 0 },
                web_search_calls: if total && count_tools { web_search_calls } else { 0 },
                code_interpreter_calls: if total && count_tools { code_interpreter_calls } else { 0 },
            };
            repo::apply_bucket_delta(runner, b.tenant_id, user_id, period, b.periods.start(period), bucket, d).await?;
        }
    }
    Ok(())
}

/// Book a reserve on the turn's buckets and re-check the limits. Returns
/// `false` when a bucket is over its limit (caller rolls back, 429).
pub async fn book_reserve(
    runner: &impl DBRunner,
    tenant_id: Uuid,
    user_id: Uuid,
    tier: ModelTier,
    periods: Periods,
    reserved: i64,
    limits: &mini_chat_sdk::UserLimits,
) -> DomainResult<bool> {
    for bucket in buckets_for(tier) {
        for period in [PERIOD_DAILY, PERIOD_MONTHLY] {
            repo::apply_bucket_delta(
                runner,
                tenant_id,
                user_id,
                period,
                periods.start(period),
                bucket,
                BucketDelta { reserved, ..BucketDelta::default() },
            )
            .await?;
        }
    }
    for bucket in buckets_for(tier) {
        for period in [PERIOD_DAILY, PERIOD_MONTHLY] {
            let u = repo::read_bucket(runner, tenant_id, user_id, period, periods.start(period), bucket).await?;
            let limit = crate::domain::quota::limit_for(limits, bucket, period);
            if u.spent.saturating_add(u.reserved) > limit {
                return Ok(false);
            }
        }
    }
    Ok(true)
}

#[must_use]
pub fn dedupe_key(tenant_id: Uuid, turn_id: Uuid, request_id: Uuid) -> String {
    format!("{}/{}/{}", tenant_id.simple(), turn_id.simple(), request_id.simple())
}

#[must_use]
pub fn usage_tokens(u: &Usage) -> UsageTokens {
    UsageTokens {
        input_tokens: u.input_tokens,
        output_tokens: u.output_tokens,
        cache_read_input_tokens: u.cache_read_input_tokens,
        cache_write_input_tokens: u.cache_write_input_tokens,
        reasoning_tokens: u.reasoning_tokens,
    }
}

/// Thread-summary outbox payload.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ThreadSummaryPayload {
    pub tenant_id: Uuid,
    pub chat_id: Uuid,
    pub system_request_id: Uuid,
    #[serde(default, with = "time::serde::rfc3339::option")]
    pub base_frontier_created_at: Option<OffsetDateTime>,
    #[serde(default)]
    pub base_frontier_message_id: Option<Uuid>,
    #[serde(with = "time::serde::rfc3339")]
    pub frozen_target_created_at: OffsetDateTime,
    pub frozen_target_message_id: Uuid,
    pub system_task_type: String,
}

/// Everything a terminal path needs to finalize a turn.
#[derive(Debug, Clone)]
pub struct FinalizeInput {
    pub billing: TurnBilling,
    pub state: &'static str,
    pub error_code: Option<String>,
    pub error_detail: Option<String>,
    pub usage: Option<Usage>,
    pub provider_response_id: Option<String>,
    pub assistant_message: Option<NewMessage>,
    pub web_search_calls: i32,
    pub code_interpreter_calls: i32,
    pub file_search_calls: i32,
    pub quota_decision: String,
    pub downgrade_reason: Option<String>,
    pub ttft_ms: Option<u64>,
    pub total_ms: u64,
    pub summary: Option<ThreadSummaryPayload>,
    /// `true` for the orphan watchdog (its own CAS predicate).
    pub orphan_cutoff: Option<OffsetDateTime>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FinalizeOutcome {
    Won,
    Lost,
}

impl AppState {
    /// Run the finalization transaction. Returns `Lost` when another
    /// finalizer already moved the turn out of `running`.
    ///
    /// # Errors
    /// Transaction failure; a failed assistant-message insert is reported as
    /// `Internal` containing [`MESSAGE_PERSISTENCE_MARKER`].
    pub async fn finalize_turn(&self, input: FinalizeInput) -> DomainResult<FinalizeOutcome> {
        let (outcome, method) =
            derive_billing(input.state, input.error_code.as_deref(), input.usage.as_ref());
        let settlement = compute_settlement(
            &input.billing,
            method,
            input.usage.as_ref(),
            self.cfg.quota.overshoot_tolerance_factor,
        )?;
        let outbox = self.outbox.clone();
        let res = self
            .write_tx(move |tx| {
                let input = input.clone();
                let outbox = outbox.clone();
                Box::pin(async move {
                    let b = &input.billing;
                    let scope = AccessScope::for_tenant(b.tenant_id);
                    let now = repo::now();
                    let assistant_id = input.assistant_message.as_ref().map(|m| m.id);
                    let won = if let Some(cutoff) = input.orphan_cutoff {
                        crate::domain::watchdog::cas_orphan(tx, &scope, b.turn_id, cutoff, now).await?
                    } else {
                        repo::cas_finalize_turn(
                            tx,
                            &scope,
                            b.turn_id,
                            &TerminalUpdate {
                                state: input.state,
                                error_code: input.error_code.clone(),
                                error_detail: input.error_detail.clone(),
                                assistant_message_id: assistant_id,
                                provider_response_id: input.provider_response_id.clone(),
                                web_search_completed_count: Some(input.web_search_calls),
                                code_interpreter_completed_count: Some(input.code_interpreter_calls),
                                file_search_completed_count: Some(input.file_search_calls),
                            },
                            now,
                        )
                        .await?
                    };
                    if !won {
                        return Ok((FinalizeOutcome::Lost, Wakes::default()));
                    }
                    if let Some(m) = input.assistant_message.clone() {
                        repo::insert_message(tx, &scope, m).await.map_err(|e| {
                            DomainError::internal(format!("{MESSAGE_PERSISTENCE_MARKER}: {e}"))
                        })?;
                    }
                    apply_settlement(
                        tx,
                        b,
                        method,
                        settlement,
                        i64::from(input.web_search_calls),
                        i64::from(input.code_interpreter_calls),
                    )
                    .await?;
                    let mut wakes = Wakes::default();
                    let usage_ev = UsageEvent {
                        tenant_id: b.tenant_id,
                        user_id: b.user_id,
                        chat_id: b.chat_id,
                        turn_id: Some(b.turn_id),
                        request_id: b.request_id,
                        effective_model: b.effective_model.clone(),
                        selected_model: b.selected_model.clone(),
                        terminal_state: input.state.to_owned(),
                        billing_outcome: outcome.to_owned(),
                        usage: if method == "actual" {
                            Some(usage_tokens(&input.usage.unwrap_or_default()))
                        } else {
                            None
                        },
                        actual_credits_micro: if b.has_reserve { settlement.credits } else { 0 },
                        settlement_method: method.to_owned(),
                        policy_version_applied: b.policy_version,
                        web_search_calls: u32::try_from(input.web_search_calls).unwrap_or(0),
                        code_interpreter_calls: u32::try_from(input.code_interpreter_calls).unwrap_or(0),
                        file_search_calls: u32::try_from(input.file_search_calls).unwrap_or(0),
                        timestamp: now,
                        requester_type: "user".to_owned(),
                        dedupe_key: dedupe_key(b.tenant_id, b.turn_id, b.request_id),
                        system_task_type: None,
                    };
                    wakes.push(outbox.enqueue(tx, Queue::Usage, b.tenant_id, &usage_ev).await?);
                    let u = input.usage.unwrap_or_default();
                    let audit = AuditEvent::Turn(TurnAuditEvent {
                        event_type: if input.state == repo::STATE_COMPLETED {
                            "turn_completed".to_owned()
                        } else {
                            "turn_failed".to_owned()
                        },
                        timestamp: now,
                        tenant_id: b.tenant_id,
                        requester_type: "user".to_owned(),
                        actor_user_id: b.user_id,
                        chat_id: b.chat_id,
                        turn_id: b.turn_id,
                        request_id: b.request_id,
                        selected_model: b.selected_model.clone(),
                        effective_model: b.effective_model.clone(),
                        terminal_state: input.state.to_owned(),
                        error_code: input.error_code.clone(),
                        policy_version_applied: Some(b.policy_version),
                        usage: AuditUsage {
                            input_tokens: u.input_tokens,
                            output_tokens: u.output_tokens,
                            cache_read_input_tokens: u.cache_read_input_tokens,
                            cache_write_input_tokens: u.cache_write_input_tokens,
                            reasoning_tokens: u.reasoning_tokens,
                        },
                        latency_ms: LatencyMs {
                            ttft_ms: input.ttft_ms,
                            total_ms: input.total_ms,
                        },
                        tool_calls: ToolCalls {
                            web_search_calls: u32::try_from(input.web_search_calls).unwrap_or(0),
                            file_search_calls: u32::try_from(input.file_search_calls).unwrap_or(0),
                        },
                        policy_decisions: PolicyDecisions {
                            license: None,
                            quota: QuotaPolicyDecision {
                                decision: input.quota_decision.clone(),
                                downgrade_from: (input.quota_decision == "downgrade")
                                    .then(|| b.selected_model.clone()),
                                downgrade_reason: input.downgrade_reason.clone(),
                                quota_scope: None,
                            },
                        },
                        prompt: String::new(),
                        response: String::new(),
                        attachments: Vec::new(),
                        trace_id: None,
                    });
                    wakes.push(outbox.enqueue(tx, Queue::Audit, b.tenant_id, &audit).await?);
                    if let Some(p) = &input.summary {
                        wakes.push(outbox.enqueue(tx, Queue::ThreadSummary, p.chat_id, p).await?);
                    }
                    Ok((FinalizeOutcome::Won, wakes))
                })
            })
            .await?;
        let (o, wakes) = res;
        wakes.fire();
        Ok(o)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn billing() -> TurnBilling {
        TurnBilling {
            tenant_id: Uuid::nil(),
            user_id: Some(Uuid::nil()),
            chat_id: Uuid::nil(),
            turn_id: Uuid::nil(),
            request_id: Uuid::nil(),
            selected_model: "m".into(),
            effective_model: "m".into(),
            tier: ModelTier::Standard,
            policy_version: 1,
            reserve_tokens: 1500,
            max_output_tokens_applied: 500,
            reserved_credits_micro: 1500,
            floor: 50,
            periods: Periods::at(OffsetDateTime::UNIX_EPOCH),
            in_mult: 1_000_000,
            out_mult: 1_000_000,
            has_reserve: true,
        }
    }

    #[test]
    fn billing_mapping() {
        let u = Usage { input_tokens: 1, ..Usage::default() };
        assert_eq!(derive_billing("completed", None, None), ("completed", "actual"));
        assert_eq!(derive_billing("cancelled", None, Some(&u)), ("aborted", "estimated"));
        assert_eq!(derive_billing("failed", Some("orphan_timeout"), None), ("aborted", "estimated"));
        assert_eq!(derive_billing("failed", Some("provider_error"), Some(&u)), ("failed", "actual"));
        assert_eq!(derive_billing("failed", Some("provider_error"), Some(&Usage::default())), ("failed", "estimated"));
        assert_eq!(derive_billing("failed", Some("web_search_calls_exceeded"), None), ("failed", "estimated"));
        assert_eq!(derive_billing("failed", Some("turn_setup_failed"), None), ("failed", "released"));
        assert_eq!(derive_billing("failed", Some("weird"), None), ("failed", "estimated"));
    }

    #[test]
    fn settlement_formulas() {
        let b = billing();
        let u = Usage { input_tokens: 900, output_tokens: 300, ..Usage::default() };
        let s = compute_settlement(&b, "actual", Some(&u), 1.1).unwrap();
        assert_eq!(s.credits, 1200);
        let s = compute_settlement(&b, "estimated", None, 1.1).unwrap();
        assert_eq!(s.credits, 1050);
        let s = compute_settlement(&b, "released", None, 1.1).unwrap();
        assert_eq!(s.credits, 0);
        // overshoot beyond tolerance is capped at the reserve
        let u = Usage { input_tokens: 1700, output_tokens: 100, ..Usage::default() };
        let s = compute_settlement(&b, "actual", Some(&u), 1.1).unwrap();
        assert!(s.capped);
        assert_eq!(s.credits, 1500);
        // within tolerance is charged actual
        let u = Usage { input_tokens: 1550, output_tokens: 0, ..Usage::default() };
        let s = compute_settlement(&b, "actual", Some(&u), 1.1).unwrap();
        assert_eq!(s.credits, 1550);
    }

    #[test]
    fn dedupe_key_format() {
        let k = dedupe_key(Uuid::from_u128(1), Uuid::from_u128(2), Uuid::from_u128(3));
        assert_eq!(k.split('/').count(), 3);
        assert!(k.split('/').all(|p| p.len() == 32 && !p.contains('-')));
    }
}
