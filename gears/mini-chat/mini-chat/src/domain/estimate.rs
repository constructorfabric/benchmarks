//! Credit arithmetic (DESIGN §5.3), token estimation (§5.5) and quota period
//! helpers (§3.2 "Quota Period Reset Semantics").

use mini_chat_sdk::EstimationBudgets;
use chrono::{DateTime, Datelike, Duration, NaiveDate, Utc};

pub const MAX_TOKENS: i64 = 10_000_000;
pub const MAX_MULT: i64 = 10_000_000_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum CreditError {
    #[error("invalid token count {0}")]
    InvalidTokenCount(i64),
    #[error("zero credit multiplier")]
    ZeroMultiplier,
    #[error("invalid credit multiplier {0}")]
    InvalidMultiplier(i64),
    #[error("credit overflow")]
    Overflow,
}

fn ceil_div(n: i64, d: i64) -> i64 {
    n / d + i64::from(n % d != 0)
}

/// `ceil(in*in_mult/1e6) + ceil(out*out_mult/1e6)`, checked.
///
/// # Errors
/// Token counts above 10M, multipliers outside `1..=10^10` or overflow.
pub fn credits_micro(
    input_tokens: i64,
    output_tokens: i64,
    in_mult: i64,
    out_mult: i64,
) -> Result<i64, CreditError> {
    for t in [input_tokens, output_tokens] {
        if !(0..=MAX_TOKENS).contains(&t) {
            return Err(CreditError::InvalidTokenCount(t));
        }
    }
    for m in [in_mult, out_mult] {
        if m == 0 {
            return Err(CreditError::ZeroMultiplier);
        }
        if !(1..=MAX_MULT).contains(&m) {
            return Err(CreditError::InvalidMultiplier(m));
        }
    }
    let a = input_tokens
        .checked_mul(in_mult)
        .ok_or(CreditError::Overflow)?;
    let b = output_tokens
        .checked_mul(out_mult)
        .ok_or(CreditError::Overflow)?;
    ceil_div(a, 1_000_000)
        .checked_add(ceil_div(b, 1_000_000))
        .ok_or(CreditError::Overflow)
}

/// Token estimate of a text: `ceil((ceil(bytes/bpt) + overhead) * (100+margin)/100)`.
#[must_use]
pub fn estimate_text_tokens(bytes: usize, budgets: &EstimationBudgets) -> i64 {
    let bpt = i64::from(budgets.bytes_per_token_conservative.max(1));
    let bytes = i64::try_from(bytes).unwrap_or(i64::MAX / 4);
    let base = ceil_div(bytes, bpt) + i64::from(budgets.fixed_overhead_tokens);
    let scaled = base.saturating_mul(100 + i64::from(budgets.safety_margin_pct));
    ceil_div(scaled, 100)
}

/// Token estimate of a context item without the fixed overhead (used for
/// recent messages and the summary in context assembly).
#[must_use]
pub fn estimate_item_tokens(bytes: usize, budgets: &EstimationBudgets) -> i64 {
    let bpt = i64::from(budgets.bytes_per_token_conservative.max(1));
    let bytes = i64::try_from(bytes).unwrap_or(i64::MAX / 4);
    let base = ceil_div(bytes, bpt);
    ceil_div(
        base.saturating_mul(100 + i64::from(budgets.safety_margin_pct)),
        100,
    )
}

/// Quota period kinds (P1: daily, monthly).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PeriodType {
    Daily,
    Monthly,
}

impl PeriodType {
    pub const ALL: [PeriodType; 2] = [PeriodType::Daily, PeriodType::Monthly];

    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Daily => "daily",
            Self::Monthly => "monthly",
        }
    }

    /// UTC period start date for an instant.
    #[must_use]
    pub fn start_of(self, at: DateTime<Utc>) -> NaiveDate {
        let d = at.date_naive();
        match self {
            Self::Daily => d,
            Self::Monthly => d.with_day(1).unwrap_or(d),
        }
    }

    /// Next reset instant (midnight UTC tomorrow / the 1st of next month).
    #[must_use]
    pub fn next_reset(self, at: DateTime<Utc>) -> DateTime<Utc> {
        let start = self.start_of(at);
        let next = match self {
            Self::Daily => start + Duration::days(1),
            Self::Monthly => {
                let (y, m) = if start.month() == 12 {
                    (start.year() + 1, 1)
                } else {
                    (start.year(), start.month() + 1)
                };
                NaiveDate::from_ymd_opt(y, m, 1).unwrap_or(start)
            }
        };
        next.and_hms_opt(0, 0, 0)
            .map_or(at, |n| DateTime::<Utc>::from_naive_utc_and_offset(n, Utc))
    }
}

/// Buckets of the quota model.
pub const BUCKET_TOTAL: &str = "total";
pub const BUCKET_PREMIUM: &str = "tier:premium";

/// Remaining percentage, floored, 0..=100.
#[must_use]
pub fn remaining_percentage(limit: i64, used: i64) -> u8 {
    if limit <= 0 {
        return 0;
    }
    let remaining = (limit - used).max(0);
    let pct = remaining.saturating_mul(100) / limit;
    u8::try_from(pct.clamp(0, 100)).unwrap_or(0)
}

/// `(warning, exhausted)` for a remaining percentage.
#[must_use]
pub fn warning_flags(remaining_pct: u8, warning_threshold_pct: u8) -> (bool, bool) {
    let warning = u16::from(remaining_pct) <= 100u16.saturating_sub(u16::from(warning_threshold_pct));
    (warning, remaining_pct == 0)
}

#[cfg(test)]
#[path = "estimate_tests.rs"]
mod estimate_tests;
