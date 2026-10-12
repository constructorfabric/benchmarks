use mini_chat_sdk::{EstimationBudgets, ModelCatalogEntry, ModelTier, TierLimits, UserLimits};
use time::macros::{date, datetime};
use uuid::Uuid;

use super::{
    CreditsError, MAX_CREDIT_MULTIPLIER_MICRO, MAX_CREDIT_TOKENS, Overshoot, PeriodStarts,
    QuotaBucket, QuotaPeriod, Remaining, ReserveInputs, ceil_div, credits_micro_checked,
    daily_period_start, estimate_reserve, estimate_text_tokens, monthly_period_start,
    next_daily_reset, next_monthly_reset, overshoot, remaining,
};
use crate::domain::models_service::tests::catalog_entry;

fn budgets(bytes_per_token: u32, overhead: u32, margin_pct: u32) -> EstimationBudgets {
    EstimationBudgets {
        bytes_per_token_conservative: bytes_per_token,
        fixed_overhead_tokens: overhead,
        safety_margin_pct: margin_pct,
        ..EstimationBudgets::default()
    }
}

// ── ceil_div ─────────────────────────────────────────────────────────────

#[test]
fn ceil_div_rounds_up_only_with_remainder() {
    assert_eq!(ceil_div(0, 7), Some(0));
    assert_eq!(ceil_div(7, 7), Some(1));
    assert_eq!(ceil_div(8, 7), Some(2));
    assert_eq!(ceil_div(1, 1_000_000), Some(1));
}

#[test]
fn ceil_div_rejects_negative_numerator_and_non_positive_denominator() {
    assert_eq!(ceil_div(-1, 7), None);
    assert_eq!(ceil_div(7, 0), None);
    assert_eq!(ceil_div(7, -1), None);
}

#[test]
fn ceil_div_handles_max_numerator() {
    assert_eq!(ceil_div(i64::MAX, 1), Some(i64::MAX));
    // i64::MAX is odd: ceil(i64::MAX / 2) = 2^62.
    assert_eq!(ceil_div(i64::MAX, 2), Some(4_611_686_018_427_387_904));
}

// ── credits ──────────────────────────────────────────────────────────────

#[test]
fn credits_error_carries_the_offending_value() {
    assert_eq!(
        credits_micro_checked(MAX_CREDIT_TOKENS + 1, 0, 1, 1),
        Err(CreditsError::InvalidTokenCount {
            tokens: MAX_CREDIT_TOKENS + 1
        })
    );
    assert_eq!(
        credits_micro_checked(0, 0, 1, 0),
        Err(CreditsError::ZeroMultiplier { multiplier: 0 })
    );
    assert_eq!(
        credits_micro_checked(0, 0, MAX_CREDIT_MULTIPLIER_MICRO + 1, 1),
        Err(CreditsError::MultiplierTooLarge {
            multiplier: MAX_CREDIT_MULTIPLIER_MICRO + 1
        })
    );
}

#[test]
fn credits_token_bounds_are_checked_before_multipliers() {
    assert!(matches!(
        credits_micro_checked(-1, 0, 0, 0),
        Err(CreditsError::InvalidTokenCount { .. })
    ));
}

// ── periods ──────────────────────────────────────────────────────────────

#[test]
fn daily_period_start_is_the_utc_date() {
    assert_eq!(
        daily_period_start(datetime!(2026-03-15 23:59:59 UTC)),
        date!(2026 - 03 - 15)
    );
    // 01:30 at +03:00 is 22:30 UTC on the previous day.
    assert_eq!(
        daily_period_start(datetime!(2026-03-16 01:30 +3)),
        date!(2026 - 03 - 15)
    );
}

