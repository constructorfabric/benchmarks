//! Settlement computation (`cpt-cf-mini-chat-algo-quota-and-billing-settlement-computation`):
//! committed credits, charged tokens and telemetry from the persisted billing columns of the
//! turn and the snapshot entry of the applied policy version; deterministic, reads nothing
//! else.

use mini_chat_sdk::{ModelCatalogEntry, UsageTokens};
use toolkit_macros::domain_model;

use crate::domain::quota::billing::SettlementMethod;
use crate::domain::quota::credits::{CreditError, credits_micro};

/// The persisted per-turn billing columns of `chat_turns` (immutable after the reserve).
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnBilling {
    pub reserve_tokens: i64,
    pub max_output_tokens_applied: i64,
    pub reserved_credits_micro: i64,
    pub minimal_generation_floor_applied: i64,
    pub policy_version_applied: u64,
    pub effective_model: String,
}

/// What a settlement commits.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SettlementComputation {
    pub method: SettlementMethod,
    /// The credits of the provider usage (actual) or of the estimated formula, before the
    /// overshoot cap.
    pub actual_credits_micro: i64,
    /// The credits added to `spent_credits_micro`: `actual_credits_micro`, or
    /// `reserved_credits_micro` when the overshoot cap applies.
    pub committed_credits_micro: i64,
    /// Actual: the committed tokens (`reserve_tokens` when capped); estimated:
    /// `min(reserve_tokens, estimated_input_tokens + floor)`; released: 0.
    pub charged_tokens: i64,
    /// The provider-reported tokens added to the `total` row telemetry, uncapped; 0 unless the
    /// method is `actual`.
    pub telemetry_input_tokens: i64,
    pub telemetry_output_tokens: i64,
    /// The actual tokens exceed `reserve_tokens` (recorded in `mini_chat_quota_overshoot`).
    pub overshoot: bool,
    /// The overshoot factor exceeds the tolerance: the committed credits are the reserve.
    pub overshoot_capped: bool,
}

/// Compute the settlement. `estimated_input_tokens` is `reserve_tokens` minus
/// `max_output_tokens_applied`; the multipliers come from `entry` (the persisted
/// `effective_model` in the snapshot of `policy_version_applied`). The overshoot factor is
/// `actual_tokens / reserve_tokens` in floating point (never integer division) and applies to
/// every actual settlement.
///
/// # Errors
///
/// [`CreditError`] when the credit computation fails; the finalization transaction fails.
pub fn compute_settlement(
    billing: &TurnBilling,
    method: SettlementMethod,
    usage: Option<&UsageTokens>,
    entry: &ModelCatalogEntry,
    overshoot_tolerance_factor: f64,
) -> Result<SettlementComputation, CreditError> {
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-settlement-computation:p1:inst-settle-compute-inputs
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-settlement-computation:p1:inst-settle-determinism
    // only the persisted columns and the multipliers of the applied snapshot entry are read:
    // no estimation budget but the persisted floor, no current configuration or policy version
    let estimated_input_tokens = billing
        .reserve_tokens
        .saturating_sub(billing.max_output_tokens_applied)
        .max(0);
    let in_mult = entry.input_tokens_credit_multiplier_micro;
    let out_mult = entry.output_tokens_credit_multiplier_micro;
    // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-settlement-computation:p1:inst-settle-determinism
    // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-settlement-computation:p1:inst-settle-compute-inputs
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-settlement-computation:p1:inst-settle-compute-match
    match method {
        SettlementMethod::Actual => settle_actual(
            billing,
            usage,
            in_mult,
            out_mult,
            overshoot_tolerance_factor,
        ),
        SettlementMethod::Estimated => {
            settle_estimated(billing, estimated_input_tokens, in_mult, out_mult)
        }
        SettlementMethod::Released => Ok(settle_released()),
    }
    // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-settlement-computation:p1:inst-settle-compute-match
}

/// The provider-reported tokens as the signed count of the credit arithmetic; a count that does
/// not fit is reported as an invalid token count.
fn reported_tokens(tokens: u64) -> Result<i64, CreditError> {
    i64::try_from(tokens).map_err(|_| CreditError::InvalidTokenCount { tokens: i64::MAX })
}

/// Non-negative token counts as a floating-point value for the overshoot ratio; exact below
/// 2^53 and the nearest value above it.
fn ratio_operand(tokens: i64) -> f64 {
    let unsigned = u64::try_from(tokens).unwrap_or(0);
    let high = u32::try_from(unsigned >> 32).unwrap_or(u32::MAX);
    let low = u32::try_from(unsigned & u64::from(u32::MAX)).unwrap_or(u32::MAX);
    f64::from(high) * 4_294_967_296.0 + f64::from(low)
}

