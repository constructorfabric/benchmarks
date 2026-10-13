//! Input token estimation (DESIGN §5.5.4) and quota period arithmetic (§5.4.2).

use chrono::{DateTime, Datelike, Months, NaiveDate, NaiveTime, Utc};
use mini_chat_sdk::EstimationBudgets;

use super::model::PeriodType;

/// `ceil((ceil(bytes / max(bpt, 1)) + overhead) * (100 + margin) / 100)`.
#[must_use]
pub fn estimated_text_tokens(text: &str, b: &EstimationBudgets) -> i64 {
    let bytes = u64::try_from(text.len()).unwrap_or(u64::MAX);
    let bpt = u64::from(b.bytes_per_token_conservative.max(1));
    let base = bytes
        .div_ceil(bpt)
        .saturating_add(u64::from(b.fixed_overhead_tokens));
    let scaled = base.saturating_mul(100 + u64::from(b.safety_margin_pct));
    i64::try_from(scaled.div_ceil(100)).unwrap_or(i64::MAX)
}

/// Token estimate of one context item (same formula as the user text).
#[must_use]
pub fn item_tokens(text: &str, b: &EstimationBudgets) -> i64 {
    estimated_text_tokens(text, b)
}

/// UTC period start dates `(daily, monthly)` for `now`.
#[must_use]
pub fn period_starts(now: DateTime<Utc>) -> (NaiveDate, NaiveDate) {
    let day = now.date_naive();
    (day, day.with_day(1).unwrap_or(day))
}

/// Start of the next period of `period` after `now`, midnight UTC.
#[must_use]
pub fn next_reset(period: PeriodType, now: DateTime<Utc>) -> DateTime<Utc> {
    let day = now.date_naive();
    let next = match period {
        PeriodType::Daily => day.succ_opt(),
        PeriodType::Monthly => day
            .with_day(1)
            .and_then(|first| first.checked_add_months(Months::new(1))),
    }
    .unwrap_or(day);
    next.and_time(NaiveTime::MIN).and_utc()
}

#[cfg(test)]
#[path = "estimation_tests.rs"]
mod estimation_tests;