#[test]
fn monthly_period_start_is_the_first_of_the_utc_month() {
    assert_eq!(
        monthly_period_start(datetime!(2026-03-31 12:00 UTC)),
        date!(2026 - 03 - 01)
    );
    assert_eq!(
        monthly_period_start(datetime!(2026-03-01 00:00 UTC)),
        date!(2026 - 03 - 01)
    );
    // 2026-04-01 00:30 at +01:00 is still March in UTC.
    assert_eq!(
        monthly_period_start(datetime!(2026-04-01 00:30 +1)),
        date!(2026 - 03 - 01)
    );
}

#[test]
fn next_daily_reset_is_the_following_midnight_utc() {
    assert_eq!(
        next_daily_reset(datetime!(2026-03-15 23:59:59 UTC)),
        datetime!(2026-03-16 00:00 UTC)
    );
    assert_eq!(
        next_daily_reset(datetime!(2026-03-15 00:00 UTC)),
        datetime!(2026-03-16 00:00 UTC)
    );
    assert_eq!(
        next_daily_reset(datetime!(2026-12-31 08:00 UTC)),
        datetime!(2027-01-01 00:00 UTC)
    );
}

#[test]
fn next_monthly_reset_is_the_first_of_next_month_utc() {
    assert_eq!(
        next_monthly_reset(datetime!(2026-01-31 10:00 UTC)),
        datetime!(2026-02-01 00:00 UTC)
    );
    assert_eq!(
        next_monthly_reset(datetime!(2026-12-15 10:00 UTC)),
        datetime!(2027-01-01 00:00 UTC)
    );
    // Leap February.
    assert_eq!(
        next_monthly_reset(datetime!(2028-02-29 23:00 UTC)),
        datetime!(2028-03-01 00:00 UTC)
    );
}

#[test]
fn reset_is_always_after_the_instant() {
    let at = datetime!(2026-05-31 23:59:59.999 UTC);
    assert!(next_daily_reset(at) > at);
    assert!(next_monthly_reset(at) > at);
    assert_eq!(next_monthly_reset(at), datetime!(2026-06-01 00:00 UTC));
}

// ── token estimation ─────────────────────────────────────────────────────

#[test]
fn estimate_empty_text_is_overhead_with_margin() {
    // ceil(0 / 4) + 100 = 100; ceil(100 * 110 / 100) = 110.
    assert_eq!(estimate_text_tokens(0, &budgets(4, 100, 10)), 110);
}

#[test]
fn estimate_rounds_bytes_and_margin_up() {
    // ceil(10 / 4) = 3; 3 + 100 = 103; ceil(103 * 110 / 100) = ceil(113.3) = 114.
    assert_eq!(estimate_text_tokens(10, &budgets(4, 100, 10)), 114);
    // ceil(1 / 3) = 1; 1 + 0 = 1; ceil(1 * 120 / 100) = 2.
    assert_eq!(estimate_text_tokens(1, &budgets(3, 0, 20)), 2);
}

#[test]
fn estimate_without_margin_is_exact() {
    assert_eq!(estimate_text_tokens(400, &budgets(4, 0, 0)), 100);
}

#[test]
fn estimate_clamps_zero_bytes_per_token_to_one() {
    assert_eq!(estimate_text_tokens(50, &budgets(0, 0, 0)), 50);
}

#[test]
fn estimate_with_default_budgets() {
    // Defaults: 4 bytes/token, 100 overhead, 10 %.
    // ceil(4000 / 4) = 1000; 1100 * 1.1 = 1210.
    assert_eq!(
        estimate_text_tokens(4000, &EstimationBudgets::default()),
        1210
    );
}

#[test]
fn estimate_saturates_instead_of_overflowing() {
    assert_eq!(
        estimate_text_tokens(u64::MAX, &budgets(1, u32::MAX, u32::MAX)),
        u64::MAX
    );
}

// ── Value sets ───────────────────────────────────────────────────────────