fn settle_actual(
    billing: &TurnBilling,
    usage: Option<&UsageTokens>,
    in_mult: u64,
    out_mult: u64,
    overshoot_tolerance_factor: f64,
) -> Result<SettlementComputation, CreditError> {
    // @cpt-begin:cpt-cf-mini-chat-state-quota-and-billing-turn-reserve:p1:inst-reserve-state-actual
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-settlement-computation:p1:inst-settle-compute-actual
    // a missing field inside a present usage object counts as 0, and a turn without usage
    // settles 0 credits; an out-of-range count fails here, at settlement
    let (input, output) = match usage {
        Some(tokens) => (
            reported_tokens(tokens.input_tokens)?,
            reported_tokens(tokens.output_tokens)?,
        ),
        None => (0, 0),
    };
    let actual_credits_micro = credits_micro(input, output, in_mult, out_mult)?;
    // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-settlement-computation:p1:inst-settle-compute-actual
    // the counts are at most `MAX_TOKEN_COUNT` each after the credit arithmetic accepted them
    let actual_tokens = input.saturating_add(output);
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-settlement-computation:p1:inst-settle-overshoot
    let overshoot = actual_tokens > billing.reserve_tokens;
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-settlement-computation:p1:inst-settle-overshoot-factor
    // the factor is a floating-point ratio, never an integer division
    let factor = ratio_operand(actual_tokens) / ratio_operand(billing.reserve_tokens);
    // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-settlement-computation:p1:inst-settle-overshoot-factor
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-settlement-computation:p1:inst-settle-overshoot-cap
    // above the tolerance the reserve is committed (a silent cap: no log, no dedicated metric)
    // and the completed turn stays completed
    let overshoot_capped = overshoot && factor > overshoot_tolerance_factor;
    let (committed_credits_micro, charged_tokens) = if overshoot_capped {
        (billing.reserved_credits_micro, billing.reserve_tokens)
    } else {
        (actual_credits_micro, actual_tokens)
    };
    // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-settlement-computation:p1:inst-settle-overshoot-cap
    // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-settlement-computation:p1:inst-settle-overshoot
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-settlement-computation:p1:inst-settle-committed-vs-actual
    // the telemetry is the provider tokens, uncapped, on an `actual` settlement only
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-settlement-computation:p1:inst-settle-compute-return
    Ok(SettlementComputation {
        method: SettlementMethod::Actual,
        actual_credits_micro,
        committed_credits_micro,
        charged_tokens,
        telemetry_input_tokens: input,
        telemetry_output_tokens: output,
        overshoot,
        overshoot_capped,
    })
    // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-settlement-computation:p1:inst-settle-compute-return
    // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-settlement-computation:p1:inst-settle-committed-vs-actual
    // @cpt-end:cpt-cf-mini-chat-state-quota-and-billing-turn-reserve:p1:inst-reserve-state-actual
}

fn settle_estimated(
    billing: &TurnBilling,
    estimated_input_tokens: i64,
    in_mult: u64,
    out_mult: u64,
) -> Result<SettlementComputation, CreditError> {
    // @cpt-begin:cpt-cf-mini-chat-state-quota-and-billing-turn-reserve:p1:inst-reserve-state-estimated
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-settlement-computation:p1:inst-settle-compute-estimated
    // the persisted floor is the charged output; never more than the reserve, and never the
    // full reserved credits unless the floor is the whole output cap
    let charged_output_tokens = billing.minimal_generation_floor_applied;
    let charged_tokens = billing
        .reserve_tokens
        .min(estimated_input_tokens.saturating_add(charged_output_tokens));
    let credits = credits_micro(
        estimated_input_tokens,
        charged_output_tokens,
        in_mult,
        out_mult,
    )?;
    let committed_credits_micro = credits.min(billing.reserved_credits_micro);
    // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-settlement-computation:p1:inst-settle-compute-estimated
    Ok(SettlementComputation {
        method: SettlementMethod::Estimated,
        actual_credits_micro: committed_credits_micro,
        committed_credits_micro,
        charged_tokens,
        telemetry_input_tokens: 0,
        telemetry_output_tokens: 0,
        overshoot: false,
        overshoot_capped: false,
    })
    // @cpt-end:cpt-cf-mini-chat-state-quota-and-billing-turn-reserve:p1:inst-reserve-state-estimated
}

fn settle_released() -> SettlementComputation {
    // @cpt-begin:cpt-cf-mini-chat-state-quota-and-billing-turn-reserve:p1:inst-reserve-state-released
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-settlement-computation:p1:inst-settle-compute-released
    // 0 tokens and 0 credits: the whole reserve is released
    SettlementComputation {
        method: SettlementMethod::Released,
        actual_credits_micro: 0,
        committed_credits_micro: 0,
        charged_tokens: 0,
        telemetry_input_tokens: 0,
        telemetry_output_tokens: 0,
        overshoot: false,
        overshoot_capped: false,
    }
    // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-settlement-computation:p1:inst-settle-compute-released
    // @cpt-end:cpt-cf-mini-chat-state-quota-and-billing-turn-reserve:p1:inst-reserve-state-released
}

#[cfg(test)]
#[path = "settlement_tests.rs"]
mod settlement_tests;
