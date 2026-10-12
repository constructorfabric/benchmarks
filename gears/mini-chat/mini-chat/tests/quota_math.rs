//! Pure quota arithmetic and decisions (`mini_chat::domain::quota`): credit formula and its bounds,
//! the preflight reserve estimate, UTC period math, bucket / tier availability, the downgrade
//! cascade, warning thresholds and the status builder, the billing outcome derivation table and
//! the settlement formulas (actual with the overshoot cap, estimated).
//!
//! Source of truth: DESIGN 5.3 (credit arithmetic, "Overflow Protection"), 5.3.1 (glossary),
//! 5.4.1 - 5.4.5, 5.5.4 / 5.5.6 / 5.5.14, 5.8 "Normative Billing Outcome Derivation", 5.9,
//! 5.10 (calculation example, used as a test vector), 3.2 "Quota Period Reset Semantics" /
//! "Quota Warning Thresholds", 4 "Downgrade Decision Flow".

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::doc_markdown,
    clippy::too_many_lines,
    clippy::similar_names,
    clippy::use_debug,
    clippy::unreadable_literal,
    clippy::needless_pass_by_value,
    clippy::float_cmp
)]

mod common;

use mini_chat::domain::quota::{
    Bucket, BucketUsage, CascadeInput, CreditsError, DowngradeReason, MAX_MULTIPLIER_MICRO,
    MAX_TOKENS, ModelDecision, Period, QuotaExhausted, QuotaTier, QuotaUsage, QuotaWarning,
    Reserve, ReserveFacts, TurnReserve, bucket_available, credits_micro, derive_billing,
    estimate_reserve, minimal_generation_floor_applied, next_reset, period_start, quota_status,
    quota_warnings, resolve_effective_model, settle_actual, settle_estimated, tier_available,
};
use mini_chat::domain::turns::TurnState;
use mini_chat_sdk::{
    BillingOutcome, KillSwitches, ModelCatalogEntry, ModelTier, PolicySnapshot, SettlementMethod,
    TerminalState, TierLimits, UserLimits,
};
use serde_json::{Value, json};
use time::format_description::well_known::Rfc3339;
use time::{Date, Month, OffsetDateTime};
use uuid::Uuid;

// ── builders ─────────────────────────────────────────────────────────────────

fn at(rfc3339: &str) -> OffsetDateTime {
    OffsetDateTime::parse(rfc3339, &Rfc3339).unwrap()
}

fn date(y: i32, m: Month, d: u8) -> Date {
    Date::from_calendar_date(y, m, d).unwrap()
}

/// A catalog entry with explicit multipliers and the default estimation budgets
/// (4 bytes / token, overhead 100, margin 10 %, image 1000, tool 500, web 500, code 1000).
fn entry(id: &str, tier: &str, enabled: bool, in_mult: u64, out_mult: u64) -> ModelCatalogEntry {
    let mut value = common::catalog_entry_json(id, tier, enabled);
    value["input_tokens_credit_multiplier_micro"] = json!(in_mult);
    value["output_tokens_credit_multiplier_micro"] = json!(out_mult);
    value["max_output_tokens"] = json!(4096);
    value["estimation_budgets"] = json!({
        "bytes_per_token_conservative": 4, "fixed_overhead_tokens": 100, "safety_margin_pct": 10,
        "image_token_budget": 1000, "tool_surcharge_tokens": 500,
        "web_search_surcharge_tokens": 500, "code_interpreter_surcharge_tokens": 1000,
        "minimal_generation_floor": 50
    });
    serde_json::from_value(value).expect("catalog entry")
}

fn with_default(mut e: ModelCatalogEntry, is_default: bool) -> ModelCatalogEntry {
    e.preference = Some(mini_chat_sdk::ModelPreference {
        is_default,
        sort_order: 0,
    });
    e
}

fn switches(patch: Value) -> KillSwitches {
    let mut all = serde_json::to_value(common::kill_switches_off()).unwrap();
    for (k, v) in patch.as_object().unwrap() {
        all[k] = v.clone();
    }
    serde_json::from_value(all).unwrap()
}

fn limits(sd: u64, sm: u64, pd: u64, pm: u64) -> UserLimits {
    UserLimits {
        user_id: Uuid::nil(),
        policy_version: 1,
        standard: TierLimits {
            limit_daily_credits_micro: sd,
            limit_monthly_credits_micro: sm,
        },
        premium: TierLimits {
            limit_daily_credits_micro: pd,
            limit_monthly_credits_micro: pm,
        },
    }
}

fn snapshot(models: Vec<ModelCatalogEntry>, kill: KillSwitches) -> PolicySnapshot {
    PolicySnapshot {
        policy_version: 1,
        model_catalog: models,
        kill_switches: kill,
    }
}

fn usage(rows: &[(Bucket, Period, i64, i64)]) -> QuotaUsage {
    rows.iter()
        .fold(QuotaUsage::new(), |u, (b, p, spent, reserved)| {
            u.with(*b, *p, *spent, *reserved)
        })
}

fn facts(message: &str) -> ReserveFacts<'_> {
    ReserveFacts {
        message,
        prior_context_tokens: 0,
        image_count: 0,
        has_ready_documents: false,
        web_search_requested: false,
        has_code_interpreter_files: false,
    }
}

// ═════════════════════════════════════════════════════════════════════════════
// credits_micro (DESIGN 5.3)
// ═════════════════════════════════════════════════════════════════════════════

#[test]
fn credits_follow_the_design_5_10_vectors() {
    // premium reserve, standard reserve, standard commit by actual
    assert_eq!(
        credits_micro(1_000, 500, 2_500_000_000, 2_500_000_000),
        Ok(3_750_000)
    );
    assert_eq!(
        credits_micro(1_000, 500, 1_000_000_000, 1_000_000_000),
        Ok(1_500_000)
    );
    assert_eq!(
        credits_micro(900, 300, 1_000_000_000, 1_000_000_000),
        Ok(1_200_000)
    );
}

#[test]
fn credits_round_up_per_component_not_on_the_sum() {
    // ceil(1 * 500_000 / 1e6) + ceil(1 * 500_000 / 1e6) = 1 + 1; rounding the sum would give 1
    assert_eq!(credits_micro(1, 1, 500_000, 500_000), Ok(2));
    assert_eq!(credits_micro(1, 0, 1, 1), Ok(1));
    assert_eq!(credits_micro(0, 1, 1, 1), Ok(1));
    assert_eq!(credits_micro(3, 3, 333_333, 333_333), Ok(2));
}

#[test]
fn zero_tokens_cost_nothing() {
    assert_eq!(credits_micro(0, 0, 1_000_000, 3_000_000), Ok(0));
}

#[test]
fn credits_of_the_catalog_multipliers_used_by_the_tests() {
    // 113 input + 4096 output tokens on the premium (3 / 15) and standard (1 / 3) catalog models
    assert_eq!(credits_micro(113, 4096, 3_000_000, 15_000_000), Ok(61_779));
    assert_eq!(credits_micro(113, 4096, 1_000_000, 3_000_000), Ok(12_401));
}

