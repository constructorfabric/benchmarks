#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Slice 2: `cpt-cf-mini-chat-algo-quota-billing-tier-availability` and the
//! cascade of `cpt-cf-mini-chat-algo-quota-billing-preflight`
//! (`cpt-cf-mini-chat-dod-quota-billing-preflight`).

use mini_chat_sdk::{KillSwitches, ModelCatalogEntry, ModelTier, UserLimits};

use super::{
    CascadeInput, CascadeOutcome, CascadeRejected, DowngradeReason, QuotaDecision, run_cascade,
    tier_available,
};
use crate::domain::service::quota::test_support::{
    NO_SWITCHES, design_catalog, design_limits, design_rows, limits, model, snapshot, with_switches,
};
use crate::domain::service::quota::types::BucketRow;
use crate::domain::service::quota::types::QuotaBucket::{TierPremium, Total};
use crate::domain::service::quota::types::QuotaPeriod::{Daily, Monthly};

// ================================================================================================
// Tier availability
// ================================================================================================

#[test]
fn design_5_10_premium_is_unavailable_and_standard_is_available() {
    let rows = design_rows();
    let limits = design_limits(7);

    // premium reserve 3_750_000: tier:premium daily 20_000_000 + 3_750_000 > 22_000_000
    assert!(!tier_available(
        ModelTier::Premium,
        &rows,
        &limits,
        3_750_000
    ));
    // standard reserve 1_500_000: total daily 26_500_000 and monthly 241_500_000 fit
    assert!(tier_available(
        ModelTier::Standard,
        &rows,
        &limits,
        1_500_000
    ));
}

#[test]
fn availability_is_inclusive_at_the_limit_and_fails_one_micro_credit_above() {
    let rows = design_rows();
    let limits = design_limits(7);

    // tier:premium daily: 20_000_000 + 2_000_000 = 22_000_000 = the limit
    assert!(tier_available(
        ModelTier::Premium,
        &rows,
        &limits,
        2_000_000
    ));
    assert!(!tier_available(
        ModelTier::Premium,
        &rows,
        &limits,
        2_000_001
    ));
    // total daily: 25_000_000 + 35_000_000 = 60_000_000 = the limit
    assert!(tier_available(
        ModelTier::Standard,
        &rows,
        &limits,
        35_000_000
    ));
    assert!(!tier_available(
        ModelTier::Standard,
        &rows,
        &limits,
        35_000_001
    ));
}

#[test]
fn reserved_credits_of_other_in_flight_turns_count_like_spent_credits() {
    let rows = vec![BucketRow::new(Daily, Total, 0, 59_000_000)];
    let limits = design_limits(7);

    assert!(tier_available(
        ModelTier::Standard,
        &rows,
        &limits,
        1_000_000
    ));
    assert!(!tier_available(
        ModelTier::Standard,
        &rows,
        &limits,
        1_000_001
    ));
}

#[test]
fn a_missing_row_counts_as_zero_spent_and_zero_reserved() {
    let limits = limits(7, 1_000, 10_000, 500, 5_000);

    assert!(tier_available(ModelTier::Standard, &[], &limits, 1_000));
    assert!(!tier_available(ModelTier::Standard, &[], &limits, 1_001));
    assert!(tier_available(ModelTier::Premium, &[], &limits, 500));
    assert!(!tier_available(ModelTier::Premium, &[], &limits, 501));
}

#[test]
fn every_required_bucket_and_period_can_make_a_premium_candidate_unavailable() {
    let limits = limits(7, 1_000, 10_000, 500, 5_000);
    // One row over its limit, everything else empty.
    let cases = [
        (Daily, Total, 1_000),
        (Monthly, Total, 10_000),
        (Daily, TierPremium, 500),
        (Monthly, TierPremium, 5_000),
    ];
    for (period, bucket, used) in cases {
        let rows = vec![BucketRow::new(period, bucket, used, 0)];

        assert!(
            !tier_available(ModelTier::Premium, &rows, &limits, 1),
            "{period:?} {bucket:?}"
        );
        assert!(
            tier_available(ModelTier::Premium, &rows, &limits, 0),
            "{period:?} {bucket:?}: exactly at the limit with a zero reserve"
        );
    }
}

