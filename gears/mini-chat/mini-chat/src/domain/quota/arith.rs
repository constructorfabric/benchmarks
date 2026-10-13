//! Pure quota arithmetic: periods, credits, estimation and billing derivation (DESIGN §5.3, §5.5.4, §5.8).

use mini_chat_sdk::{EstimationBudgets, UsageTokens};
use time::{Date, Month, OffsetDateTime, Time};

use super::{BillingOutcome, PeriodStarts, SettlementMethod};

/// Upper bound of a token count accepted by the credit computation.
pub const MAX_TOKENS: i64 = 10_000_000;
/// Upper bound of a credit multiplier.
pub const MAX_MULT: i64 = 10_000_000_000;
const MICRO: i64 = 1_000_000;

pub(super) fn period_starts(now: OffsetDateTime) -> PeriodStarts {
    let utc = now.to_offset(time::UtcOffset::UTC);
    let daily = utc.date();
    let monthly = daily.replace_day(1).unwrap_or(daily);
    PeriodStarts { daily, monthly }
}

/// Next UTC midnight after `now` (daily reset).
#[must_use]
pub fn next_daily_reset(now: OffsetDateTime) -> OffsetDateTime {
    let today = now.to_offset(time::UtcOffset::UTC).date();
    let next = today.next_day().unwrap_or(today);
    next.with_time(Time::MIDNIGHT).assume_utc()
}

/// The 1st of the next month at 00:00 UTC (monthly reset).
#[must_use]
pub fn next_monthly_reset(now: OffsetDateTime) -> OffsetDateTime {
    let today = now.to_offset(time::UtcOffset::UTC).date();
    let (year, month) = if today.month() == Month::December {
        (today.year() + 1, Month::January)
    } else {
        (today.year(), today.month().next())
    };
    let first = Date::from_calendar_date(year, month, 1).unwrap_or(today);
    first.with_time(Time::MIDNIGHT).assume_utc()
}

fn ceil_div_micro(n: i64) -> i64 {
    // n >= 0 here (validated inputs), so plain division + remainder check is exact.
    n.div_euclid(MICRO) + i64::from(n.rem_euclid(MICRO) != 0)
}

pub(super) fn credits_micro(input_tokens: i64, output_tokens: i64, in_mult: i64, out_mult: i64) -> Result<i64, String> {
    if !(0..=MAX_TOKENS).contains(&input_tokens) {
        return Err(format!("invalid input token count {input_tokens} (allowed 0..={MAX_TOKENS})"));
    }
    if !(0..=MAX_TOKENS).contains(&output_tokens) {
        return Err(format!("invalid output token count {output_tokens} (allowed 0..={MAX_TOKENS})"));
    }
    for (name, mult) in [("input", in_mult), ("output", out_mult)] {
        if mult == 0 {
            return Err(format!("zero {name} credit multiplier"));
        }
        if !(1..=MAX_MULT).contains(&mult) {
            return Err(format!("invalid {name} credit multiplier {mult} (allowed 1..={MAX_MULT})"));
        }
    }
    let input_product = input_tokens.checked_mul(in_mult).ok_or_else(|| "input credit multiplication overflow".to_owned())?;
    let output_product =
        output_tokens.checked_mul(out_mult).ok_or_else(|| "output credit multiplication overflow".to_owned())?;
    ceil_div_micro(input_product)
        .checked_add(ceil_div_micro(output_product))
        .ok_or_else(|| "credit addition overflow".to_owned())
}

pub(super) fn estimate_text_tokens(utf8_bytes: usize, budgets: &EstimationBudgets) -> i64 {
    let bpt = u128::from(budgets.bytes_per_token_conservative.max(1));
    let bytes = u64::try_from(utf8_bytes).map_or(u128::MAX, u128::from);
    let text = bytes.div_ceil(bpt);
    let base = text.saturating_add(u128::from(budgets.fixed_overhead_tokens));
    let scaled = base.saturating_mul(100 + u128::from(budgets.safety_margin_pct));
    let est = scaled.div_ceil(100);
    i64::try_from(est).unwrap_or(i64::MAX)
}

/// Codes of failed turns that reached the provider (actual if usage reported, else estimated).
const POST_PROVIDER_CODES: &[&str] = &[
    "provider_error",
    "provider_timeout",
    "rate_limited",
    "web_search_calls_exceeded",
    "code_interpreter_calls_exceeded",
    "agentic_iterations_exceeded",
    "unexpected_tool_use",
    "message_persistence_failed",
];

/// Codes of failed turns that never reached the provider (released).
const PRE_PROVIDER_CODES: &[&str] = &["context_length_exceeded", "validation_error", "input_too_long", "turn_setup_failed"];

pub(super) fn derive_billing(
    state: &str,
    error_code: Option<&str>,
    usage: Option<&UsageTokens>,
) -> (BillingOutcome, SettlementMethod) {
    match state {
        "completed" => (BillingOutcome::Completed, SettlementMethod::Actual),
        "cancelled" => (BillingOutcome::Aborted, SettlementMethod::Estimated),
        _ => {
            let code = error_code.unwrap_or_default();
            if code == "orphan_timeout" {
                return (BillingOutcome::Aborted, SettlementMethod::Estimated);
            }
            if POST_PROVIDER_CODES.contains(&code) {
                let known = usage.is_some_and(|u| u.input_tokens > 0 || u.output_tokens > 0);
                let method = if known { SettlementMethod::Actual } else { SettlementMethod::Estimated };
                return (BillingOutcome::Failed, method);
            }
            if PRE_PROVIDER_CODES.contains(&code) {
                return (BillingOutcome::Failed, SettlementMethod::Released);
            }
            tracing::error!(error_code = code, state, "unknown error code at settlement; settling estimated");
            (BillingOutcome::Failed, SettlementMethod::Estimated)
        }
    }
}
