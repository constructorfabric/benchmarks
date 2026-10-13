//! Quota arithmetic (DESIGN sections 3.2 "Quota Period Reset Semantics",
//! 5.3 "Credit Arithmetic" / "Overflow Protection", 5.5.4 "Input Token
//! Estimation").
//!
//! Pure functions only: credit computation with bounds and overflow checks,
//! UTC quota periods and their reset instants, the conservative text-token
//! estimate used by preflight, the quota value sets (periods, buckets), the
//! per-candidate reserve (section 5.4.1), the overshoot rule (section 5.4.5)
//! and the remaining-percentage arithmetic of the quota warnings (section 3.2
//! "Quota Warning Thresholds").

use mini_chat_sdk::{EstimationBudgets, ModelCatalogEntry, ModelTier, TierLimits, UserLimits};
use thiserror::Error;
use time::{Date, Duration, OffsetDateTime, UtcOffset};
use toolkit_macros::domain_model;

/// Upper bound of a token count accepted by [`credits_micro_checked`].
pub const MAX_CREDIT_TOKENS: i64 = 10_000_000;

/// Upper bound of a credit multiplier (micro-credits per 1M tokens); the
/// valid range is `1..=MAX_CREDIT_MULTIPLIER_MICRO`.
pub const MAX_CREDIT_MULTIPLIER_MICRO: i64 = 10_000_000_000;

/// Divisor of the credit formula: multipliers are micro-credits per 1M tokens.
pub const CREDIT_MULTIPLIER_SCALE: i64 = 1_000_000;

/// Why a credit computation was rejected (DESIGN 5.3 "Error handling").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum CreditsError {
    /// A token count is negative or above [`MAX_CREDIT_TOKENS`].
    #[error("token count {tokens} is outside 0..={MAX_CREDIT_TOKENS}")]
    InvalidTokenCount { tokens: i64 },
    /// A multiplier is zero or negative (that usage would be free).
    #[error("credit multiplier {multiplier} must be positive")]
    ZeroMultiplier { multiplier: i64 },
    /// A multiplier is above [`MAX_CREDIT_MULTIPLIER_MICRO`].
    #[error("credit multiplier {multiplier} exceeds {MAX_CREDIT_MULTIPLIER_MICRO}")]
    MultiplierTooLarge { multiplier: i64 },
    /// Checked arithmetic overflowed (unreachable within the bounds above).
    #[error("credit computation overflowed")]
    Overflow,
}

/// Canonical credit formula, rounded up per component:
///
/// `ceil_div(input_tokens * in_mult, 1_000_000) + ceil_div(output_tokens * out_mult, 1_000_000)`
///
/// Both token counts must be in `0..=10_000_000` and both multipliers in
/// `1..=10_000_000_000`; the bounds are checked before multiplying.
///
/// # Errors
/// [`CreditsError::InvalidTokenCount`], [`CreditsError::ZeroMultiplier`] or
/// [`CreditsError::MultiplierTooLarge`] for an out-of-bounds argument;
/// [`CreditsError::Overflow`] if checked arithmetic fails.
pub fn credits_micro_checked(
    input_tokens: i64,
    output_tokens: i64,
    in_mult: i64,
    out_mult: i64,
) -> Result<i64, CreditsError> {
    check_tokens(input_tokens)?;
    check_tokens(output_tokens)?;
    check_multiplier(in_mult)?;
    check_multiplier(out_mult)?;

    let input_credits = component_credits(input_tokens, in_mult)?;
    let output_credits = component_credits(output_tokens, out_mult)?;
    input_credits
        .checked_add(output_credits)
        .ok_or(CreditsError::Overflow)
}

fn check_tokens(tokens: i64) -> Result<(), CreditsError> {
    if (0..=MAX_CREDIT_TOKENS).contains(&tokens) {
        Ok(())
    } else {
        Err(CreditsError::InvalidTokenCount { tokens })
    }
}

fn check_multiplier(multiplier: i64) -> Result<(), CreditsError> {
    if multiplier <= 0 {
        Err(CreditsError::ZeroMultiplier { multiplier })
    } else if multiplier > MAX_CREDIT_MULTIPLIER_MICRO {
        Err(CreditsError::MultiplierTooLarge { multiplier })
    } else {
        Ok(())
    }
}