#[test]
fn the_documented_bounds_are_accepted_exactly() {
    assert_eq!(MAX_TOKENS, 10_000_000);
    assert_eq!(MAX_MULTIPLIER_MICRO, 10_000_000_000);
    assert_eq!(
        credits_micro(
            MAX_TOKENS,
            MAX_TOKENS,
            MAX_MULTIPLIER_MICRO,
            MAX_MULTIPLIER_MICRO
        ),
        Ok(200_000_000_000)
    );
    assert_eq!(credits_micro(0, 0, 1, 1), Ok(0));
}

#[test]
fn token_counts_above_ten_million_are_rejected() {
    assert_eq!(
        credits_micro(MAX_TOKENS + 1, 0, 1, 1),
        Err(CreditsError::TokenCountOutOfRange)
    );
    assert_eq!(
        credits_micro(0, MAX_TOKENS + 1, 1, 1),
        Err(CreditsError::TokenCountOutOfRange)
    );
}

#[test]
fn a_zero_multiplier_and_an_oversized_multiplier_are_separate_errors() {
    assert_eq!(
        credits_micro(10, 10, 0, 1),
        Err(CreditsError::ZeroMultiplier)
    );
    assert_eq!(
        credits_micro(10, 10, 1, 0),
        Err(CreditsError::ZeroMultiplier)
    );
    assert_eq!(
        credits_micro(10, 10, MAX_MULTIPLIER_MICRO + 1, 1),
        Err(CreditsError::MultiplierOutOfRange)
    );
    assert_eq!(
        credits_micro(10, 10, 1, MAX_MULTIPLIER_MICRO + 1),
        Err(CreditsError::MultiplierOutOfRange)
    );
}

// ═════════════════════════════════════════════════════════════════════════════
// estimate_reserve (DESIGN 5.4.1, 5.5.4, 5.5.6)
// ═════════════════════════════════════════════════════════════════════════════

#[test]
fn a_plain_message_reserves_text_plus_the_output_cap() {
    let model = entry("p", "Premium", true, 3_000_000, 15_000_000);
    let reserve = estimate_reserve(
        &model,
        &common::kill_switches_off(),
        32_768,
        &facts("hello"),
    );
    // ceil((ceil(5 / 4) + 100) * 110 / 100) = ceil(112.2) = 113
    assert_eq!(
        reserve,
        Reserve {
            estimated_input_tokens: 113,
            max_output_tokens_applied: 4096,
            reserve_tokens: 4209,
            reserved_credits_micro: 61_779,
        }
    );
}

#[test]
fn the_output_cap_is_the_smaller_of_the_model_and_the_streaming_limit() {
    let model = entry("p", "Premium", true, 1_000_000, 1_000_000);
    let reserve = estimate_reserve(&model, &common::kill_switches_off(), 1_000, &facts("hello"));
    assert_eq!(reserve.max_output_tokens_applied, 1_000);
    assert_eq!(reserve.reserve_tokens, 113 + 1_000);
    assert_eq!(reserve.reserved_credits_micro, 113 + 1_000);
}

#[test]
fn the_estimate_counts_utf8_bytes_and_an_empty_message_costs_the_overhead() {
    let model = entry("p", "Premium", true, 1_000_000, 1_000_000);
    let kill = common::kill_switches_off();
    let empty = estimate_reserve(&model, &kill, 100, &facts(""));
    assert_eq!(empty.estimated_input_tokens, 110, "ceil(100 * 110 / 100)");
    let accents = "\u{e9}".repeat(10); // 20 bytes
    let reserve = estimate_reserve(&model, &kill, 100, &facts(&accents));
    // ceil((ceil(20 / 4) + 100) * 110 / 100) = ceil(115.5) = 116
    assert_eq!(reserve.estimated_input_tokens, 116);
}

#[test]
fn prior_context_tokens_stand_in_for_the_history() {
    let model = entry("p", "Premium", true, 1_000_000, 1_000_000);
    let mut f = facts("hello");
    f.prior_context_tokens = 887;
    let reserve = estimate_reserve(&model, &common::kill_switches_off(), 100, &f);
    assert_eq!(reserve.estimated_input_tokens, 113 + 887);
    assert_eq!(reserve.reserve_tokens, 1_000 + 100);
}

#[test]
fn every_image_adds_the_image_budget() {
    let model = entry("p", "Premium", true, 1_000_000, 1_000_000);
    let mut f = facts("hello");
    f.image_count = 2;
    let reserve = estimate_reserve(&model, &common::kill_switches_off(), 100, &f);
    assert_eq!(reserve.estimated_input_tokens, 113 + 2 * 1_000);
}

#[test]
fn the_tool_surcharges_apply_once_and_only_when_the_tool_is_sent() {
    let kill = common::kill_switches_off();
    let model = entry("p", "Premium", true, 1_000_000, 1_000_000);

    let mut f = facts("hello");
    f.has_ready_documents = true;
    assert_eq!(
        estimate_reserve(&model, &kill, 100, &f).estimated_input_tokens,
        113 + 500,
        "file_search surcharge"
    );
    let mut f = facts("hello");
    f.web_search_requested = true;
    assert_eq!(
        estimate_reserve(&model, &kill, 100, &f).estimated_input_tokens,
        113 + 500,
        "web_search surcharge"
    );
    let mut f = facts("hello");
    f.has_code_interpreter_files = true;
    assert_eq!(
        estimate_reserve(&model, &kill, 100, &f).estimated_input_tokens,
        113 + 1000,
        "code_interpreter surcharge"
    );
    let mut f = facts("hello");
    f.has_ready_documents = true;
    f.web_search_requested = true;
    f.has_code_interpreter_files = true;
    f.image_count = 1;
    assert_eq!(
        estimate_reserve(&model, &kill, 100, &f).estimated_input_tokens,
        113 + 500 + 500 + 1000 + 1000
    );
}

#[test]
fn a_surcharge_is_zero_when_the_candidate_does_not_support_the_tool() {
    let kill = common::kill_switches_off();
    let mut model = entry("s", "Standard", true, 1_000_000, 1_000_000);
    model.general_config.tool_support.web_search = false;
    model.general_config.tool_support.file_search = false;
    model.general_config.tool_support.code_interpreter = false;
    let mut f = facts("hello");
    f.web_search_requested = true;
    f.has_ready_documents = true;
    f.has_code_interpreter_files = true;
    assert_eq!(
        estimate_reserve(&model, &kill, 100, &f).estimated_input_tokens,
        113,
        "DESIGN 5.5.6: no tool_support, no surcharge"
    );
}

#[test]
fn the_file_search_and_code_interpreter_kill_switches_remove_their_surcharge() {
    let model = entry("p", "Premium", true, 1_000_000, 1_000_000);
    let mut f = facts("hello");
    f.has_ready_documents = true;
    f.has_code_interpreter_files = true;
    let kill = switches(json!({"disable_file_search": true, "disable_code_interpreter": true}));
    assert_eq!(
        estimate_reserve(&model, &kill, 100, &f).estimated_input_tokens,
        113
    );
}

