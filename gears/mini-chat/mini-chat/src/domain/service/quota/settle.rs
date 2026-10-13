//! Quota settlement (`cpt-cf-mini-chat-algo-quota-billing-settle`,
//! `cpt-cf-mini-chat-dod-quota-billing-settlement`): the pure planning of the
//! committed credits and of the bucket deltas. The database writes are applied
//! by the repository.

use mini_chat_sdk::{ModelTier, PolicySnapshot, SettlementMethod, UsageTokens};
use toolkit_macros::domain_model;

use super::arith::{CreditError, credits_micro};
use super::types::QuotaBucket;

/// Persisted columns and context of one settlement.
#[domain_model]
#[derive(Debug, Clone, Copy)]
pub struct SettleInput<'a> {
    /// `chat_turns.reserve_tokens`.
    pub reserve_tokens: i64,
    /// `chat_turns.max_output_tokens_applied`.
    pub max_output_tokens_applied: i64,
    /// `chat_turns.reserved_credits_micro` (`turn_reserved_credits_micro`).
    pub reserved_credits_micro: i64,
    /// `chat_turns.minimal_generation_floor_applied`.
    pub minimal_generation_floor_applied: i64,
    /// `chat_turns.effective_model`.
    pub effective_model: &'a str,
    /// The usable snapshot of `policy_version_applied`; `None` when it is
    /// unavailable (or of another version).
    pub snapshot: Option<&'a PolicySnapshot>,
    /// The settlement method derived from the billing outcome table.
    pub method: SettlementMethod,
    /// Provider usage (used by `actual` only).
    pub usage: Option<&'a UsageTokens>,
    /// `quota.overshoot_tolerance_factor`.
    pub overshoot_tolerance_factor: f64,
}

/// Why a settlement took the deterministic fallback.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FallbackCause {
    /// The snapshot of `policy_version_applied` is unavailable.
    SnapshotUnavailable,
    /// `effective_model` is missing from the snapshot.
    ModelMissing,
    /// A credit computation failed.
    Credit(CreditError),
}

/// The outcome of the settlement planning.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SettlePlan {
    /// The method applied (`estimated` after the deterministic fallback).
    pub method: SettlementMethod,
    /// The credits debited from the buckets.
    pub committed_credits_micro: i64,
    /// Input tokens added to `quota_usage.input_tokens` and to the telemetry.
    pub telemetry_input_tokens: i64,
    /// Output tokens added to `quota_usage.output_tokens` and to the telemetry.
    pub telemetry_output_tokens: i64,
    /// The overshoot cap applied (internal only).
    pub overshoot_capped: bool,
    /// Tier of the effective model from the snapshot entry; `None` when the
    /// snapshot gives none (the repository then reads the premium rows).
    pub tier: Option<ModelTier>,
    /// The fallback cause, if the deterministic fallback was taken.
    pub fallback: Option<FallbackCause>,
}