fn component_credits(tokens: i64, multiplier: i64) -> Result<i64, CreditsError> {
    let product = tokens
        .checked_mul(multiplier)
        .ok_or(CreditsError::Overflow)?;
    ceil_div(product, CREDIT_MULTIPLIER_SCALE).ok_or(CreditsError::Overflow)
}

/// Ceiling division of a non-negative numerator by a positive denominator.
///
/// Returns `None` when the numerator is negative or the denominator is not
/// positive.
#[must_use]
pub fn ceil_div(numerator: i64, denominator: i64) -> Option<i64> {
    if numerator < 0 || denominator <= 0 {
        return None;
    }
    let quotient = numerator.checked_div(denominator)?;
    let remainder = numerator.checked_rem(denominator)?;
    if remainder == 0 {
        Some(quotient)
    } else {
        quotient.checked_add(1)
    }
}

// ── Quota periods (UTC) ──────────────────────────────────────────────────

/// Start of the daily quota period containing `at`: its UTC date.
#[must_use]
pub fn daily_period_start(at: OffsetDateTime) -> Date {
    at.to_offset(UtcOffset::UTC).date()
}

/// Start of the monthly quota period containing `at`: the 1st of its UTC month.
#[must_use]
pub fn monthly_period_start(at: OffsetDateTime) -> Date {
    let day = daily_period_start(at);
    day.saturating_sub(Duration::days(i64::from(day.day()) - 1))
}

/// Next daily reset after `at`: the following midnight UTC.
#[must_use]
pub fn next_daily_reset(at: OffsetDateTime) -> OffsetDateTime {
    daily_period_start(at)
        .saturating_add(Duration::DAY)
        .midnight()
        .assume_utc()
}

/// Next monthly reset after `at`: midnight UTC on the 1st of the next month.
#[must_use]
pub fn next_monthly_reset(at: OffsetDateTime) -> OffsetDateTime {
    let first = monthly_period_start(at);
    let month_days = first.month().length(first.year());
    first
        .saturating_add(Duration::days(i64::from(month_days)))
        .midnight()
        .assume_utc()
}

// ── Token estimation ─────────────────────────────────────────────────────

/// Conservative token estimate of a text of `utf8_bytes` bytes (DESIGN 5.5.4):
///
/// ```text
/// base_estimate = ceil(utf8_bytes / bytes_per_token_conservative) + fixed_overhead_tokens
/// estimated_text_tokens = ceil(base_estimate * (100 + safety_margin_pct) / 100)
/// ```
///
/// A `bytes_per_token_conservative` of 0 is clamped to 1. An empty text yields
/// `fixed_overhead_tokens` before the margin. Saturates at `u64::MAX`.
#[must_use]
pub fn estimate_text_tokens(utf8_bytes: u64, budgets: &EstimationBudgets) -> u64 {
    let bytes_per_token = u64::from(budgets.bytes_per_token_conservative.max(1));
    let base = utf8_bytes
        .div_ceil(bytes_per_token)
        .saturating_add(u64::from(budgets.fixed_overhead_tokens));
    let margin_pct = 100_u128 + u128::from(budgets.safety_margin_pct);
    let estimate = (u128::from(base) * margin_pct).div_ceil(100);
    u64::try_from(estimate).unwrap_or(u64::MAX)
}

// ── Quota value sets ─────────────────────────────────────────────────────

/// `quota_usage.period_type` of the enforced P1 periods.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum QuotaPeriod {
    Daily,
    Monthly,
}

impl QuotaPeriod {
    /// Every enforced period, daily first.
    pub const ALL: [Self; 2] = [Self::Daily, Self::Monthly];

    /// Stored and wire value.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Daily => "daily",
            Self::Monthly => "monthly",
        }
    }

    /// The period stored as `value`; `None` for a value outside the set.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "daily" => Some(Self::Daily),
            "monthly" => Some(Self::Monthly),
            _ => None,
        }
    }

    /// The limit of this period in `limits`.
    #[must_use]
    pub const fn limit(self, limits: &TierLimits) -> i64 {
        match self {
            Self::Daily => limits.limit_daily_credits_micro,
            Self::Monthly => limits.limit_monthly_credits_micro,
        }
    }

    /// The reset instant of the period containing `at`.
    #[must_use]
    pub fn next_reset(self, at: OffsetDateTime) -> OffsetDateTime {
        match self {
            Self::Daily => next_daily_reset(at),
            Self::Monthly => next_monthly_reset(at),
        }
    }
}