#[test]
fn a_zero_multiplier_or_an_overflowing_estimate_books_i64_max() {
    let kill = common::kill_switches_off();
    let mut zero = entry("p", "Premium", true, 1_000_000, 1_000_000);
    zero.input_tokens_credit_multiplier_micro = 0;
    let reserve = estimate_reserve(&zero, &kill, 100, &facts("hello"));
    assert_eq!(reserve.reserved_credits_micro, i64::MAX);
    assert_eq!(reserve.reserve_tokens, 113 + 100, "tokens stay exact");

    let big = entry("p", "Premium", true, 1_000_000, 1_000_000);
    let mut f = facts("hello");
    f.prior_context_tokens = MAX_TOKENS; // estimated input goes above the 10M credit bound
    assert_eq!(
        estimate_reserve(&big, &kill, 100, &f).reserved_credits_micro,
        i64::MAX
    );
}

#[test]
fn a_zero_bytes_per_token_budget_is_clamped_to_one() {
    let mut model = entry("p", "Premium", true, 1_000_000, 1_000_000);
    model.estimation_budgets.bytes_per_token_conservative = 0;
    let reserve = estimate_reserve(&model, &common::kill_switches_off(), 100, &facts("abcd"));
    // ceil((4 + 100) * 110 / 100) = ceil(114.4) = 115
    assert_eq!(reserve.estimated_input_tokens, 115);
}

// ═════════════════════════════════════════════════════════════════════════════
// periods (DESIGN 3.2 "Quota Period Reset Semantics", 5.4.2 conversion algorithm)
// ═════════════════════════════════════════════════════════════════════════════

#[test]
fn period_names_and_order() {
    assert_eq!(Period::Daily.as_str(), "daily");
    assert_eq!(Period::Monthly.as_str(), "monthly");
    assert_eq!(Period::ALL, [Period::Daily, Period::Monthly]);
    assert_eq!(Bucket::Total.as_str(), "total");
    assert_eq!(Bucket::PremiumTier.as_str(), "tier:premium");
    assert_eq!(QuotaTier::Premium.as_str(), "premium");
    assert_eq!(QuotaTier::Total.as_str(), "total");
}

#[test]
fn period_starts_follow_the_design_examples() {
    let t = at("2026-02-28T15:30:00Z");
    assert_eq!(
        period_start(Period::Daily, t),
        date(2026, Month::February, 28)
    );
    assert_eq!(
        period_start(Period::Monthly, t),
        date(2026, Month::February, 1)
    );
    let late = at("2026-02-28T23:59:59Z");
    assert_eq!(
        period_start(Period::Daily, late),
        date(2026, Month::February, 28)
    );
    let midnight = at("2026-03-01T00:00:00Z");
    assert_eq!(
        period_start(Period::Daily, midnight),
        date(2026, Month::March, 1)
    );
    assert_eq!(
        period_start(Period::Monthly, midnight),
        date(2026, Month::March, 1)
    );
}

#[test]
fn period_starts_are_computed_in_utc_whatever_the_offset() {
    // 2026-03-01T02:00 at +05:30 is 2026-02-28T20:30Z
    let t = at("2026-03-01T02:00:00+05:30");
    assert_eq!(
        period_start(Period::Daily, t),
        date(2026, Month::February, 28)
    );
    assert_eq!(
        period_start(Period::Monthly, t),
        date(2026, Month::February, 1)
    );
}

#[test]
fn next_reset_is_the_next_utc_midnight_or_the_first_of_next_month() {
    assert_eq!(
        next_reset(Period::Daily, at("2026-02-28T15:30:00Z")),
        at("2026-03-01T00:00:00Z")
    );
    assert_eq!(
        next_reset(Period::Monthly, at("2026-02-28T15:30:00Z")),
        at("2026-03-01T00:00:00Z")
    );
    assert_eq!(
        next_reset(Period::Monthly, at("2026-12-15T08:00:00Z")),
        at("2027-01-01T00:00:00Z"),
        "year rollover"
    );
    assert_eq!(
        next_reset(Period::Daily, at("2026-03-01T00:00:00Z")),
        at("2026-03-02T00:00:00Z"),
        "exactly at the boundary the next period is a full period away"
    );
    assert_eq!(
        next_reset(Period::Daily, at("2028-02-29T10:00:00Z")),
        at("2028-03-01T00:00:00Z"),
        "leap day"
    );
}

// ═════════════════════════════════════════════════════════════════════════════
// availability (DESIGN 5.4.2)
// ═════════════════════════════════════════════════════════════════════════════

fn bu(spent: i64, reserved: i64) -> BucketUsage {
    BucketUsage {
        spent_credits_micro: spent,
        reserved_credits_micro: reserved,
    }
}

#[test]
fn a_bucket_is_available_while_spent_reserved_and_this_reserve_fit_the_limit() {
    assert!(
        bucket_available(bu(60, 30), 10, 100),
        "exactly at the limit"
    );
    assert!(
        !bucket_available(bu(60, 30), 11, 100),
        "one micro-credit over"
    );
    assert!(bucket_available(bu(0, 0), 100, 100));
    assert!(
        !bucket_available(bu(0, 0), 1, 0),
        "a zero limit admits nothing"
    );
    assert!(
        !bucket_available(bu(0, 0), i64::MAX, 1_000_000_000_000),
        "the i64::MAX reserve of an overflowing candidate never fits"
    );
    assert!(
        !bucket_available(bu(i64::MAX, 0), 1, i64::MAX),
        "no wrap-around"
    );
}

#[test]
fn standard_availability_reads_only_the_total_bucket() {
    let l = limits(1_000, 10_000, 100, 1_000);
    let u = usage(&[
        (Bucket::PremiumTier, Period::Daily, 100, 0), // premium sub-cap exhausted
    ]);
    assert!(tier_available(ModelTier::Standard, &u, &l, 500));
    assert!(!tier_available(ModelTier::Premium, &u, &l, 1));
}

#[test]
fn premium_availability_needs_both_buckets_in_both_periods() {
    let l = limits(1_000, 10_000, 500, 5_000);
    for blocked in [
        (Bucket::Total, Period::Daily, 900, 0),
        (Bucket::Total, Period::Monthly, 9_900, 0),
        (Bucket::PremiumTier, Period::Daily, 450, 0),
        (Bucket::PremiumTier, Period::Monthly, 4_950, 0),
    ] {
        let u = usage(&[blocked]);
        assert!(
            !tier_available(ModelTier::Premium, &u, &l, 101),
            "{blocked:?} must block the premium tier"
        );
    }
    assert!(tier_available(ModelTier::Premium, &usage(&[]), &l, 101));
}

#[test]
fn reserves_of_other_requests_count_as_used() {
    let l = limits(1_000, 10_000, 1_000, 10_000);
    let u = usage(&[(Bucket::Total, Period::Daily, 400, 500)]);
    assert!(tier_available(ModelTier::Standard, &u, &l, 100));
    assert!(!tier_available(ModelTier::Standard, &u, &l, 101));
}

// ═════════════════════════════════════════════════════════════════════════════
// downgrade cascade (DESIGN 4 "Downgrade Decision Flow", 5.4.2, 5.10)
// ═════════════════════════════════════════════════════════════════════════════

const PREMIUM: &str = "p1";
const STANDARD: &str = "s1";

