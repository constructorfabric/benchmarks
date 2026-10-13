//! Settlement, usage events (DESIGN §5.4.4, §5.4.5, §5.6, §5.7, §5.8).

use mini_chat_sdk::UsageEvent;
use sea_orm::sea_query::Expr;
use time::OffsetDateTime;
use toolkit_db::DbTx;
use toolkit_db::outbox::Wake;

use super::buckets;
use super::preflight::buckets_for;
use super::{BUCKET_TOTAL, Settlement, SettlementInput, SettlementMethod};
use crate::domain::error::DomainError;
use crate::domain::services::AppServices;
use crate::infra::db::entities::quota_usage::Column;
use crate::infra::outbox::PAYLOAD_USAGE;

/// Persisted reserve of a turn (all preflight columns present).
struct TurnReserve<'a> {
    reserve_tokens: i64,
    max_output_tokens_applied: i64,
    reserved_credits_micro: i64,
    policy_version: i64,
    effective_model: &'a str,
    floor: i64,
}

fn turn_reserve(turn: &crate::infra::db::entities::chat_turn::Model) -> Option<TurnReserve<'_>> {
    Some(TurnReserve {
        reserve_tokens: turn.reserve_tokens?,
        max_output_tokens_applied: i64::from(turn.max_output_tokens_applied?),
        reserved_credits_micro: turn.reserved_credits_micro?,
        policy_version: turn.policy_version_applied?,
        effective_model: turn.effective_model.as_deref()?,
        floor: i64::from(turn.minimal_generation_floor_applied.unwrap_or(0)),
    })
}

fn clamp_i32(v: u32) -> i32 {
    i32::try_from(v).unwrap_or(i32::MAX)
}

pub(super) async fn settle(app: &AppServices, tx: &DbTx<'_>, input: &SettlementInput) -> Result<Settlement, DomainError> {
    let turn = &input.turn;
    let Some(r) = turn_reserve(turn) else {
        tracing::warn!(turn_id = %turn.id, "turn has no reserve; settlement skipped");
        return Ok(Settlement {
            method: input.method,
            billing_outcome: input.billing_outcome,
            actual_credits_micro: 0,
            overshoot_capped: false,
        });
    };
    let snapshot = app.policy.snapshot_by_version(input.user_id, r.policy_version).await?;
    let model = snapshot.model(r.effective_model).ok_or_else(|| {
        DomainError::internal(format!(
            "effective model {} not found in policy snapshot {}",
            r.effective_model, r.policy_version
        ))
    })?;
    let (in_mult, out_mult) = (model.input_tokens_credit_multiplier_micro, model.output_tokens_credit_multiplier_micro);
    let credit_err = |e: String| {
        tracing::warn!(turn_id = %turn.id, error = %e, "settlement credit computation failed");
        DomainError::internal(format!("settlement credit computation failed: {e}"))
    };

    let usage = input.usage.unwrap_or_default();
    let mut capped = false;
    let committed = match input.method {
        SettlementMethod::Actual => {
            let actual = super::credits_micro(usage.input_tokens, usage.output_tokens, in_mult, out_mult).map_err(credit_err)?;
            let actual_tokens = usage.input_tokens.saturating_add(usage.output_tokens);
            if actual_tokens > r.reserve_tokens {
                for period in [super::PERIOD_DAILY, super::PERIOD_MONTHLY] {
                    crate::infra::metrics::incr("mini_chat_quota_overshoot_total", 1, &[("period", period.to_owned())]);
                }
                #[allow(clippy::cast_precision_loss)]
                let factor = actual_tokens as f64 / (r.reserve_tokens.max(1) as f64);
                if factor > app.cfg.quota.overshoot_tolerance_factor {
                    capped = true;
                    r.reserved_credits_micro
                } else {
                    actual
                }
            } else {
                actual
            }
        }
        SettlementMethod::Estimated => {
            let est_in = r.reserve_tokens.saturating_sub(r.max_output_tokens_applied).max(0);
            super::credits_micro(est_in, r.floor, in_mult, out_mult).map_err(credit_err)?
        }
        SettlementMethod::Released => 0,
    };

    let now = crate::clock::now();
    let (tenant_id, user_id) = (input.tenant_id, input.user_id);
    let is_actual = input.method == SettlementMethod::Actual;
    let counts_tools = input.method != SettlementMethod::Released;
    let tier = model.tier;
    for (period_type, start) in buckets::periods_of(input.periods) {
        for bucket in buckets_for(tier) {
            buckets::ensure_row(tx, tenant_id, user_id, period_type, start, bucket, now).await?;
            let mut exprs = vec![
                buckets::sub_floor_expr(Column::ReservedCreditsMicro, r.reserved_credits_micro),
                buckets::add_expr(Column::SpentCreditsMicro, committed),
                buckets::add_expr(Column::Calls, 1_i32),
                (Column::UpdatedAt, Expr::value(now)),
            ];
            if *bucket == BUCKET_TOTAL {
                if is_actual {
                    exprs.push(buckets::add_expr(Column::InputTokens, usage.input_tokens));
                    exprs.push(buckets::add_expr(Column::OutputTokens, usage.output_tokens));
                }
                if counts_tools {
                    exprs.push(buckets::add_expr(Column::WebSearchCalls, clamp_i32(input.web_search_calls)));
                    exprs.push(buckets::add_expr(Column::CodeInterpreterCalls, clamp_i32(input.code_interpreter_calls)));
                }
            }
            buckets::update_row(tx, tenant_id, user_id, period_type, start, bucket, exprs).await?;
        }
    }
    Ok(Settlement {
        method: input.method,
        billing_outcome: input.billing_outcome,
        actual_credits_micro: committed,
        overshoot_capped: capped,
    })
}