#[test]
fn a_standard_candidate_ignores_the_premium_bucket() {
    let limits = limits(7, 1_000_000, 10_000_000, 500, 5_000);
    let rows = vec![
        BucketRow::new(Daily, TierPremium, 10_000_000, 0),
        BucketRow::new(Monthly, TierPremium, 10_000_000, 0),
    ];

    assert!(tier_available(ModelTier::Standard, &rows, &limits, 100_000));
}

#[test]
fn bucket_total_is_limited_by_the_standard_limits_and_tier_premium_by_the_premium_ones() {
    // total limits tiny, premium limits large
    let tiny_total = limits(7, 10, 10, 1_000_000, 1_000_000);
    assert!(!tier_available(ModelTier::Standard, &[], &tiny_total, 100));
    assert!(!tier_available(ModelTier::Premium, &[], &tiny_total, 100));
    // total limits large, premium limits tiny
    let tiny_premium = limits(7, 1_000_000, 1_000_000, 10, 10);
    assert!(tier_available(ModelTier::Standard, &[], &tiny_premium, 100));
    assert!(!tier_available(ModelTier::Premium, &[], &tiny_premium, 100));
}

#[test]
fn an_overflowing_addition_makes_the_tier_unavailable() {
    let limits = limits(7, i64::MAX, i64::MAX, i64::MAX, i64::MAX);

    let rows = vec![BucketRow::new(Daily, Total, i64::MAX, 0)];
    assert!(!tier_available(ModelTier::Standard, &rows, &limits, 1));

    let rows = vec![BucketRow::new(Daily, Total, 1, 1)];
    assert!(
        !tier_available(ModelTier::Standard, &rows, &limits, i64::MAX),
        "1 + 1 + i64::MAX overflows"
    );
}

#[test]
fn the_unfittable_reserve_of_a_broken_candidate_fits_no_bucket() {
    let limits = design_limits(7);

    assert!(!tier_available(ModelTier::Standard, &[], &limits, i64::MAX));
    assert!(!tier_available(ModelTier::Premium, &[], &limits, i64::MAX));
}

// ================================================================================================
// Cascade
// ================================================================================================

struct Case {
    catalog: Vec<ModelCatalogEntry>,
    switches: KillSwitches,
    limits: UserLimits,
    rows: Vec<BucketRow>,
    selected: String,
    prior_context_tokens: i64,
    web_search_enabled: bool,
}

impl Case {
    /// The DESIGN 5.10 world, selecting `selected`.
    fn design(selected: &str) -> Self {
        Self {
            catalog: design_catalog(),
            switches: NO_SWITCHES,
            limits: design_limits(7),
            rows: design_rows(),
            selected: selected.to_owned(),
            prior_context_tokens: 0,
            web_search_enabled: false,
        }
    }

    fn run(&self) -> Result<CascadeOutcome, CascadeRejected> {
        let snapshot = with_switches(snapshot(7, self.catalog.clone()), self.switches);
        run_cascade(&CascadeInput {
            snapshot: &snapshot,
            limits: &self.limits,
            rows: &self.rows,
            selected_model: &self.selected,
            message_bytes: 1000,
            image_count: 0,
            web_search_enabled: self.web_search_enabled,
            has_ready_file_search_documents: false,
            has_ready_code_interpreter_files: false,
            prior_context_tokens: self.prior_context_tokens,
            streaming_max_output_tokens: 32768,
            minimal_generation_floor: 50,
        })
    }

    fn outcome(&self) -> CascadeOutcome {
        self.run().expect("a tier is available")
    }
}

fn id(outcome: &CascadeOutcome) -> &str {
    &outcome.entry.id
}

// ---- allow -------------------------------------------------------------------------------------------

#[test]
fn the_selected_premium_model_runs_when_every_premium_bucket_fits() {
    let mut case = Case::design("p");
    case.rows.clear();

    let outcome = case.outcome();

    assert_eq!(id(&outcome), "p");
    assert_eq!(outcome.tier, ModelTier::Premium);
    assert_eq!(outcome.downgrade_reason, None);
    assert_eq!(outcome.estimate.reserved_credits_micro, 3_750_000);
}

#[test]
fn a_standard_selection_runs_on_standard_and_never_on_premium() {
    let mut case = Case::design("s");
    case.rows.clear();

    let outcome = case.outcome();

    assert_eq!(id(&outcome), "s");
    assert_eq!(outcome.tier, ModelTier::Standard);
    assert_eq!(outcome.downgrade_reason, None);
}

// ---- the DESIGN 5.10 downgrade -----------------------------------------------------------------------