fn catalog() -> Vec<ModelCatalogEntry> {
    vec![
        entry(PREMIUM, "Premium", true, 2_500_000_000, 2_500_000_000),
        entry(STANDARD, "Standard", true, 1_000_000_000, 1_000_000_000),
    ]
}

/// The 5.10 reserves: 3_750_000 for the premium candidate, 1_500_000 for the standard one.
fn reserve_5_10(candidate: &ModelCatalogEntry) -> Reserve {
    let credits = if candidate.tier == ModelTier::Premium {
        3_750_000
    } else {
        1_500_000
    };
    Reserve {
        estimated_input_tokens: 1_000,
        max_output_tokens_applied: 500,
        reserve_tokens: 1_500,
        reserved_credits_micro: credits,
    }
}

fn decide(
    selected: &str,
    snap: &PolicySnapshot,
    u: &QuotaUsage,
    l: &UserLimits,
) -> Result<ModelDecision, QuotaExhausted> {
    resolve_effective_model(&CascadeInput {
        selected_model: selected,
        snapshot: snap,
        usage: u,
        limits: l,
        reserve_of: &reserve_5_10,
    })
}

/// 5.10.1: premium day 20M / month 200M of 22M / 300M, total day 25M / month 240M of 60M / 600M.
fn example_5_10() -> (QuotaUsage, UserLimits) {
    (
        usage(&[
            (Bucket::PremiumTier, Period::Daily, 20_000_000, 0),
            (Bucket::PremiumTier, Period::Monthly, 200_000_000, 0),
            (Bucket::Total, Period::Daily, 25_000_000, 0),
            (Bucket::Total, Period::Monthly, 240_000_000, 0),
        ]),
        limits(60_000_000, 600_000_000, 22_000_000, 300_000_000),
    )
}

#[test]
fn the_design_5_10_example_downgrades_premium_to_standard() {
    let (u, l) = example_5_10();
    let decision = decide(
        PREMIUM,
        &snapshot(catalog(), common::kill_switches_off()),
        &u,
        &l,
    )
    .expect("standard is available");
    assert_eq!(decision.model_id, STANDARD);
    assert_eq!(decision.tier, ModelTier::Standard);
    assert_eq!(
        decision.downgrade_reason,
        Some(DowngradeReason::PremiumQuotaExhausted)
    );
    assert_eq!(
        decision.reserve.reserved_credits_micro, 1_500_000,
        "the booked reserve is the one of the effective model"
    );
    assert_eq!(decision.reserve.reserve_tokens, 1_500);
}

#[test]
fn an_available_premium_selection_is_allowed_unchanged() {
    let (mut u, l) = example_5_10();
    u = u.with(Bucket::PremiumTier, Period::Daily, 10_000_000, 0);
    let decision = decide(
        PREMIUM,
        &snapshot(catalog(), common::kill_switches_off()),
        &u,
        &l,
    )
    .expect("premium fits");
    assert_eq!(decision.model_id, PREMIUM);
    assert_eq!(decision.tier, ModelTier::Premium);
    assert_eq!(decision.downgrade_reason, None);
    assert_eq!(decision.reserve.reserved_credits_micro, 3_750_000);
}

#[test]
fn any_exhausted_premium_subcap_period_downgrades() {
    let snap = snapshot(catalog(), common::kill_switches_off());
    for period in Period::ALL {
        // the premium sub-cap of one period has 1_000_000 left (< the 3_750_000 premium reserve)
        let l = limits(1_000_000_000, 10_000_000_000, 5_000_000, 50_000_000);
        let spent = if period == Period::Daily {
            4_000_000
        } else {
            49_000_000
        };
        let u = usage(&[(Bucket::PremiumTier, period, spent, 0)]);
        let d = decide(PREMIUM, &snap, &u, &l).expect("standard still fits");
        assert_eq!(d.model_id, STANDARD, "{period:?}");
        assert_eq!(
            d.downgrade_reason,
            Some(DowngradeReason::PremiumQuotaExhausted),
            "{period:?}"
        );
    }
}

#[test]
fn a_total_bucket_that_fits_standard_but_not_premium_downgrades() {
    let snap = snapshot(catalog(), common::kill_switches_off());
    for period in Period::ALL {
        // 2_000_000 left in the shared cap: premium (3_750_000) does not fit, standard (1_500_000) does
        let l = limits(10_000_000, 100_000_000, 1_000_000_000, 10_000_000_000);
        let spent = if period == Period::Daily {
            8_000_000
        } else {
            98_000_000
        };
        let u = usage(&[(Bucket::Total, period, spent, 0)]);
        let d = decide(PREMIUM, &snap, &u, &l).expect("standard fits");
        assert_eq!(d.model_id, STANDARD, "{period:?}");
        assert_eq!(
            d.downgrade_reason,
            Some(DowngradeReason::PremiumQuotaExhausted),
            "{period:?}"
        );
    }
}

#[test]
fn all_tiers_exhausted_is_quota_exhausted() {
    let l = limits(1_000_000, 10_000_000, 500_000, 5_000_000);
    let u = usage(&[(Bucket::Total, Period::Daily, 1_000_000, 0)]);
    let snap = snapshot(catalog(), common::kill_switches_off());
    assert_eq!(decide(PREMIUM, &snap, &u, &l).unwrap_err(), QuotaExhausted);
    assert_eq!(decide(STANDARD, &snap, &u, &l).unwrap_err(), QuotaExhausted);
}

#[test]
fn a_standard_selection_never_cascades_up_to_premium() {
    let l = limits(10_000_000, 10_000_000, 500_000_000, 5_000_000_000);
    let snap = snapshot(catalog(), common::kill_switches_off());
    let u = usage(&[(Bucket::Total, Period::Monthly, 9_999_000, 0)]);
    // total monthly cannot take 1_500_000; premium is irrelevant for a standard selection
    assert_eq!(decide(STANDARD, &snap, &u, &l).unwrap_err(), QuotaExhausted);
    let ok = decide(STANDARD, &snap, &usage(&[]), &l).expect("fits");
    assert_eq!(ok.model_id, STANDARD);
    assert_eq!(ok.downgrade_reason, None);
}

#[test]
fn a_standard_selection_ignores_the_premium_kill_switches() {
    let l = limits(1_000_000_000, 10_000_000_000, 500_000_000, 5_000_000_000);
    for patch in [
        json!({"force_standard_tier": true}),
        json!({"disable_premium_tier": true}),
    ] {
        let snap = snapshot(catalog(), switches(patch));
        let d = decide(STANDARD, &snap, &usage(&[]), &l).expect("standard");
        assert_eq!(d.model_id, STANDARD);
        assert_eq!(d.downgrade_reason, None);
    }
}

#[test]
fn the_premium_kill_switches_skip_the_premium_tier_with_their_reason() {
    let l = limits(1_000_000_000, 10_000_000_000, 500_000_000, 5_000_000_000);
    for (patch, reason) in [
        (
            json!({"force_standard_tier": true}),
            DowngradeReason::ForceStandardTier,
        ),
        (
            json!({"disable_premium_tier": true}),
            DowngradeReason::DisablePremiumTier,
        ),
    ] {
        let snap = snapshot(catalog(), switches(patch));
        let d = decide(PREMIUM, &snap, &usage(&[]), &l).expect("standard");
        assert_eq!(d.model_id, STANDARD);
        assert_eq!(d.downgrade_reason, Some(reason));
        assert_eq!(d.reserve.reserved_credits_micro, 1_500_000);
    }
}

