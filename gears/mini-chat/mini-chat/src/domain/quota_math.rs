//! Pure credit, estimation and period arithmetic (DESIGN §5.3–§5.5).

use mini_chat_sdk::EstimationBudgets;
use time::{Date, Duration, Month, OffsetDateTime, Time};

pub const MAX_TOKENS: i64 = 10_000_000;
pub const MAX_MULT: i64 = 10_000_000_000;
const MICRO: i64 = 1_000_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum CreditError {
    #[error("invalid token count")]
    InvalidTokenCount,
    #[error("zero multiplier")]
    ZeroMultiplier,
    #[error("multiplier above limit")]
    MultiplierTooLarge,
    #[error("arithmetic overflow")]
    Overflow,
}

#[allow(
    clippy::integer_division,
    reason = "intentional truncating division; the remainder term rounds the result up"
)]
fn ceil_div(n: i64, d: i64) -> i64 {
    n / d + i64::from(n % d != 0)
}

/// Canonical credit formula with per-component ceil rounding and bound checks.
///
/// # Errors
/// Out-of-range tokens/multipliers or overflow.
pub fn credits_micro_checked(
    input_tokens: i64,
    output_tokens: i64,
    in_mult: i64,
    out_mult: i64,
) -> Result<i64, CreditError> {
    for t in [input_tokens, output_tokens] {
        if !(0..=MAX_TOKENS).contains(&t) {
            return Err(CreditError::InvalidTokenCount);
        }
    }
    for m in [in_mult, out_mult] {
        if m <= 0 {
            return Err(CreditError::ZeroMultiplier);
        }
        if m > MAX_MULT {
            return Err(CreditError::MultiplierTooLarge);
        }
    }
    let a = input_tokens
        .checked_mul(in_mult)
        .ok_or(CreditError::Overflow)?;
    let b = output_tokens
        .checked_mul(out_mult)
        .ok_or(CreditError::Overflow)?;
    ceil_div(a, MICRO)
        .checked_add(ceil_div(b, MICRO))
        .ok_or(CreditError::Overflow)
}

/// Multiplier as i64 (saturating; out-of-range values are caught by `credits_micro_checked`).
#[must_use]
pub fn mult(v: u64) -> i64 {
    i64::try_from(v).unwrap_or(i64::MAX)
}

/// `ceil((ceil(bytes / bpt) + fixed) * (100 + margin) / 100)`.
#[must_use]
pub fn estimate_text_tokens(utf8_bytes: usize, b: &EstimationBudgets) -> i64 {
    let bpt = i64::from(b.bytes_per_token_conservative.max(1));
    let bytes = i64::try_from(utf8_bytes).unwrap_or(i64::MAX.div_euclid(4));
    let base = ceil_div(bytes, bpt) + i64::from(b.fixed_overhead_tokens);
    ceil_div(
        base.saturating_mul(100 + i64::from(b.safety_margin_pct)),
        100,
    )
}

/// Token estimate of a context item: `ceil(bytes / bpt)` plus the safety margin (no fixed overhead).
#[must_use]
pub fn estimate_item_tokens(utf8_bytes: usize, b: &EstimationBudgets) -> i64 {
    let bpt = i64::from(b.bytes_per_token_conservative.max(1));
    let bytes = i64::try_from(utf8_bytes).unwrap_or(i64::MAX.div_euclid(4));
    ceil_div(
        ceil_div(bytes, bpt).saturating_mul(100 + i64::from(b.safety_margin_pct)),
        100,
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Period {
    Daily,
    Monthly,
}

impl Period {
    pub const ALL: [Self; 2] = [Self::Daily, Self::Monthly];

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Daily => "daily",
            Self::Monthly => "monthly",
        }
    }

    /// UTC period start date for `now`.
    #[must_use]
    pub fn start(self, now: OffsetDateTime) -> Date {
        let d = now.to_offset(time::UtcOffset::UTC).date();
        match self {
            Self::Daily => d,
            Self::Monthly => d.replace_day(1).unwrap_or(d),
        }
    }

    /// Next reset instant (midnight UTC tomorrow / 1st of next month).
    #[must_use]
    pub fn next_reset(self, now: OffsetDateTime) -> OffsetDateTime {
        let start = self.start(now);
        let next = match self {
            Self::Daily => start.next_day().unwrap_or(start),
            Self::Monthly => {
                let (y, m) = if start.month() == Month::December {
                    (start.year() + 1, Month::January)
                } else {
                    (start.year(), start.month().next())
                };
                Date::from_calendar_date(y, m, 1).unwrap_or(start)
            }
        };
        next.with_time(Time::MIDNIGHT).assume_utc()
    }
}

/// Floored integer remaining percentage (0..=100) of `limit` after `used`.
#[must_use]
pub fn remaining_percentage(limit: i64, used: i64) -> u32 {
    if limit <= 0 {
        return 0;
    }
    let remaining = (limit - used).max(0);
    let pct = (i128::from(remaining) * 100).div_euclid(i128::from(limit));
    u32::try_from(pct.clamp(0, 100)).unwrap_or(0)
}

/// `(warning, exhausted)` for a remaining percentage.
#[must_use]
pub fn warning_flags(remaining_pct: u32, warning_threshold_pct: u8) -> (bool, bool) {
    let warn_at = 100u32.saturating_sub(u32::from(warning_threshold_pct));
    (remaining_pct <= warn_at, remaining_pct == 0)
}

/// Elapsed milliseconds helper.
#[must_use]
pub fn elapsed_ms(d: Duration) -> u64 {
    u64::try_from(d.whole_milliseconds().max(0)).unwrap_or(u64::MAX)
}

#[cfg(test)]
#[path = "quota_math_tests.rs"]
mod tests;