/// `quota_usage.bucket` values used for enforcement (section 3.7 "Bucket
/// Semantics"): `total` is the overall cap, `tier:premium` the premium subcap.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum QuotaBucket {
    Total,
    Premium,
}

impl QuotaBucket {
    /// Status order: premium subcap first, then the overall cap.
    pub const ALL: [Self; 2] = [Self::Premium, Self::Total];

    /// Stored value.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Total => "total",
            Self::Premium => "tier:premium",
        }
    }

    /// The bucket stored as `value`; `None` for a value outside the
    /// enforcement set (`tier:standard` is analytics only).
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "total" => Some(Self::Total),
            "tier:premium" => Some(Self::Premium),
            _ => None,
        }
    }

    /// `tier` of the quota status endpoint and of `done.quota_warnings`.
    #[must_use]
    pub const fn status_tier(self) -> &'static str {
        match self {
            Self::Total => "total",
            Self::Premium => "premium",
        }
    }

    /// The limits of this bucket: `total` maps to `standard` (the overall
    /// cap), `tier:premium` to `premium`.
    #[must_use]
    pub const fn limits(self, limits: &UserLimits) -> TierLimits {
        match self {
            Self::Total => limits.standard,
            Self::Premium => limits.premium,
        }
    }

    /// The buckets a turn on `tier` books and is checked against.
    #[must_use]
    pub const fn for_tier(tier: ModelTier) -> &'static [Self] {
        match tier {
            ModelTier::Standard => &[Self::Total],
            ModelTier::Premium => &[Self::Total, Self::Premium],
        }
    }
}

/// The `period_start` dates of one turn, computed once at preflight and used
/// unchanged by every settlement of the turn (section 3.2 "Quota period
/// boundary invariant").
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PeriodStarts {
    pub daily: Date,
    pub monthly: Date,
}

impl PeriodStarts {
    /// The period starts of the periods containing `at` (UTC). The orphan
    /// watchdog passes `chat_turns.started_at`.
    #[must_use]
    pub fn at(at: OffsetDateTime) -> Self {
        Self {
            daily: daily_period_start(at),
            monthly: monthly_period_start(at),
        }
    }

    /// The start of `period`.
    #[must_use]
    pub const fn start(self, period: QuotaPeriod) -> Date {
        match period {
            QuotaPeriod::Daily => self.daily,
            QuotaPeriod::Monthly => self.monthly,
        }
    }
}

// ── Preflight reserve ────────────────────────────────────────────────────

/// What a request adds to the estimate of one candidate model; the tool flags
/// are the gates of that candidate (its `tool_support` and the kill switches).
#[domain_model]
#[allow(
    clippy::struct_excessive_bools,
    reason = "one independent gate per built-in tool, as decided per candidate model"
)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ReserveInputs {
    /// UTF-8 size of the current user message.
    pub message_utf8_bytes: u64,
    /// Images attached to the current message.
    pub image_count: u32,
    /// `input_tokens + output_tokens` of the latest assistant message with usage.
    pub prior_context_tokens: u64,
    /// `file_search` is sent with the candidate.
    pub file_search: bool,
    /// Web search is sent with the candidate.
    pub web_search: bool,
    /// `code_interpreter` is sent with the candidate.
    pub code_interpreter: bool,
}

/// The reserve one candidate model would book (section 5.4.1).
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReserveEstimate {
    pub estimated_input_tokens: i64,
    /// `min(model.max_output_tokens, streaming.max_output_tokens)`.
    pub max_output_tokens_applied: u32,
    /// `estimated_input_tokens + max_output_tokens_applied`.
    pub reserve_tokens: i64,
    /// `credits_micro(estimated_input_tokens, max_output_tokens_applied, in, out)`.
    pub reserved_credits_micro: i64,
}