#[test]
fn both_premium_kill_switches_report_one_of_them() {
    let l = limits(1_000_000_000, 10_000_000_000, 500_000_000, 5_000_000_000);
    let snap = snapshot(
        catalog(),
        switches(json!({"force_standard_tier": true, "disable_premium_tier": true})),
    );
    let d = decide(PREMIUM, &snap, &usage(&[]), &l).expect("standard");
    assert!(matches!(
        d.downgrade_reason,
        Some(DowngradeReason::ForceStandardTier | DowngradeReason::DisablePremiumTier)
    ));
}

#[test]
fn a_kill_switch_with_an_exhausted_standard_tier_is_still_a_reject() {
    let l = limits(1_000_000, 10_000_000, 500_000_000, 5_000_000_000);
    let u = usage(&[(Bucket::Total, Period::Daily, 1_000_000, 0)]);
    let snap = snapshot(catalog(), switches(json!({"force_standard_tier": true})));
    assert_eq!(decide(PREMIUM, &snap, &u, &l).unwrap_err(), QuotaExhausted);
}

#[test]
fn a_disabled_selected_model_downgrades_with_model_disabled() {
    let l = limits(1_000_000_000, 10_000_000_000, 500_000_000, 5_000_000_000);
    let models = vec![
        entry("p-off", "Premium", false, 2_500_000_000, 2_500_000_000),
        entry(STANDARD, "Standard", true, 1_000_000_000, 1_000_000_000),
    ];
    let snap = snapshot(models, common::kill_switches_off());
    // the only premium model is disabled: the tier has no candidate, standard serves
    let d = decide("p-off", &snap, &usage(&[]), &l).expect("standard");
    assert_eq!(d.model_id, STANDARD);
    assert_eq!(d.downgrade_reason, Some(DowngradeReason::ModelDisabled));
}

#[test]
fn a_disabled_selection_falls_to_another_enabled_model_of_its_own_tier_first() {
    let l = limits(1_000_000_000, 10_000_000_000, 500_000_000, 5_000_000_000);
    let models = vec![
        entry("p-off", "Premium", false, 2_500_000_000, 2_500_000_000),
        entry("p-two", "Premium", true, 2_500_000_000, 2_500_000_000),
        entry(STANDARD, "Standard", true, 1_000_000_000, 1_000_000_000),
    ];
    let snap = snapshot(models, common::kill_switches_off());
    let d = decide("p-off", &snap, &usage(&[]), &l).expect("premium sibling");
    assert_eq!(d.model_id, "p-two");
    assert_eq!(d.tier, ModelTier::Premium);
    assert_eq!(d.downgrade_reason, Some(DowngradeReason::ModelDisabled));
}

#[test]
fn a_model_missing_from_the_catalog_starts_at_premium_with_model_disabled() {
    let l = limits(1_000_000_000, 10_000_000_000, 500_000_000, 5_000_000_000);
    let snap = snapshot(catalog(), common::kill_switches_off());
    let d = decide("gone", &snap, &usage(&[]), &l).expect("premium default");
    assert_eq!(d.model_id, PREMIUM);
    assert_eq!(d.downgrade_reason, Some(DowngradeReason::ModelDisabled));
}

#[test]
fn the_first_reason_wins() {
    let l = limits(1_000_000_000, 10_000_000_000, 500_000_000, 5_000_000_000);
    // disabled premium selection + force_standard_tier: model_disabled was set first
    let models = vec![
        entry("p-off", "Premium", false, 2_500_000_000, 2_500_000_000),
        entry("p-two", "Premium", true, 2_500_000_000, 2_500_000_000),
        entry(STANDARD, "Standard", true, 1_000_000_000, 1_000_000_000),
    ];
    let snap = snapshot(models, switches(json!({"force_standard_tier": true})));
    let d = decide("p-off", &snap, &usage(&[]), &l).expect("standard");
    assert_eq!(d.model_id, STANDARD);
    assert_eq!(d.downgrade_reason, Some(DowngradeReason::ModelDisabled));

    // missing model + exhausted premium sub-cap: still model_disabled
    let snap = snapshot(catalog(), common::kill_switches_off());
    let u = usage(&[(Bucket::PremiumTier, Period::Daily, 500_000_000, 0)]);
    let d = decide("gone", &snap, &u, &l).expect("standard");
    assert_eq!(d.model_id, STANDARD);
    assert_eq!(d.downgrade_reason, Some(DowngradeReason::ModelDisabled));
}

#[test]
fn a_candidate_is_the_tenant_default_then_the_first_enabled_model_of_the_tier() {
    let l = limits(1_000_000_000, 10_000_000_000, 500_000_000, 5_000_000_000);
    let exhausted_premium = usage(&[(Bucket::PremiumTier, Period::Daily, 500_000_000, 0)]);

    let first = vec![
        entry(PREMIUM, "Premium", true, 2_500_000_000, 2_500_000_000),
        with_default(entry("s-a", "Standard", true, 1, 1), false),
        with_default(entry("s-b", "Standard", true, 1, 1), false),
    ];
    let d = decide(
        PREMIUM,
        &snapshot(first, common::kill_switches_off()),
        &exhausted_premium,
        &l,
    )
    .unwrap();
    assert_eq!(d.model_id, "s-a", "catalog order without a default");

    let preferred = vec![
        entry(PREMIUM, "Premium", true, 2_500_000_000, 2_500_000_000),
        with_default(entry("s-a", "Standard", true, 1, 1), false),
        with_default(entry("s-b", "Standard", true, 1, 1), true),
    ];
    let d = decide(
        PREMIUM,
        &snapshot(preferred, common::kill_switches_off()),
        &exhausted_premium,
        &l,
    )
    .unwrap();
    assert_eq!(
        d.model_id, "s-b",
        "the tenant preference wins over catalog order"
    );

    let disabled_default = vec![
        entry(PREMIUM, "Premium", true, 2_500_000_000, 2_500_000_000),
        with_default(entry("s-a", "Standard", true, 1, 1), false),
        with_default(entry("s-b", "Standard", false, 1, 1), true),
    ];
    let d = decide(
        PREMIUM,
        &snapshot(disabled_default, common::kill_switches_off()),
        &exhausted_premium,
        &l,
    )
    .unwrap();
    assert_eq!(d.model_id, "s-a", "disabled models are never candidates");
}

#[test]
fn a_standard_selection_stays_on_itself_when_enabled() {
    let l = limits(1_000_000_000, 10_000_000_000, 500_000_000, 5_000_000_000);
    let models = vec![
        with_default(entry("s-a", "Standard", true, 1, 1), true),
        with_default(entry("s-b", "Standard", true, 1, 1), false),
    ];
    let d = decide(
        "s-b",
        &snapshot(models, common::kill_switches_off()),
        &usage(&[]),
        &l,
    )
    .unwrap();
    assert_eq!(d.model_id, "s-b");
    assert_eq!(d.downgrade_reason, None);
}