#[test]
fn periods_and_buckets_round_trip_their_stored_values() {
    for period in QuotaPeriod::ALL {
        assert_eq!(QuotaPeriod::parse(period.as_str()), Some(period));
    }
    for bucket in QuotaBucket::ALL {
        assert_eq!(QuotaBucket::parse(bucket.as_str()), Some(bucket));
    }
    assert_eq!(QuotaPeriod::parse("weekly"), None);
    assert_eq!(QuotaBucket::parse("tier:standard"), None);
    assert_eq!(QuotaBucket::Premium.as_str(), "tier:premium");
    assert_eq!(QuotaBucket::Premium.status_tier(), "premium");
    assert_eq!(QuotaBucket::Total.status_tier(), "total");
}

#[test]
fn buckets_map_to_their_limits_and_tiers() {
    let limits = UserLimits {
        user_id: Uuid::nil(),
        policy_version: 1,
        standard: TierLimits {
            limit_daily_credits_micro: 10,
            limit_monthly_credits_micro: 100,
        },
        premium: TierLimits {
            limit_daily_credits_micro: 3,
            limit_monthly_credits_micro: 30,
        },
    };
    assert_eq!(
        QuotaPeriod::Daily.limit(&QuotaBucket::Total.limits(&limits)),
        10
    );
    assert_eq!(
        QuotaPeriod::Monthly.limit(&QuotaBucket::Total.limits(&limits)),
        100
    );
    assert_eq!(
        QuotaPeriod::Daily.limit(&QuotaBucket::Premium.limits(&limits)),
        3
    );
    assert_eq!(
        QuotaPeriod::Monthly.limit(&QuotaBucket::Premium.limits(&limits)),
        30
    );
    assert_eq!(
        QuotaBucket::for_tier(ModelTier::Standard),
        &[QuotaBucket::Total]
    );
    assert_eq!(
        QuotaBucket::for_tier(ModelTier::Premium),
        &[QuotaBucket::Total, QuotaBucket::Premium]
    );
}

#[test]
fn period_starts_are_utc_dates_at_period_boundaries() {
    let before = PeriodStarts::at(datetime!(2026-02-28 23:59:59 UTC));
    assert_eq!(before.daily, date!(2026 - 02 - 28));
    assert_eq!(before.monthly, date!(2026 - 02 - 01));
    let after = PeriodStarts::at(datetime!(2026-03-01 00:00:01 UTC));
    assert_eq!(after.start(QuotaPeriod::Daily), date!(2026 - 03 - 01));
    assert_eq!(after.start(QuotaPeriod::Monthly), date!(2026 - 03 - 01));
    let offset = PeriodStarts::at(datetime!(2026-03-01 01:00 +02:00));
    assert_eq!(offset.daily, date!(2026 - 02 - 28));
    assert_eq!(
        QuotaPeriod::Daily.next_reset(datetime!(2026-02-28 23:59:59 UTC)),
        datetime!(2026-03-01 00:00 UTC)
    );
    assert_eq!(
        QuotaPeriod::Monthly.next_reset(datetime!(2026-02-28 23:59:59 UTC)),
        datetime!(2026-03-01 00:00 UTC)
    );
}

// ── Reserve ──────────────────────────────────────────────────────────────

fn model(extra: serde_json::Value) -> ModelCatalogEntry {
    catalog_entry("m", extra)
}

#[test]
fn reserve_applies_the_canonical_formula_with_every_surcharge() {
    // Defaults: 4 bytes/token, 100 overhead, 10 %, image 1000, tool 500,
    // web 500, code interpreter 1000; multipliers 1x in / 3x out per token.
    let entry = model(serde_json::json!({}));
    let inputs = ReserveInputs {
        message_utf8_bytes: 400,
        image_count: 2,
        prior_context_tokens: 1000,
        file_search: true,
        web_search: true,
        code_interpreter: true,
    };
    let reserve = estimate_reserve(&entry, &inputs, 32_768).unwrap();
    // text: (100 + 100) * 1.1 = 220; + 1000 + 2 * 1000 + 500 + 500 + 1000.
    assert_eq!(reserve.estimated_input_tokens, 5220);
    assert_eq!(reserve.max_output_tokens_applied, 4096);
    assert_eq!(reserve.reserve_tokens, 5220 + 4096);
    assert_eq!(reserve.reserved_credits_micro, 5220 + 4096 * 3);
}