/// Plans the settlement: `actual` (credits of the usage, with the overshoot
/// cap), `estimated` (credits of `estimated_input_tokens` and the floor,
/// `estimated_input_tokens = reserve_tokens - max_output_tokens_applied`) and
/// `released` (0). Any snapshot or credit problem of `actual` / `estimated`
/// switches to the deterministic fallback: method `estimated`, committed
/// credits `reserved_credits_micro`, telemetry tokens 0 and 0, no overshoot
/// cap. `released` never falls back.
#[must_use]
pub fn plan_settlement(input: &SettleInput<'_>) -> SettlePlan {
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-settle:p1:inst-lookup-effective
    let entry = input.snapshot.and_then(|snapshot| {
        snapshot
            .model_catalog
            .iter()
            .find(|entry| entry.id == input.effective_model)
    });
    let tier = entry.map(|entry| entry.tier);
    // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-settle:p1:inst-lookup-effective
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-settle:p1:inst-match-method
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-settle:p1:inst-case-released
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-settle:p1:inst-fallback-not-released
    if input.method == SettlementMethod::Released {
        return SettlePlan {
            method: SettlementMethod::Released,
            committed_credits_micro: 0,
            telemetry_input_tokens: 0,
            telemetry_output_tokens: 0,
            overshoot_capped: false,
            tier,
            fallback: None,
        };
    }
    // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-settle:p1:inst-fallback-not-released
    // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-settle:p1:inst-case-released
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-settle:p1:inst-lookup-effective
    let Some(entry) = entry else {
        let cause = if input.snapshot.is_none() {
            FallbackCause::SnapshotUnavailable
        } else {
            FallbackCause::ModelMissing
        };
        return fallback(input, None, cause);
    };
    // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-settle:p1:inst-lookup-effective
    let in_mult = entry.input_tokens_credit_multiplier_micro;
    let out_mult = entry.output_tokens_credit_multiplier_micro;
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-settle:p1:inst-case-estimated
    if input.method == SettlementMethod::Estimated {
        // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-settle:p1:inst-derive-estimated-input
        let estimated_input = input
            .reserve_tokens
            .saturating_sub(input.max_output_tokens_applied);
        // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-settle:p1:inst-derive-estimated-input
        // The bounded debit `min(reserve_tokens, estimated_input + floor)`: the charged
        // output is the floor, never more than what the reserve still covers (a floor
        // above `max_output_tokens_applied` cannot occur in a persisted turn, so this
        // only guards a corrupt row, e.g. one the orphan watchdog finalizes).
        // @cpt-begin:cpt-cf-mini-chat-algo-background-workers-finalize-orphan:p1:inst-orphan-bounded-debit
        let charged_output = estimated_charged_tokens(
            input.reserve_tokens,
            input.max_output_tokens_applied,
            input.minimal_generation_floor_applied,
        )
        .saturating_sub(estimated_input);
        // @cpt-end:cpt-cf-mini-chat-algo-background-workers-finalize-orphan:p1:inst-orphan-bounded-debit
        return match credits_micro(estimated_input, charged_output, in_mult, out_mult) {
            Ok(credits) => SettlePlan {
                method: SettlementMethod::Estimated,
                committed_credits_micro: credits,
                telemetry_input_tokens: 0,
                telemetry_output_tokens: 0,
                overshoot_capped: false,
                tier,
                fallback: None,
            },
            // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-settle:p1:inst-if-settle-error
            // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-settle:p1:inst-return-settle-error
            Err(error) => fallback(input, tier, FallbackCause::Credit(error)),
            // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-settle:p1:inst-return-settle-error
            // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-settle:p1:inst-if-settle-error
        };
    }
    // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-settle:p1:inst-case-estimated
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-settle:p1:inst-case-actual
    let input_tokens = input.usage.map_or(0, |usage| to_i64(usage.input_tokens));
    let output_tokens = input.usage.map_or(0, |usage| to_i64(usage.output_tokens));
    let actual = match credits_micro(input_tokens, output_tokens, in_mult, out_mult) {
        Ok(credits) => credits,
        // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-settle:p1:inst-if-settle-error
        // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-settle:p1:inst-return-settle-error
        Err(error) => return fallback(input, tier, FallbackCause::Credit(error)),
        // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-settle:p1:inst-return-settle-error
        // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-settle:p1:inst-if-settle-error
    };
    // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-settle:p1:inst-case-actual
    let total_tokens = input_tokens.saturating_add(output_tokens);
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-settle:p1:inst-if-overshoot
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-settle:p1:inst-overshoot-factor
    let capped = total_tokens > input.reserve_tokens
        && overshoot_factor(total_tokens, input.reserve_tokens) > input.overshoot_tolerance_factor;
    // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-settle:p1:inst-overshoot-factor
    // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-settle:p1:inst-if-overshoot
    // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-settle:p1:inst-match-method
    SettlePlan {
        method: SettlementMethod::Actual,
        // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-settle:p1:inst-within-tolerance
        // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-settle:p1:inst-cap-at-reserve
        committed_credits_micro: if capped {
            input.reserved_credits_micro
        } else {
            actual
        },
        // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-settle:p1:inst-cap-at-reserve
        // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-settle:p1:inst-within-tolerance
        telemetry_input_tokens: input_tokens,
        telemetry_output_tokens: output_tokens,
        overshoot_capped: capped,
        tier,
        fallback: None,
    }
}