/// `{tenant}/{turn}/{request}` in simple (32-hex) UUID form.
#[must_use]
pub fn dedupe_key(tenant_id: uuid::Uuid, turn_id: uuid::Uuid, request_id: uuid::Uuid) -> String {
    format!("{}/{}/{}", tenant_id.simple(), turn_id.simple(), request_id.simple())
}

pub(super) fn usage_event(
    input: &SettlementInput,
    settlement: &Settlement,
    selected_model: &str,
    file_search_calls: u32,
    terminal_state: &str,
    now: OffsetDateTime,
) -> UsageEvent {
    let turn = &input.turn;
    let requester_type = if turn.requester_type.is_empty() { "user".to_owned() } else { turn.requester_type.clone() };
    UsageEvent {
        tenant_id: input.tenant_id,
        user_id: (!input.user_id.is_nil()).then_some(input.user_id),
        chat_id: turn.chat_id,
        turn_id: Some(turn.id),
        request_id: turn.request_id,
        effective_model: turn.effective_model.clone().unwrap_or_default(),
        selected_model: selected_model.to_owned(),
        terminal_state: terminal_state.to_owned(),
        billing_outcome: settlement.billing_outcome.as_str().to_owned(),
        usage: if settlement.method == SettlementMethod::Actual { input.usage } else { None },
        actual_credits_micro: settlement.actual_credits_micro,
        settlement_method: settlement.method.as_str().to_owned(),
        policy_version_applied: turn.policy_version_applied.unwrap_or(0),
        web_search_calls: input.web_search_calls,
        code_interpreter_calls: input.code_interpreter_calls,
        file_search_calls,
        timestamp: now,
        requester_type,
        dedupe_key: dedupe_key(input.tenant_id, turn.id, turn.request_id),
        system_task_type: None,
    }
}

pub(super) async fn enqueue_usage(app: &AppServices, tx: &DbTx<'_>, event: &UsageEvent) -> Result<Wake, DomainError> {
    app.outbox.enqueue_json(tx, app.outbox.usage_queue(), event.tenant_id, PAYLOAD_USAGE, event).await
}