#[test]
fn reserve_omits_surcharges_of_tools_not_sent() {
    let entry = model(serde_json::json!({}));
    let inputs = ReserveInputs {
        message_utf8_bytes: 400,
        ..ReserveInputs::default()
    };
    let reserve = estimate_reserve(&entry, &inputs, 32_768).unwrap();
    assert_eq!(reserve.estimated_input_tokens, 220);
}

#[test]
fn reserve_uses_the_candidate_budgets_and_the_output_cap() {
    let entry = model(serde_json::json!({
        "max_output_tokens": 8000,
        "estimation_budgets": { "bytes_per_token_conservative": 2, "fixed_overhead_tokens": 0, "safety_margin_pct": 0 },
        "input_tokens_credit_multiplier_micro": 500_000,
    }));
    let reserve = estimate_reserve(
        &entry,
        &ReserveInputs {
            message_utf8_bytes: 9,
            ..ReserveInputs::default()
        },
        1000,
    )
    .unwrap();
    assert_eq!(reserve.estimated_input_tokens, 5);
    assert_eq!(reserve.max_output_tokens_applied, 1000);
    // ceil(5 * 0.5) = 3; 1000 * 3 = 3000.
    assert_eq!(reserve.reserved_credits_micro, 3003);
}

#[test]
fn reserve_errors_on_bad_multipliers_and_oversized_estimates() {
    let zero = model(serde_json::json!({ "output_tokens_credit_multiplier_micro": 0 }));
    assert_eq!(
        estimate_reserve(&zero, &ReserveInputs::default(), 32_768),
        Err(CreditsError::ZeroMultiplier { multiplier: 0 })
    );
    let entry = model(serde_json::json!({}));
    let huge = ReserveInputs {
        prior_context_tokens: u64::MAX,
        ..ReserveInputs::default()
    };
    assert!(matches!(
        estimate_reserve(&entry, &huge, 32_768),
        Err(CreditsError::InvalidTokenCount { .. })
    ));
}

// ── Overshoot ────────────────────────────────────────────────────────────

#[test]
fn overshoot_compares_the_float_ratio_with_the_tolerance() {
    assert_eq!(overshoot(1000, 1000, 1.10), Overshoot::None);
    assert_eq!(overshoot(900, 1000, 1.10), Overshoot::None);
    assert_eq!(overshoot(1100, 1000, 1.10), Overshoot::WithinTolerance);
    assert_eq!(overshoot(1101, 1000, 1.10), Overshoot::Capped);
    // Integer division would truncate 1.15 to 1.
    assert_eq!(overshoot(11_500, 10_000, 1.10), Overshoot::Capped);
    assert_eq!(overshoot(1001, 1000, 1.0), Overshoot::Capped);
    assert_eq!(overshoot(1, 0, 1.5), Overshoot::Capped);
}

// ── Remaining ────────────────────────────────────────────────────────────

#[test]
fn remaining_skips_non_positive_limits() {
    assert_eq!(remaining(0, 0), None);
    assert_eq!(remaining(-5, 0), None);
}

#[test]
fn remaining_percentage_is_floored_and_clamped() {
    assert_eq!(
        remaining(1000, 0),
        Some(Remaining {
            credits_micro: 1000,
            percentage: 100
        })
    );
    assert_eq!(
        remaining(1000, 801),
        Some(Remaining {
            credits_micro: 199,
            percentage: 19
        })
    );
    assert_eq!(
        remaining(1000, 991),
        Some(Remaining {
            credits_micro: 9,
            percentage: 0
        })
    );
    assert_eq!(
        remaining(1000, 5000),
        Some(Remaining {
            credits_micro: 0,
            percentage: 0
        })
    );
    assert_eq!(
        remaining(i64::MAX, i64::MAX - 1),
        Some(Remaining {
            credits_micro: 1,
            percentage: 0
        })
    );
}
