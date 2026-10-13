//! Credit arithmetic, token estimation and quota periods (DESIGN §5.3–5.5).

use mini_chat_sdk::EstimationBudgets;
use time::{Date, Duration, Month, OffsetDateTime, Time};

pub const MAX_TOKENS: i64 = 10_000_000;
pub const MAX_MULT: i64 = 10_000_000_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum CreditError {
    #[error("token count out of range: {0}")]
    InvalidTokenCount(i64),
    #[error("credit multiplier is zero")]
    ZeroMultiplier,
    #[error("credit multiplier out of range: {0}")]
    InvalidMultiplier(i64),
    #[error("credit computation overflow")]
    Overflow,
}

#[allow(clippy::integer_division)] // truncating division + remainder correction is the point
fn ceil_div(n: i64, d: i64) -> i64 {
    n / d + i64::from(n % d != 0)
}

/// `ceil_div(in*in_mult, 1e6) + ceil_div(out*out_mult, 1e6)` with checked arithmetic.
///
/// # Errors
/// Out-of-range token counts or multipliers, or overflow.
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
    let a = input_tokens.checked_mul(in_mult).ok_or(CreditError::Overflow)?;
    let b = output_tokens.checked_mul(out_mult).ok_or(CreditError::Overflow)?;
    ceil_div(a, 1_000_000)
        .checked_add(ceil_div(b, 1_000_000))
        .ok_or(CreditError::Overflow)
}

/// `ceil((ceil(bytes / bpt) + fixed_overhead) * (100 + margin) / 100)`.
#[must_use]
pub fn estimate_text_tokens(utf8_bytes: usize, budgets: &EstimationBudgets) -> i64 {
    let bpt = i64::from(budgets.bytes_per_token_conservative.max(1));
    let bytes = i64::try_from(utf8_bytes).unwrap_or(i64::MAX.div_euclid(4));
    let base = ceil_div(bytes, bpt) + i64::from(budgets.fixed_overhead_tokens);
    ceil_div(base * (100 + i64::from(budgets.safety_margin_pct)), 100)
}

/// Quota period type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Period {
    Daily,
    Monthly,
}

impl Period {
    pub const ALL: [Self; 2] = [Self::Daily, Self::Monthly];

    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Daily => "daily",
            Self::Monthly => "monthly",
        }
    }

    /// UTC calendar start of the period containing `at`.
    #[must_use]
    pub fn start(self, at: OffsetDateTime) -> Date {
        let utc = at.to_offset(time::UtcOffset::UTC).date();
        match self {
            Self::Daily => utc,
            Self::Monthly => Date::from_calendar_date(utc.year(), utc.month(), 1).unwrap_or(utc),
        }
    }

    /// Start (midnight UTC) of the next period after `at`.
    #[must_use]
    pub fn next_reset(self, at: OffsetDateTime) -> OffsetDateTime {
        let start = self.start(at);
        let next = match self {
            Self::Daily => start + Duration::days(1),
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

/// Floored remaining percentage of a limit (0..=100).
#[must_use]
pub fn remaining_percentage(limit: i64, used: i64) -> u32 {
    if limit <= 0 {
        return 0;
    }
    let remaining = (limit - used).max(0);
    let pct = (i128::from(remaining) * 100).div_euclid(i128::from(limit));
    u32::try_from(pct.clamp(0, 100)).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use time::macros::datetime;

    use super::*;

    #[test]
    fn credits_per_component_ceil() {
        // 1000 tokens * 2.5e9 / 1e6 = 2_500_000 ; 500 * 2.5e9 / 1e6 = 1_250_000
        assert_eq!(credits_micro(1000, 500, 2_500_000_000, 2_500_000_000), Ok(3_750_000));
        // ceil per component: 1 * 1 / 1e6 -> 1 each
        assert_eq!(credits_micro(1, 1, 1, 1), Ok(2));
        assert_eq!(credits_micro(0, 0, 1, 1), Ok(0));
        // DESIGN 5.10 example
        assert_eq!(credits_micro(900, 300, 1_000_000_000, 1_000_000_000), Ok(1_200_000));
    }

    #[test]
    fn credits_validation() {
        assert_eq!(credits_micro(1, 1, 0, 1), Err(CreditError::ZeroMultiplier));
        assert_eq!(credits_micro(-1, 1, 1, 1), Err(CreditError::InvalidTokenCount(-1)));
        assert_eq!(credits_micro(MAX_TOKENS + 1, 0, 1, 1), Err(CreditError::InvalidTokenCount(MAX_TOKENS + 1)));
        assert_eq!(credits_micro(1, 1, MAX_MULT + 1, 1), Err(CreditError::InvalidMultiplier(MAX_MULT + 1)));
    }

    #[test]
    fn text_estimation() {
        let b = EstimationBudgets::default(); // 4 bpt, 100 overhead, 10%
        assert_eq!(estimate_text_tokens(0, &b), 110);
        assert_eq!(estimate_text_tokens(400, &b), 220);
        assert_eq!(estimate_text_tokens(1, &b), 112); // ceil(101*1.1)=112
    }

    #[test]
    fn periods_are_utc_calendar() {
        let t = datetime!(2026-02-28 23:59:59 UTC);
        assert_eq!(Period::Daily.start(t), time::macros::date!(2026 - 02 - 28));
        assert_eq!(Period::Monthly.start(t), time::macros::date!(2026 - 02 - 01));
        assert_eq!(Period::Daily.next_reset(t), datetime!(2026-03-01 00:00:00 UTC));
        assert_eq!(Period::Monthly.next_reset(datetime!(2026-12-05 10:00 UTC)), datetime!(2027-01-01 00:00 UTC));
    }

    #[test]
    fn remaining_pct_floors() {
        assert_eq!(remaining_percentage(100, 0), 100);
        assert_eq!(remaining_percentage(1000, 995), 0);
        assert_eq!(remaining_percentage(1000, 200), 80);
        assert_eq!(remaining_percentage(1000, 2000), 0);
    }
}