#[test]
fn design_5_10_downgrades_to_the_standard_model_booking_the_standard_reserve() {
    let outcome = Case::design("p").outcome();

    assert_eq!(id(&outcome), "s");
    assert_eq!(outcome.tier, ModelTier::Standard);
    assert_eq!(
        outcome.downgrade_reason,
        Some(DowngradeReason::PremiumQuotaExhausted)
    );
    assert_eq!(outcome.estimate.estimated_input_tokens, 1000);
    assert_eq!(outcome.estimate.max_output_tokens_applied, 500);
    assert_eq!(outcome.estimate.reserve_tokens, 1500);
    assert_eq!(outcome.estimate.reserved_credits_micro, 1_500_000);
}

#[test]
fn the_premium_monthly_bucket_exhausting_also_downgrades() {
    let mut case = Case::design("p");
    case.rows = vec![BucketRow::new(Monthly, TierPremium, 299_000_000, 0)];

    let outcome = case.outcome();

    assert_eq!(id(&outcome), "s");
    assert_eq!(
        outcome.downgrade_reason,
        Some(DowngradeReason::PremiumQuotaExhausted)
    );
}

#[test]
fn the_total_bucket_exhausting_leaves_no_tier_and_rejects_with_the_starting_tier() {
    let mut case = Case::design("p");
    case.rows = vec![BucketRow::new(Daily, Total, 59_500_000, 0)];

    assert_eq!(
        case.run().unwrap_err(),
        CascadeRejected {
            starting_tier: ModelTier::Premium
        }
    );
    case.selected = "s".to_owned();
    assert_eq!(
        case.run().unwrap_err(),
        CascadeRejected {
            starting_tier: ModelTier::Standard
        }
    );
}

#[test]
fn the_booked_reserve_includes_the_prior_context_of_the_chat() {
    let mut case = Case::design("p");
    case.rows.clear();
    case.limits = limits(7, 60_000_000, 600_000_000, 4_000_000, 300_000_000);

    assert_eq!(id(&case.outcome()), "p", "1000 + 500 tokens fit 4_000_000");

    case.prior_context_tokens = 200;
    // (1200 + 500) tokens at 2.5 credits per 1K = 4_250_000 > 4_000_000
    let outcome = case.outcome();
    assert_eq!(id(&outcome), "s");
    assert_eq!(outcome.estimate.estimated_input_tokens, 1200);
}

#[test]
fn only_one_candidate_per_tier_is_checked() {
    let mut case = Case::design("s");
    case.catalog = vec![
        model("s-expensive", ModelTier::Standard)
            .mult(10_000_000_000, 10_000_000_000)
            .build(),
        model("s-cheap", ModelTier::Standard)
            .default_flag(Some(false))
            .build(),
    ];
    case.selected = "s-expensive".to_owned();
    case.rows.clear();
    // s-expensive reserve: 1500 tokens * 10 credits per 1K = 15_000_000; s-cheap 1_500_000
    case.limits = limits(7, 2_000_000, 600_000_000, 22_000_000, 300_000_000);

    assert_eq!(
        case.run().unwrap_err(),
        CascadeRejected {
            starting_tier: ModelTier::Standard
        },
        "the selected model is the candidate; the cheaper one is not tried"
    );
}

// ---- kill switches -----------------------------------------------------------------------------------

#[test]
fn force_standard_tier_skips_premium_with_its_own_reason() {
    let mut case = Case::design("p");
    case.rows.clear();
    case.switches.force_standard_tier = true;

    let outcome = case.outcome();

    assert_eq!(id(&outcome), "s");
    assert_eq!(
        outcome.downgrade_reason,
        Some(DowngradeReason::ForceStandardTier)
    );
}

#[test]
fn disable_premium_tier_skips_premium_with_its_own_reason() {
    let mut case = Case::design("p");
    case.rows.clear();
    case.switches.disable_premium_tier = true;

    let outcome = case.outcome();

    assert_eq!(id(&outcome), "s");
    assert_eq!(
        outcome.downgrade_reason,
        Some(DowngradeReason::DisablePremiumTier)
    );
}

#[test]
fn with_both_premium_switches_set_the_reason_is_force_standard_tier() {
    let mut case = Case::design("p");
    case.rows.clear();
    case.switches.force_standard_tier = true;
    case.switches.disable_premium_tier = true;

    assert_eq!(
        case.outcome().downgrade_reason,
        Some(DowngradeReason::ForceStandardTier)
    );
}

