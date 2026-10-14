//! Period boundaries (`cpt-cf-mini-chat-algo-quota-and-billing-period-boundaries`): UTC daily
//! and monthly `period_start` dates and `next_reset` instants from one captured instant.

use time::{Date, Month, OffsetDateTime, UtcOffset};
use toolkit_macros::domain_model;

/// The two P1 periods (4-hour and weekly windows are deferred to P2+).
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum PeriodType {
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-period-boundaries:p1:inst-period-p1-scope
    // only the `daily` and `monthly` periods of the UTC calendar exist in P1; per-tenant
    // timezones, 4-hour windows and weekly windows are deferred to P2+
    Daily,
    Monthly,
    // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-period-boundaries:p1:inst-period-p1-scope
}

impl PeriodType {
    /// Every period, in the order the rows are read and written: daily, then monthly.
    pub const ALL: [Self; 2] = [Self::Daily, Self::Monthly];

    /// The `quota_usage.period_type` value and the API value: `daily` or `monthly`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Daily => "daily",
            Self::Monthly => "monthly",
        }
    }
}

/// The `period_start` DATE of the daily and monthly rows of one instant. Computed once at
/// preflight, carried with the reserve and used by the settlement; never recomputed from the
/// current clock.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PeriodStarts {
    pub daily: Date,
    pub monthly: Date,
}

impl PeriodStarts {
    /// The UTC calendar date of the instant and the first day of its UTC month. Also the
    /// derivation of a finalizer without the preflight context (the orphan watchdog) from
    /// `chat_turns.started_at`.
    #[must_use]
    pub fn of(instant: OffsetDateTime) -> Self {
        // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-period-boundaries:p1:inst-period-start
        // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-period-boundaries:p1:inst-period-determinism
        // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-period-boundaries:p1:inst-period-from-started-at
        // a pure function of the UTC calendar date of the instant: the same instant (or any
        // instant in the same UTC day) always gives the same rows, also for a finalizer that
        // derives the starts from `chat_turns.started_at`
        let date = instant.to_offset(UtcOffset::UTC).date();
        // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-period-boundaries:p1:inst-period-return
        Self {
            daily: date,
            monthly: date.replace_day(1).unwrap_or(date),
        }
        // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-period-boundaries:p1:inst-period-return
        // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-period-boundaries:p1:inst-period-from-started-at
        // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-period-boundaries:p1:inst-period-determinism
        // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-period-boundaries:p1:inst-period-start
    }

    /// The start of one period.
    #[must_use]
    pub const fn start_of(self, period: PeriodType) -> Date {
        match period {
            PeriodType::Daily => self.daily,
            PeriodType::Monthly => self.monthly,
        }
    }
}

/// The instant the period that contains `instant` resets: 00:00 UTC of the next day for
/// `daily`, 00:00 UTC of the first day of the next month for `monthly`.
#[must_use]
pub fn next_reset(period: PeriodType, instant: OffsetDateTime) -> OffsetDateTime {
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-period-boundaries:p1:inst-period-next-reset
    let today = instant.to_offset(UtcOffset::UTC).date();
    let next = match period {
        PeriodType::Daily => today.next_day(),
        PeriodType::Monthly => {
            let first = today.replace_day(1).unwrap_or(today);
            // the first day of the next month
            let (year, month) = (first.year(), first.month().next());
            let year = if month == Month::January {
                year.saturating_add(1)
            } else {
                year
            };
            Date::from_calendar_date(year, month, 1).ok()
        }
    };
    // the end of the representable range has no next reset; the instant itself is the fallback
    next.map_or(instant, |date| date.midnight().assume_utc())
    // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-period-boundaries:p1:inst-period-next-reset
}

#[cfg(test)]
#[path = "periods_tests.rs"]
mod periods_tests;