#[test]
fn no_enabled_model_in_any_tier_of_the_cascade_is_a_reject() {
    let l = limits(1_000_000_000, 10_000_000_000, 500_000_000, 5_000_000_000);
    let models = vec![
        entry(PREMIUM, "Premium", false, 1, 1),
        entry(STANDARD, "Standard", false, 1, 1),
    ];
    let snap = snapshot(models, common::kill_switches_off());
    assert_eq!(
        decide(PREMIUM, &snap, &usage(&[]), &l).unwrap_err(),
        QuotaExhausted
    );
}

#[test]
fn each_candidate_is_checked_with_its_own_reserve() {
    // the premium reserve (3_750_000) does not fit a 3_000_000 sub-cap, the standard one does
    let l = limits(60_000_000, 600_000_000, 3_000_000, 30_000_000);
    let snap = snapshot(catalog(), common::kill_switches_off());
    let d = decide(PREMIUM, &snap, &usage(&[]), &l).unwrap();
    assert_eq!(d.model_id, STANDARD);
    assert_eq!(d.reserve.reserved_credits_micro, 1_500_000);
}

#[test]
fn an_unusable_reserve_makes_the_candidate_unavailable() {
    let l = limits(60_000_000, 600_000_000, 22_000_000, 300_000_000);
    let snap = snapshot(catalog(), common::kill_switches_off());
    let overflowing_premium = |candidate: &ModelCatalogEntry| Reserve {
        reserved_credits_micro: if candidate.tier == ModelTier::Premium {
            i64::MAX
        } else {
            1_500_000
        },
        ..reserve_5_10(candidate)
    };
    let d = resolve_effective_model(&CascadeInput {
        selected_model: PREMIUM,
        snapshot: &snap,
        usage: &usage(&[]),
        limits: &l,
        reserve_of: &overflowing_premium,
    })
    .expect("standard");
    assert_eq!(d.model_id, STANDARD, "overflow downgrades, it is not a 500");

    let overflowing_all = |candidate: &ModelCatalogEntry| Reserve {
        reserved_credits_micro: i64::MAX,
        ..reserve_5_10(candidate)
    };
    let err = resolve_effective_model(&CascadeInput {
        selected_model: PREMIUM,
        snapshot: &snap,
        usage: &usage(&[]),
        limits: &l,
        reserve_of: &overflowing_all,
    })
    .unwrap_err();
    assert_eq!(err, QuotaExhausted);
}

#[test]
fn downgrade_reason_wire_names() {
    assert_eq!(
        DowngradeReason::PremiumQuotaExhausted.as_str(),
        "premium_quota_exhausted"
    );
    assert_eq!(
        DowngradeReason::ForceStandardTier.as_str(),
        "force_standard_tier"
    );
    assert_eq!(
        DowngradeReason::DisablePremiumTier.as_str(),
        "disable_premium_tier"
    );
    assert_eq!(DowngradeReason::ModelDisabled.as_str(), "model_disabled");
}

// ═════════════════════════════════════════════════════════════════════════════
// quota status and warnings (DESIGN 3.2 "Quota Warning Thresholds", "Quota Status Endpoint")
// ═════════════════════════════════════════════════════════════════════════════

fn period_status(
    status: &[mini_chat::domain::quota::TierStatus],
    tier: QuotaTier,
    period: Period,
) -> Option<mini_chat::domain::quota::PeriodStatus> {
    status
        .iter()
        .find(|t| t.tier == tier)?
        .periods
        .iter()
        .find(|p| p.period == period)
        .cloned()
}

#[test]
fn status_reports_limit_used_remaining_and_percentage_per_period() {
    let l = limits(1_000, 10_000, 500, 5_000);
    let u = usage(&[
        (Bucket::Total, Period::Daily, 200, 50),
        (Bucket::PremiumTier, Period::Monthly, 1_000, 0),
    ]);
    let now = at("2026-02-28T15:30:00Z");
    let status = quota_status(&l, &u, 80, now);

    let total_daily = period_status(&status, QuotaTier::Total, Period::Daily).unwrap();
    assert_eq!(total_daily.limit_credits_micro, 1_000);
    assert_eq!(total_daily.used_credits_micro, 250, "spent + reserved");
    assert_eq!(total_daily.remaining_credits_micro, 750);
    assert_eq!(total_daily.remaining_percentage, 75);
    assert!(!total_daily.warning && !total_daily.exhausted);
    assert_eq!(total_daily.next_reset, at("2026-03-01T00:00:00Z"));

    let premium_monthly = period_status(&status, QuotaTier::Premium, Period::Monthly).unwrap();
    assert_eq!(premium_monthly.limit_credits_micro, 5_000);
    assert_eq!(premium_monthly.used_credits_micro, 1_000);
    assert_eq!(premium_monthly.remaining_percentage, 80);
    assert_eq!(premium_monthly.next_reset, at("2026-03-01T00:00:00Z"));

    // a bucket without a row counts as zero
    let premium_daily = period_status(&status, QuotaTier::Premium, Period::Daily).unwrap();
    assert_eq!(premium_daily.used_credits_micro, 0);
    assert_eq!(premium_daily.remaining_percentage, 100);
}

#[test]
fn a_period_inside_the_warning_band_warns_and_the_edge_is_inclusive() {
    // threshold 80 %: warning when remaining_percentage <= 20
    let l = limits(100, 100, 100, 100);
    let now = at("2026-02-28T15:30:00Z");
    let at_edge = quota_status(
        &l,
        &usage(&[(Bucket::Total, Period::Daily, 80, 0)]),
        80,
        now,
    );
    let p = period_status(&at_edge, QuotaTier::Total, Period::Daily).unwrap();
    assert_eq!(p.remaining_percentage, 20);
    assert!(p.warning, "20 <= 100 - 80");
    assert!(!p.exhausted);

    let before = quota_status(
        &l,
        &usage(&[(Bucket::Total, Period::Daily, 79, 0)]),
        80,
        now,
    );
    let p = period_status(&before, QuotaTier::Total, Period::Daily).unwrap();
    assert_eq!(p.remaining_percentage, 21);
    assert!(!p.warning);
}

#[test]
fn the_threshold_is_configurable() {
    let l = limits(100, 100, 100, 100);
    let now = at("2026-02-28T15:30:00Z");
    let u = usage(&[(Bucket::Total, Period::Daily, 60, 0)]);
    let tight = quota_status(&l, &u, 50, now);
    assert!(
        period_status(&tight, QuotaTier::Total, Period::Daily)
            .unwrap()
            .warning
    );
    let loose = quota_status(&l, &u, 90, now);
    assert!(
        !period_status(&loose, QuotaTier::Total, Period::Daily)
            .unwrap()
            .warning
    );
}

#[test]
fn a_fully_used_period_is_exhausted_and_never_negative() {
    let l = limits(100, 100, 100, 100);
    let now = at("2026-02-28T15:30:00Z");
    let u = usage(&[(Bucket::Total, Period::Daily, 130, 20)]); // overshoot beyond the limit
    let status = quota_status(&l, &u, 80, now);
    let p = period_status(&status, QuotaTier::Total, Period::Daily).unwrap();
    assert_eq!(p.used_credits_micro, 150);
    assert_eq!(p.remaining_credits_micro, 0, "remaining is clamped at zero");
    assert_eq!(p.remaining_percentage, 0);
    assert!(p.exhausted && p.warning);
}

