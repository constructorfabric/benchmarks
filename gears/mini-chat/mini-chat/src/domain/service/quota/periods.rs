//! Period keys and reset times
//! (`cpt-cf-mini-chat-algo-quota-billing-period-keys`).

use time::{Date, Month, OffsetDateTime, UtcOffset};
use toolkit_macros::domain_model;

use super::types::QuotaPeriod;

/// The `period_start` keys of one instant and the matching reset instants.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PeriodKeys {
    /// UTC calendar date of the instant.
    pub daily_start: Date,
    /// First day of the UTC month of the instant.
    pub monthly_start: Date,
}

impl PeriodKeys {
    /// `period_start` of `period`.
    #[must_use]
    pub const fn start(self, period: QuotaPeriod) -> Date {
        match period {
            QuotaPeriod::Daily => self.daily_start,
            QuotaPeriod::Monthly => self.monthly_start,
        }
    }

    /// Midnight UTC of the day after [`Self::daily_start`].
    #[must_use]
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-period-keys:p1:inst-next-reset
    pub fn daily_reset(self) -> OffsetDateTime {
        let next = self.daily_start.next_day().unwrap_or(self.daily_start);
        next.midnight().assume_utc()
    }
    // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-period-keys:p1:inst-next-reset

    /// Midnight UTC of the first day of the month after [`Self::monthly_start`].
    #[must_use]
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-period-keys:p1:inst-next-reset
    pub fn monthly_reset(self) -> OffsetDateTime {
        let month = self.monthly_start.month();
        let year = if month == Month::December {
            self.monthly_start.year().saturating_add(1)
        } else {
            self.monthly_start.year()
        };
        Date::from_calendar_date(year, month.next(), 1)
            .unwrap_or(self.monthly_start)
            .midnight()
            .assume_utc()
    }
    // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-period-keys:p1:inst-next-reset

    /// The reset instant of `period`.
    #[must_use]
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-period-keys:p1:inst-next-reset
    pub fn next_reset(self, period: QuotaPeriod) -> OffsetDateTime {
        match period {
            QuotaPeriod::Daily => self.daily_reset(),
            QuotaPeriod::Monthly => self.monthly_reset(),
        }
    }
    // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-period-keys:p1:inst-next-reset
}

/// The period keys of `at` (converted to UTC first): daily is the UTC
/// calendar date, monthly the first of the UTC month. No tenant timezone.
#[must_use]
pub fn period_keys(at: OffsetDateTime) -> PeriodKeys {
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-period-keys:p1:inst-daily-start
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-period-keys:p1:inst-utc-only
    let daily_start = at.to_offset(UtcOffset::UTC).date();
    // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-period-keys:p1:inst-utc-only
    // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-period-keys:p1:inst-daily-start
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-period-keys:p1:inst-monthly-start
    let monthly_start = daily_start.replace_day(1).unwrap_or(daily_start);
    // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-period-keys:p1:inst-monthly-start
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-period-keys:p1:inst-return-periods
    PeriodKeys {
        daily_start,
        monthly_start,
    }
    // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-period-keys:p1:inst-return-periods
}

#[cfg(test)]
#[path = "periods_tests.rs"]
mod tests;