#[test]
fn a_skipped_premium_tier_is_not_reported_as_exhausted_even_when_it_is() {
    let mut case = Case::design("p");
    case.switches.disable_premium_tier = true;

    // The premium bucket is exhausted too (design rows), but the reason set
    // first (the switch) stays.
    assert_eq!(
        case.outcome().downgrade_reason,
        Some(DowngradeReason::DisablePremiumTier)
    );
}

#[test]
fn the_premium_switches_do_not_touch_a_standard_selection() {
    let mut case = Case::design("s");
    case.rows.clear();
    case.switches.force_standard_tier = true;
    case.switches.disable_premium_tier = true;

    let outcome = case.outcome();

    assert_eq!(id(&outcome), "s");
    assert_eq!(outcome.downgrade_reason, None);
}

// ---- disabled and missing selected models ------------------------------------------------------------

#[test]
fn a_disabled_selected_premium_model_starts_at_premium_with_the_model_disabled_reason() {
    let mut case = Case::design("p");
    case.rows.clear();
    case.catalog = vec![
        model("p", ModelTier::Premium).enabled(false).build(),
        model("p2", ModelTier::Premium).build(),
        model("s", ModelTier::Standard).build(),
    ];

    let outcome = case.outcome();

    assert_eq!(id(&outcome), "p2", "the default enabled model of the tier");
    assert_eq!(outcome.tier, ModelTier::Premium);
    assert_eq!(
        outcome.downgrade_reason,
        Some(DowngradeReason::ModelDisabled)
    );
}

#[test]
fn a_premium_tier_without_an_enabled_model_is_skipped_keeping_the_model_disabled_reason() {
    let mut case = Case::design("p");
    case.rows.clear();
    case.catalog = vec![
        model("p", ModelTier::Premium).enabled(false).build(),
        model("s", ModelTier::Standard).build(),
    ];

    let outcome = case.outcome();

    assert_eq!(id(&outcome), "s");
    assert_eq!(
        outcome.downgrade_reason,
        Some(DowngradeReason::ModelDisabled)
    );
}

#[test]
fn a_disabled_selected_standard_model_is_replaced_within_the_standard_tier() {
    let mut case = Case::design("s1");
    case.rows.clear();
    case.catalog = vec![
        model("p", ModelTier::Premium).build(),
        model("s1", ModelTier::Standard).enabled(false).build(),
        model("s2", ModelTier::Standard).build(),
    ];

    let outcome = case.outcome();

    assert_eq!(id(&outcome), "s2");
    assert_eq!(outcome.tier, ModelTier::Standard, "never cascades up");
    assert_eq!(
        outcome.downgrade_reason,
        Some(DowngradeReason::ModelDisabled)
    );
}

#[test]
fn a_selected_model_missing_from_the_catalog_starts_at_premium_with_model_disabled() {
    let mut case = Case::design("gone");
    case.rows.clear();

    let outcome = case.outcome();

    assert_eq!(id(&outcome), "p", "the first enabled default premium model");
    assert_eq!(
        outcome.downgrade_reason,
        Some(DowngradeReason::ModelDisabled)
    );
}

#[test]
fn a_missing_selected_model_falls_through_to_standard_when_premium_cannot_take_it() {
    let case = Case::design("gone");

    // The design rows exhaust premium; the model_disabled reason set first stays.
    let outcome = case.outcome();

    assert_eq!(id(&outcome), "s");
    assert_eq!(
        outcome.downgrade_reason,
        Some(DowngradeReason::ModelDisabled)
    );
}

#[test]
fn the_model_disabled_reason_wins_over_the_kill_switches() {
    let mut case = Case::design("p");
    case.rows.clear();
    case.catalog = vec![
        model("p", ModelTier::Premium).enabled(false).build(),
        model("s", ModelTier::Standard).build(),
    ];
    case.switches.force_standard_tier = true;

    assert_eq!(
        case.outcome().downgrade_reason,
        Some(DowngradeReason::ModelDisabled)
    );
}

#[test]
fn no_enabled_model_in_any_tier_rejects() {
    let mut case = Case::design("p");
    case.catalog = vec![
        model("p", ModelTier::Premium).enabled(false).build(),
        model("s", ModelTier::Standard).enabled(false).build(),
    ];

    assert_eq!(
        case.run().unwrap_err(),
        CascadeRejected {
            starting_tier: ModelTier::Premium
        }
    );
}