/// The reserve of `model` for a request (section 5.4.1, canonical formula),
/// with the model's `estimation_budgets` and multipliers; the output cap is
/// `min(model.max_output_tokens, max_output_tokens_cap)`.
///
/// # Errors
///
/// The [`CreditsError`] of the credit computation (an estimate above
/// [`MAX_CREDIT_TOKENS`] is [`CreditsError::InvalidTokenCount`], a bad
/// multiplier its own error); preflight treats the candidate as unavailable.
pub fn estimate_reserve(
    model: &ModelCatalogEntry,
    inputs: &ReserveInputs,
    max_output_tokens_cap: u32,
) -> Result<ReserveEstimate, CreditsError> {
    let budgets = &model.estimation_budgets;
    let surcharge = |enabled: bool, tokens: u32| if enabled { u64::from(tokens) } else { 0 };
    let estimated_input = estimate_text_tokens(inputs.message_utf8_bytes, budgets)
        .saturating_add(inputs.prior_context_tokens)
        .saturating_add(
            u64::from(inputs.image_count).saturating_mul(u64::from(budgets.image_token_budget)),
        )
        .saturating_add(surcharge(inputs.file_search, budgets.tool_surcharge_tokens))
        .saturating_add(surcharge(
            inputs.web_search,
            budgets.web_search_surcharge_tokens,
        ))
        .saturating_add(surcharge(
            inputs.code_interpreter,
            budgets.code_interpreter_surcharge_tokens,
        ));
    let estimated_input_tokens = i64::try_from(estimated_input).unwrap_or(i64::MAX);
    let max_output_tokens_applied = model.max_output_tokens.min(max_output_tokens_cap);
    let reserved_credits_micro = credits_micro_checked(
        estimated_input_tokens,
        i64::from(max_output_tokens_applied),
        model.input_tokens_credit_multiplier_micro,
        model.output_tokens_credit_multiplier_micro,
    )?;
    let reserve_tokens = estimated_input_tokens
        .checked_add(i64::from(max_output_tokens_applied))
        .ok_or(CreditsError::Overflow)?;
    Ok(ReserveEstimate {
        estimated_input_tokens,
        max_output_tokens_applied,
        reserve_tokens,
        reserved_credits_micro,
    })
}

// ── Settlement ───────────────────────────────────────────────────────────

/// Outcome of the overshoot rule on an actual settlement (section 5.4.5).
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Overshoot {
    /// Actual tokens do not exceed the reserve.
    None,
    /// Actual tokens exceed the reserve within the tolerance: actual is
    /// committed.
    WithinTolerance,
    /// Actual tokens exceed the reserve beyond the tolerance: the commit is
    /// capped at the reserve.
    Capped,
}

/// Applies the overshoot rule: `actual_tokens / reserve_tokens` is compared
/// in floating point with `tolerance_factor` (a reserve of 0 with positive
/// actual tokens is beyond any tolerance).
#[must_use]
#[allow(
    clippy::cast_precision_loss,
    reason = "DESIGN 5.4.5 mandates the floating-point ratio; token counts are at most 2e7"
)]
pub fn overshoot(actual_tokens: i64, reserve_tokens: i64, tolerance_factor: f64) -> Overshoot {
    if actual_tokens <= reserve_tokens {
        return Overshoot::None;
    }
    if reserve_tokens <= 0 {
        return Overshoot::Capped;
    }
    let factor = actual_tokens as f64 / reserve_tokens as f64;
    if factor <= tolerance_factor {
        Overshoot::WithinTolerance
    } else {
        Overshoot::Capped
    }
}

// ── Warnings ─────────────────────────────────────────────────────────────

/// Remaining credits and floored remaining percentage of one bucket period.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Remaining {
    /// `limit - used`, clamped at 0.
    pub credits_micro: i64,
    /// `floor(remaining * 100 / limit)`, in `0..=100`.
    pub percentage: u8,
}

/// The remaining share of a period with `limit > 0` after `used` credits
/// (`spent + reserved`); `None` when `limit <= 0` (the period is skipped).
#[must_use]
pub fn remaining(limit: i64, used: i64) -> Option<Remaining> {
    if limit <= 0 {
        return None;
    }
    let credits_micro = i128::from(limit)
        .saturating_sub(i128::from(used.max(0)))
        .clamp(0, i128::from(limit));
    let percentage = (credits_micro * 100)
        .checked_div(i128::from(limit))
        .unwrap_or(0)
        .clamp(0, 100);
    Some(Remaining {
        credits_micro: i64::try_from(credits_micro).unwrap_or(limit),
        percentage: u8::try_from(percentage).unwrap_or(100),
    })
}

#[cfg(test)]
#[path = "quota_tests.rs"]
mod tests;