fn to_i64(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

/// The overshoot factor: actual tokens over the reserve, in floating point
/// (never an integer division).
#[allow(clippy::cast_precision_loss)]
fn overshoot_factor(actual_tokens: i64, reserve_tokens: i64) -> f64 {
    actual_tokens as f64 / reserve_tokens as f64
}

/// The deterministic fallback: `estimated`, the reserved credits, no tokens.
// @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-settle:p1:inst-if-fallback
// @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-settle:p1:inst-fallback-estimated
fn fallback(input: &SettleInput<'_>, tier: Option<ModelTier>, cause: FallbackCause) -> SettlePlan {
    SettlePlan {
        method: SettlementMethod::Estimated,
        committed_credits_micro: input.reserved_credits_micro,
        telemetry_input_tokens: 0,
        telemetry_output_tokens: 0,
        overshoot_capped: false,
        tier,
        fallback: Some(cause),
    }
}
// @cpt-end:cpt-cf-mini-chat-algo-quota-billing-settle:p1:inst-fallback-estimated
// @cpt-end:cpt-cf-mini-chat-algo-quota-billing-settle:p1:inst-if-fallback

/// `min(reserve_tokens, estimated_input_tokens + minimal_generation_floor_applied)`
/// (DESIGN 5.8): the tokens charged by an estimated settlement.
#[must_use]
pub fn estimated_charged_tokens(
    reserve_tokens: i64,
    max_output_tokens_applied: i64,
    minimal_generation_floor_applied: i64,
) -> i64 {
    let estimated_input = reserve_tokens.saturating_sub(max_output_tokens_applied);
    reserve_tokens.min(estimated_input.saturating_add(minimal_generation_floor_applied))
}

/// Completed tool calls of the turn.
#[domain_model]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ToolCallCounts {
    /// Completed web search calls.
    pub web_search: u32,
    /// Completed code interpreter calls.
    pub code_interpreter: u32,
    /// Completed file search calls (reported in the event only).
    pub file_search: u32,
}

/// The change of one `quota_usage` row at settlement.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BucketDelta {
    /// The bucket of the row.
    pub bucket: QuotaBucket,
    /// Added to `reserved_credits_micro` (negative: the turn's reserve is
    /// released).
    pub reserved_credits_micro: i64,
    /// Added to `spent_credits_micro`.
    pub spent_credits_micro: i64,
    /// Added to `calls`.
    pub calls: i32,
    /// Added to `input_tokens`.
    pub input_tokens: i64,
    /// Added to `output_tokens`.
    pub output_tokens: i64,
    /// Added to `web_search_calls`.
    pub web_search_calls: i32,
    /// Added to `code_interpreter_calls`.
    pub code_interpreter_calls: i32,
}

/// The row deltas of one period in write order: `total` always; `tier:premium`
/// only for a premium turn, with credits and `calls` only. Completed web search
/// and code interpreter calls count into `total` on `actual` and `estimated`
/// settlements and are 0 on `released`.
#[must_use]
pub fn bucket_deltas(
    plan: &SettlePlan,
    turn_reserved_credits_micro: i64,
    tool_calls: ToolCallCounts,
    premium: bool,
) -> Vec<BucketDelta> {
    let counts_tools = plan.method != SettlementMethod::Released;
    let tool_count = |count: u32| {
        if counts_tools {
            i32::try_from(count).unwrap_or(i32::MAX)
        } else {
            0
        }
    };
    let mut deltas = vec![BucketDelta {
        bucket: QuotaBucket::Total,
        reserved_credits_micro: -turn_reserved_credits_micro,
        spent_credits_micro: plan.committed_credits_micro,
        calls: 1,
        input_tokens: plan.telemetry_input_tokens,
        output_tokens: plan.telemetry_output_tokens,
        web_search_calls: tool_count(tool_calls.web_search),
        code_interpreter_calls: tool_count(tool_calls.code_interpreter),
    }];
    if premium {
        deltas.push(BucketDelta {
            bucket: QuotaBucket::TierPremium,
            reserved_credits_micro: -turn_reserved_credits_micro,
            spent_credits_micro: plan.committed_credits_micro,
            calls: 1,
            input_tokens: 0,
            output_tokens: 0,
            web_search_calls: 0,
            code_interpreter_calls: 0,
        });
    }
    deltas
}

#[cfg(test)]
#[path = "settle_tests.rs"]
mod tests;