#[test]
fn a_tier_with_quota_but_no_enabled_model_is_unavailable() {
    let mut case = Case::design("s");
    case.rows.clear();
    case.catalog = vec![
        model("p", ModelTier::Premium).build(),
        model("s", ModelTier::Standard).enabled(false).build(),
    ];

    assert_eq!(
        case.run().unwrap_err(),
        CascadeRejected {
            starting_tier: ModelTier::Standard
        }
    );
}

// ---- candidate selection -----------------------------------------------------------------------------

fn standard_catalog_run(catalog: Vec<ModelCatalogEntry>, selected: &str) -> String {
    let mut case = Case::design(selected);
    case.rows.clear();
    case.catalog = catalog;
    id(&case.outcome()).to_owned()
}

#[test]
fn the_selected_model_is_the_candidate_when_it_is_enabled_in_its_tier() {
    let catalog = vec![
        model("s-default", ModelTier::Standard).build(),
        model("s-other", ModelTier::Standard)
            .default_flag(Some(false))
            .build(),
    ];

    assert_eq!(standard_catalog_run(catalog, "s-other"), "s-other");
}

#[test]
fn the_fallback_candidate_is_the_first_enabled_default_model_in_catalog_order() {
    let catalog = vec![
        model("s0", ModelTier::Standard).default_flag(None).build(),
        model("s1", ModelTier::Standard)
            .default_flag(Some(true))
            .build(),
        model("s2", ModelTier::Standard)
            .default_flag(Some(true))
            .build(),
        model("gone", ModelTier::Premium).enabled(false).build(),
    ];

    assert_eq!(standard_catalog_run(catalog, "gone"), "s1");
}

#[test]
fn without_a_default_the_first_enabled_model_of_the_tier_is_the_candidate() {
    let catalog = vec![
        model("s0", ModelTier::Standard)
            .default_flag(Some(false))
            .build(),
        model("s1", ModelTier::Standard).default_flag(None).build(),
        model("gone", ModelTier::Premium).enabled(false).build(),
    ];

    assert_eq!(standard_catalog_run(catalog, "gone"), "s0");
}

#[test]
fn a_disabled_default_model_is_not_a_candidate() {
    let catalog = vec![
        model("s0", ModelTier::Standard).enabled(false).build(),
        model("s1", ModelTier::Standard).default_flag(None).build(),
        model("gone", ModelTier::Premium).enabled(false).build(),
    ];

    assert_eq!(standard_catalog_run(catalog, "gone"), "s1");
}

// ---- per-candidate estimate ------------------------------------------------------------------------

#[test]
fn each_candidate_is_estimated_with_its_own_tool_support() {
    let mut case = Case::design("p");
    case.catalog = vec![
        model("p", ModelTier::Premium)
            .mult(2_500_000_000, 2_500_000_000)
            .tools(true, true, true)
            .build(),
        model("s", ModelTier::Standard)
            .tools(false, true, true)
            .build(),
    ];
    case.web_search_enabled = true;

    let outcome = case.outcome();

    // premium is exhausted: standard has no web search support -> no surcharge
    assert_eq!(id(&outcome), "s");
    assert_eq!(outcome.estimate.estimated_input_tokens, 1000);

    case.rows.clear();
    case.selected = "p".to_owned();
    let outcome = case.outcome();
    assert_eq!(id(&outcome), "p");
    assert_eq!(
        outcome.estimate.estimated_input_tokens, 1500,
        "1000 + web 500"
    );
}

// ---- helpers and constants ---------------------------------------------------------------------------

#[test]
fn the_decision_values_have_their_wire_names() {
    assert_eq!(QuotaDecision::Allow.as_str(), "allow");
    assert_eq!(QuotaDecision::Downgrade.as_str(), "downgrade");
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

#[test]
fn the_premium_skip_reason_prefers_force_standard_tier() {
    use super::premium_skipped_by;

    assert_eq!(premium_skipped_by(NO_SWITCHES), None);
    assert_eq!(
        premium_skipped_by(KillSwitches {
            disable_premium_tier: true,
            ..NO_SWITCHES
        }),
        Some(DowngradeReason::DisablePremiumTier)
    );
    assert_eq!(
        premium_skipped_by(KillSwitches {
            disable_premium_tier: true,
            force_standard_tier: true,
            ..NO_SWITCHES
        }),
        Some(DowngradeReason::ForceStandardTier)
    );
}