#[test]
fn less_than_one_percent_left_counts_as_exhausted() {
    // 5 of 1000 left = 0.5 % -> floored to 0 -> exhausted
    let l = limits(1_000, 1_000, 1_000, 1_000);
    let now = at("2026-02-28T15:30:00Z");
    let u = usage(&[(Bucket::Total, Period::Daily, 995, 0)]);
    let status = quota_status(&l, &u, 80, now);
    let p = period_status(&status, QuotaTier::Total, Period::Daily).unwrap();
    assert_eq!(p.remaining_credits_micro, 5);
    assert_eq!(p.remaining_percentage, 0);
    assert!(p.exhausted);
}

#[test]
fn the_percentage_is_floored() {
    let l = limits(3, 3, 3, 3);
    let now = at("2026-02-28T15:30:00Z");
    let u = usage(&[(Bucket::Total, Period::Daily, 1, 0)]);
    let status = quota_status(&l, &u, 80, now);
    // 2 of 3 left = 66.67 %
    assert_eq!(
        period_status(&status, QuotaTier::Total, Period::Daily)
            .unwrap()
            .remaining_percentage,
        66
    );
}

#[test]
fn periods_with_a_non_positive_limit_are_skipped() {
    let l = limits(1_000, 0, 0, 500);
    let status = quota_status(&l, &usage(&[]), 80, at("2026-02-28T15:30:00Z"));
    assert!(period_status(&status, QuotaTier::Total, Period::Daily).is_some());
    assert!(period_status(&status, QuotaTier::Total, Period::Monthly).is_none());
    assert!(period_status(&status, QuotaTier::Premium, Period::Daily).is_none());
    assert!(period_status(&status, QuotaTier::Premium, Period::Monthly).is_some());
}

#[test]
fn periods_of_a_tier_are_listed_daily_then_monthly() {
    let l = limits(1_000, 10_000, 500, 5_000);
    let status = quota_status(&l, &usage(&[]), 80, at("2026-02-28T15:30:00Z"));
    for tier in &status {
        let order: Vec<Period> = tier.periods.iter().map(|p| p.period).collect();
        assert_eq!(
            order,
            vec![Period::Daily, Period::Monthly],
            "{:?}",
            tier.tier
        );
    }
}

#[test]
fn warnings_carry_next_reset_only_when_warning_or_exhausted() {
    let l = limits(100, 100, 100, 100);
    let now = at("2026-02-28T15:30:00Z");
    let u = usage(&[
        (Bucket::Total, Period::Daily, 85, 0),       // warning
        (Bucket::Total, Period::Monthly, 100, 0),    // exhausted
        (Bucket::PremiumTier, Period::Daily, 10, 0), // fine
    ]);
    let status = quota_status(&l, &u, 80, now);
    let warnings: Vec<QuotaWarning> = quota_warnings(&status);

    let find = |tier, period| -> QuotaWarning {
        warnings
            .iter()
            .find(|w| w.tier == tier && w.period == period)
            .cloned()
            .unwrap_or_else(|| panic!("no warning entry for {tier:?}/{period:?}"))
    };
    let warn = find(QuotaTier::Total, Period::Daily);
    assert!(warn.warning && !warn.exhausted);
    assert_eq!(warn.remaining_percentage, 15);
    assert_eq!(warn.next_reset, Some(at("2026-03-01T00:00:00Z")));

    let gone = find(QuotaTier::Total, Period::Monthly);
    assert!(gone.exhausted && gone.warning);
    assert_eq!(gone.remaining_percentage, 0);
    assert_eq!(gone.next_reset, Some(at("2026-03-01T00:00:00Z")));

    let fine = find(QuotaTier::Premium, Period::Daily);
    assert!(!fine.warning && !fine.exhausted);
    assert_eq!(fine.remaining_percentage, 90);
    assert_eq!(fine.next_reset, None);
}

#[test]
fn skipped_periods_do_not_appear_in_the_warnings() {
    let l = limits(100, 0, 100, 0);
    let status = quota_status(&l, &usage(&[]), 80, at("2026-02-28T15:30:00Z"));
    let warnings = quota_warnings(&status);
    assert!(warnings.iter().all(|w| w.period == Period::Daily));
    assert_eq!(warnings.len(), 2);
}

// ═════════════════════════════════════════════════════════════════════════════
// billing outcome derivation (DESIGN 5.8 table, 5.7 "Usage accounting rules per outcome")
// ═════════════════════════════════════════════════════════════════════════════

fn billing(
    state: TurnState,
    code: Option<&str>,
    usage: Option<(u64, u64)>,
) -> (TerminalState, BillingOutcome, SettlementMethod) {
    let b = derive_billing(state, code, usage);
    (b.terminal_state, b.billing_outcome, b.settlement_method)
}

#[test]
fn a_completed_turn_always_settles_actual_even_without_usage() {
    for usage in [Some((10, 5)), Some((0, 0)), None] {
        assert_eq!(
            billing(TurnState::Completed, None, usage),
            (
                TerminalState::Completed,
                BillingOutcome::Completed,
                SettlementMethod::Actual
            ),
            "{usage:?}"
        );
    }
}

#[test]
fn post_provider_failures_settle_actual_with_usage_and_estimated_without() {
    for code in [
        "provider_error",
        "provider_timeout",
        "rate_limited",
        "web_search_calls_exceeded",
        "code_interpreter_calls_exceeded",
        "agentic_iterations_exceeded",
        "unexpected_tool_use",
        "message_persistence_failed",
    ] {
        assert_eq!(
            billing(TurnState::Failed, Some(code), Some((7, 3))),
            (
                TerminalState::Failed,
                BillingOutcome::Failed,
                SettlementMethod::Actual
            ),
            "{code} with usage"
        );
        assert_eq!(
            billing(TurnState::Failed, Some(code), None),
            (
                TerminalState::Failed,
                BillingOutcome::Failed,
                SettlementMethod::Estimated
            ),
            "{code} without usage"
        );
    }
}

#[test]
fn failed_usage_with_every_field_zero_is_unknown_usage() {
    assert_eq!(
        billing(TurnState::Failed, Some("provider_error"), Some((0, 0))).2,
        SettlementMethod::Estimated,
        "no zero-charge exploitation"
    );
    assert_eq!(
        billing(TurnState::Failed, Some("provider_error"), Some((0, 1))).2,
        SettlementMethod::Actual
    );
    assert_eq!(
        billing(TurnState::Failed, Some("provider_error"), Some((1, 0))).2,
        SettlementMethod::Actual
    );
}

#[test]
fn pre_provider_failures_are_released() {
    for code in [
        "context_length_exceeded",
        "validation_error",
        "input_too_long",
        "turn_setup_failed",
    ] {
        assert_eq!(
            billing(TurnState::Failed, Some(code), None),
            (
                TerminalState::Failed,
                BillingOutcome::Failed,
                SettlementMethod::Released
            ),
            "{code}"
        );
    }
}

#[test]
fn a_cancelled_turn_is_aborted_and_estimated_whatever_usage_it_saw() {
    for usage in [None, Some((10, 5))] {
        assert_eq!(
            billing(TurnState::Cancelled, None, usage),
            (
                TerminalState::Cancelled,
                BillingOutcome::Aborted,
                SettlementMethod::Estimated
            ),
            "{usage:?}"
        );
    }
}

#[test]
fn the_orphan_watchdog_outcome_is_aborted_estimated_with_state_failed() {
    assert_eq!(
        billing(TurnState::Failed, Some("orphan_timeout"), None),
        (
            TerminalState::Failed,
            BillingOutcome::Aborted,
            SettlementMethod::Estimated
        )
    );
}

#[test]
fn an_unknown_error_code_is_failed_estimated() {
    assert_eq!(
        billing(TurnState::Failed, Some("something_new"), Some((9, 9))),
        (
            TerminalState::Failed,
            BillingOutcome::Failed,
            SettlementMethod::Estimated
        ),
        "unknown codes never trust usage and never release"
    );
}

// ═════════════════════════════════════════════════════════════════════════════
// settlement formulas (DESIGN 5.4.4, 5.4.5, 5.5.14, 5.8, 5.9)
// ═════════════════════════════════════════════════════════════════════════════

fn turn_reserve(reserve_tokens: u64, max_out: u32, credits: i64, floor: u32) -> TurnReserve {
    TurnReserve {
        reserve_tokens,
        max_output_tokens_applied: max_out,
        reserved_credits_micro: credits,
        minimal_generation_floor_applied: floor,
    }
}

#[test]
fn the_floor_is_the_smaller_of_the_config_floor_and_the_output_cap() {
    assert_eq!(minimal_generation_floor_applied(50, 4096), 50);
    assert_eq!(minimal_generation_floor_applied(100, 60), 60);
    assert_eq!(minimal_generation_floor_applied(60, 60), 60);
}

#[test]
fn actual_settlement_charges_the_provider_usage() {
    // the 5.10 commit: reserve 1_500 tokens / 1_500_000, actual 900 + 300
    let reserve = turn_reserve(1_500, 500, 1_500_000, 50);
    let charge = settle_actual(&reserve, (900, 300), 1_000_000_000, 1_000_000_000, 1.10).unwrap();
    assert_eq!(charge.credits_micro, 1_200_000);
    assert_eq!(charge.method, SettlementMethod::Actual);
    assert_eq!((charge.input_tokens, charge.output_tokens), (900, 300));
}

#[test]
fn actual_equal_to_the_reserve_unfreezes_nothing() {
    let reserve = turn_reserve(1_500, 500, 1_500_000, 50);
    let charge = settle_actual(&reserve, (1_000, 500), 1_000_000_000, 1_000_000_000, 1.10).unwrap();
    assert_eq!(charge.credits_micro, 1_500_000);
}

#[test]
fn an_overshoot_within_the_tolerance_is_charged_in_full() {
    // 10_500 / 10_000 = 1.05 <= 1.10
    let reserve = turn_reserve(10_000, 4_000, 2_500_000, 50);
    let charge = settle_actual(&reserve, (10_000, 500), 250_000, 250_000, 1.10).unwrap();
    assert_eq!(
        charge.credits_micro,
        credits_micro(10_000, 500, 250_000, 250_000).unwrap()
    );
    assert!(charge.credits_micro > 0);
    // the tolerance edge is inclusive: 11_000 / 10_000 == 1.10
    let edge = settle_actual(&reserve, (10_000, 1_000), 250_000, 250_000, 1.10).unwrap();
    assert_eq!(
        edge.credits_micro,
        credits_micro(10_000, 1_000, 250_000, 250_000).unwrap()
    );
}

#[test]
fn an_overshoot_beyond_the_tolerance_is_capped_at_the_reserve() {
    // DESIGN 5.4.5 example: reserve 10_000 tokens / 2_500_000, actual 11_000 + 500 = 1.15
    let reserve = turn_reserve(10_000, 4_000, 2_500_000, 50);
    let charge = settle_actual(&reserve, (11_000, 500), 250_000, 250_000, 1.10).unwrap();
    assert_eq!(charge.credits_micro, 2_500_000, "committed = reserved");
    assert_eq!(charge.method, SettlementMethod::Actual);
    assert_eq!(
        (charge.input_tokens, charge.output_tokens),
        (11_000, 500),
        "token telemetry keeps the provider's actual counts"
    );
}

#[test]
fn the_overshoot_ratio_uses_floating_point_division() {
    // 10_001 / 10_000 = 1.0001 > 1.0 although integer division gives 1
    let reserve = turn_reserve(10_000, 4_000, 1_000_000, 50);
    let charge = settle_actual(&reserve, (10_001, 0), 100_000, 100_000, 1.0).unwrap();
    assert_eq!(charge.credits_micro, 1_000_000);
}

#[test]
fn zero_usage_on_an_actual_settlement_charges_nothing() {
    let reserve = turn_reserve(1_500, 500, 1_500_000, 50);
    let charge = settle_actual(&reserve, (0, 0), 1_000_000_000, 1_000_000_000, 1.10).unwrap();
    assert_eq!(charge.credits_micro, 0);
}

#[test]
fn out_of_range_provider_usage_is_a_settlement_error() {
    let reserve = turn_reserve(1_500, 500, 1_500_000, 50);
    let err = settle_actual(&reserve, (MAX_TOKENS + 1, 0), 1_000, 1_000, 1.10).unwrap_err();
    assert_eq!(err, CreditsError::TokenCountOutOfRange);
}

#[test]
fn estimated_settlement_charges_the_input_estimate_plus_the_floor() {
    // reserve 4_209 tokens = 113 input + 4_096 output cap; floor 50 -> 113 + 50 tokens
    let reserve = turn_reserve(4_209, 4_096, 61_779, 50);
    let charge = settle_estimated(&reserve, 3_000_000, 15_000_000).unwrap();
    assert_eq!(
        charge.credits_micro,
        credits_micro(113, 50, 3_000_000, 15_000_000).unwrap()
    );
    assert_eq!(charge.method, SettlementMethod::Estimated);
    assert_eq!(
        (charge.input_tokens, charge.output_tokens),
        (0, 0),
        "the estimated split is not telemetry"
    );
    assert!(charge.credits_micro < reserve.reserved_credits_micro);
}

#[test]
fn estimated_settlement_never_exceeds_the_reserve() {
    // a legacy row whose floor is larger than the output cap: charged tokens are capped at the reserve
    let reserve = turn_reserve(1_113, 1_000, 1_113, 5_000);
    let charge = settle_estimated(&reserve, 1_000_000, 1_000_000).unwrap();
    assert_eq!(charge.credits_micro, 1_113);
    assert!(charge.credits_micro <= reserve.reserved_credits_micro);
}

#[test]
fn estimated_settlement_is_deterministic_from_the_persisted_columns() {
    let reserve = turn_reserve(4_209, 4_096, 61_779, 50);
    let a = settle_estimated(&reserve, 3_000_000, 15_000_000).unwrap();
    let b = settle_estimated(&reserve, 3_000_000, 15_000_000).unwrap();
    assert_eq!(a, b);
}
