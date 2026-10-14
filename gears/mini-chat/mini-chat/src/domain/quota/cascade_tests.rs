//! Downgrade cascade tests: the truth table over premium and standard buckets in both periods,
//! the kill switches and their reasons, a disabled or missing selected model, a tier without an
//! enabled model, the candidate choice per tier, the one-candidate-per-tier rule, the
//! per-candidate estimate, the DESIGN 5.10 numbers and the exposed capabilities.
//!
//! Slice S1-all: algorithm `cpt-cf-mini-chat-algo-quota-and-billing-downgrade-cascade`,
//! definition of done `cpt-cf-mini-chat-dod-quota-and-billing-preflight-downgrade`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use mini_chat_sdk::{
    KillSwitches, ModelCatalogEntry, ModelPreference, ModelTier, PolicySnapshot, QuotaDecision,
};
use serde_json::json;

use super::{CascadeDecision, CascadeInput, DowngradeReason, run_cascade};
use crate::domain::quota::estimation::RequestFacts;
use crate::domain::quota::periods::PeriodType;
use crate::domain::quota::rows::{Bucket, BucketRow, QuotaRows};
use crate::test_support::quota::{
    PREMIUM, STANDARD, catalog_v, default_settings, design_limits, model, plain_facts,
    premium_model, standard_model,
};
use crate::test_support::{USER_A1, entry};

fn spent(credits: i64) -> BucketRow {
    BucketRow {
        spent_credits_micro: credits,
        ..BucketRow::default()
    }
}

/// The DESIGN 5.10 consumption.
fn design_rows() -> QuotaRows {
    QuotaRows::new()
        .with(PeriodType::Daily, Bucket::TierPremium, spent(20_000_000))
        .with(PeriodType::Monthly, Bucket::TierPremium, spent(200_000_000))
        .with(PeriodType::Daily, Bucket::Total, spent(25_000_000))
        .with(PeriodType::Monthly, Bucket::Total, spent(240_000_000))
}

fn snap(entries: Vec<ModelCatalogEntry>) -> PolicySnapshot {
    catalog_v(1, entries)
}

fn with_switches(mut snapshot: PolicySnapshot, switches: KillSwitches) -> PolicySnapshot {
    snapshot.kill_switches = switches;
    snapshot
}

fn cascade_with(
    snapshot: &PolicySnapshot,
    selected: &str,
    rows: &QuotaRows,
    facts: &RequestFacts,
    prior_context_tokens: i64,
) -> Option<CascadeDecision> {
    let limits = design_limits(USER_A1);
    let settings = default_settings();
    run_cascade(&CascadeInput {
        snapshot,
        selected_model: selected,
        limits: &limits,
        rows,
        facts,
        prior_context_tokens,
        settings: &settings,
    })
}

fn cascade(snapshot: &PolicySnapshot, selected: &str, rows: &QuotaRows) -> Option<CascadeDecision> {
    cascade_with(snapshot, selected, rows, &plain_facts(), 0)
}

fn disabled(mut model: ModelCatalogEntry) -> ModelCatalogEntry {
    model.enabled = false;
    model
}

fn named(id: &str, tier: ModelTier, multiplier: u64) -> ModelCatalogEntry {
    model(id, tier, multiplier)
}

fn default_flag(mut model: ModelCatalogEntry) -> ModelCatalogEntry {
    model.preference = Some(ModelPreference {
        is_default: true,
        sort_order: 1,
    });
    model
}

// ── allow ────────────────────────────────────────────────────────────────────

#[test]
fn an_available_premium_model_is_allowed_as_selected() {
    let decision = cascade(
        &snap(vec![premium_model(), standard_model()]),
        PREMIUM,
        &QuotaRows::new(),
    )
    .expect("allowed");
    assert_eq!(decision.effective_model, PREMIUM);
    assert_eq!(decision.tier, ModelTier::Premium);
    assert_eq!(decision.quota_decision, QuotaDecision::Allow);
    assert_eq!(decision.downgrade_from, None);
    assert_eq!(decision.downgrade_reason, None);
    assert_eq!(decision.reserve.reserved_credits_micro, 3_750_000);
}

#[test]
fn an_available_standard_model_is_allowed_as_selected() {
    let decision = cascade(
        &snap(vec![premium_model(), standard_model()]),
        STANDARD,
        &QuotaRows::new(),
    )
    .expect("allowed");
    assert_eq!(decision.effective_model, STANDARD);
    assert_eq!(decision.tier, ModelTier::Standard);
    assert_eq!(decision.quota_decision, QuotaDecision::Allow);
    assert_eq!(decision.reserve.reserved_credits_micro, 1_500_000);
}

#[test]
fn the_cascade_is_deterministic() {
    let snapshot = snap(vec![premium_model(), standard_model()]);
    assert_eq!(
        cascade(&snapshot, PREMIUM, &design_rows()),
        cascade(&snapshot, PREMIUM, &design_rows())
    );
}

// ── premium exhausted ────────────────────────────────────────────────────────

#[test]
fn the_design_example_downgrades_to_standard_with_the_standard_reserve() {
    let decision = cascade(
        &snap(vec![premium_model(), standard_model()]),
        PREMIUM,
        &design_rows(),
    )
    .expect("downgraded");
    assert_eq!(decision.effective_model, STANDARD);
    assert_eq!(decision.tier, ModelTier::Standard);
    assert_eq!(decision.quota_decision, QuotaDecision::Downgrade);
    assert_eq!(decision.downgrade_from.as_deref(), Some(PREMIUM));
    assert_eq!(
        decision.downgrade_reason,
        Some(DowngradeReason::PremiumQuotaExhausted)
    );
    // the booked reserve is the one of the effective model, not the premium one that failed
    assert_eq!(decision.estimate.estimated_input_tokens, 1000);
    assert_eq!(decision.reserve.reserve_tokens, 1500);
    assert_eq!(decision.reserve.reserved_credits_micro, 1_500_000);
    assert!(!decision.reserve.credit_computation_failed);
}

#[test]
fn the_premium_monthly_sub_cap_downgrades() {
    let rows = QuotaRows::new().with(PeriodType::Monthly, Bucket::TierPremium, spent(299_000_000));
    let decision = cascade(
        &snap(vec![premium_model(), standard_model()]),
        PREMIUM,
        &rows,
    )
    .unwrap();
    assert_eq!(decision.effective_model, STANDARD);
    assert_eq!(
        decision.downgrade_reason,
        Some(DowngradeReason::PremiumQuotaExhausted)
    );
}

#[test]
fn a_total_bucket_that_fits_standard_but_not_premium_downgrades() {
    // 57_000_000 + 3_750_000 > 60_000_000 but 57_000_000 + 1_500_000 <= 60_000_000
    let rows = QuotaRows::new().with(PeriodType::Daily, Bucket::Total, spent(57_000_000));
    let decision = cascade(
        &snap(vec![premium_model(), standard_model()]),
        PREMIUM,
        &rows,
    )
    .unwrap();
    assert_eq!(decision.effective_model, STANDARD);
    assert_eq!(
        decision.downgrade_reason,
        Some(DowngradeReason::PremiumQuotaExhausted)
    );
}

#[test]
fn a_reserve_in_flight_counts_for_the_downgrade() {
    let rows = QuotaRows::new().with(
        PeriodType::Daily,
        Bucket::TierPremium,
        BucketRow {
            reserved_credits_micro: 20_000_000,
            ..BucketRow::default()
        },
    );
    let decision = cascade(
        &snap(vec![premium_model(), standard_model()]),
        PREMIUM,
        &rows,
    )
    .unwrap();
    assert_eq!(decision.effective_model, STANDARD);
}

// ── nothing available ────────────────────────────────────────────────────────

#[test]
fn an_exhausted_total_bucket_rejects_both_tiers() {
    let rows = QuotaRows::new().with(PeriodType::Daily, Bucket::Total, spent(59_500_000));
    assert!(
        cascade(
            &snap(vec![premium_model(), standard_model()]),
            PREMIUM,
            &rows
        )
        .is_none()
    );
    assert!(
        cascade(
            &snap(vec![premium_model(), standard_model()]),
            STANDARD,
            &rows
        )
        .is_none()
    );
}

#[test]
fn an_exhausted_monthly_total_bucket_rejects() {
    let rows = QuotaRows::new().with(PeriodType::Monthly, Bucket::Total, spent(599_000_000));
    assert!(
        cascade(
            &snap(vec![premium_model(), standard_model()]),
            PREMIUM,
            &rows
        )
        .is_none()
    );
}

#[test]
fn both_exhausted_premium_and_total_reject() {
    let rows = design_rows()
        .with(PeriodType::Daily, Bucket::Total, spent(59_900_000))
        .with(PeriodType::Monthly, Bucket::Total, spent(599_900_000));
    assert!(
        cascade(
            &snap(vec![premium_model(), standard_model()]),
            PREMIUM,
            &rows
        )
        .is_none()
    );
}

#[test]
fn a_catalog_without_enabled_models_rejects() {
    let snapshot = snap(vec![disabled(premium_model()), disabled(standard_model())]);
    assert!(cascade(&snapshot, PREMIUM, &QuotaRows::new()).is_none());
    assert!(cascade(&snap(vec![]), PREMIUM, &QuotaRows::new()).is_none());
}

// ── kill switches ────────────────────────────────────────────────────────────

#[test]
fn force_standard_tier_skips_premium_with_its_reason() {
    let snapshot = with_switches(
        snap(vec![premium_model(), standard_model()]),
        KillSwitches {
            force_standard_tier: true,
            ..KillSwitches::default()
        },
    );
    let decision = cascade(&snapshot, PREMIUM, &QuotaRows::new()).unwrap();
    assert_eq!(decision.effective_model, STANDARD);
    assert_eq!(decision.quota_decision, QuotaDecision::Downgrade);
    assert_eq!(
        decision.downgrade_reason,
        Some(DowngradeReason::ForceStandardTier)
    );
    assert_eq!(decision.downgrade_from.as_deref(), Some(PREMIUM));
}

#[test]
fn disable_premium_tier_skips_premium_with_its_reason() {
    let snapshot = with_switches(
        snap(vec![premium_model(), standard_model()]),
        KillSwitches {
            disable_premium_tier: true,
            ..KillSwitches::default()
        },
    );
    let decision = cascade(&snapshot, PREMIUM, &QuotaRows::new()).unwrap();
    assert_eq!(decision.effective_model, STANDARD);
    assert_eq!(
        decision.downgrade_reason,
        Some(DowngradeReason::DisablePremiumTier)
    );
}

#[test]
fn force_standard_tier_wins_over_disable_premium_tier() {
    let snapshot = with_switches(
        snap(vec![premium_model(), standard_model()]),
        KillSwitches {
            force_standard_tier: true,
            disable_premium_tier: true,
            ..KillSwitches::default()
        },
    );
    let decision = cascade(&snapshot, PREMIUM, &QuotaRows::new()).unwrap();
    assert_eq!(
        decision.downgrade_reason,
        Some(DowngradeReason::ForceStandardTier)
    );
}

#[test]
fn a_premium_kill_switch_does_not_touch_a_standard_selection() {
    let snapshot = with_switches(
        snap(vec![premium_model(), standard_model()]),
        KillSwitches {
            force_standard_tier: true,
            ..KillSwitches::default()
        },
    );
    let decision = cascade(&snapshot, STANDARD, &QuotaRows::new()).unwrap();
    assert_eq!(decision.quota_decision, QuotaDecision::Allow);
    assert_eq!(decision.downgrade_reason, None);
}

#[test]
fn a_premium_kill_switch_with_no_standard_model_rejects() {
    let snapshot = with_switches(
        snap(vec![premium_model()]),
        KillSwitches {
            disable_premium_tier: true,
            ..KillSwitches::default()
        },
    );
    assert!(cascade(&snapshot, PREMIUM, &QuotaRows::new()).is_none());
}

// ── disabled or missing selected model ───────────────────────────────────────

#[test]
fn a_disabled_premium_selection_takes_the_first_enabled_premium_model() {
    let snapshot = snap(vec![
        disabled(premium_model()),
        named("premium-2", ModelTier::Premium, 2_500_000_000),
        named("premium-3", ModelTier::Premium, 2_500_000_000),
        standard_model(),
    ]);
    let decision = cascade(&snapshot, PREMIUM, &QuotaRows::new()).unwrap();
    assert_eq!(decision.effective_model, "premium-2");
    assert_eq!(decision.tier, ModelTier::Premium);
    assert_eq!(decision.quota_decision, QuotaDecision::Downgrade);
    assert_eq!(decision.downgrade_from.as_deref(), Some(PREMIUM));
    assert_eq!(
        decision.downgrade_reason,
        Some(DowngradeReason::ModelDisabled)
    );
}

#[test]
fn the_tenant_default_model_of_the_tier_is_preferred_to_catalog_order() {
    let snapshot = snap(vec![
        disabled(premium_model()),
        named("premium-2", ModelTier::Premium, 2_500_000_000),
        default_flag(named("premium-3", ModelTier::Premium, 2_500_000_000)),
        standard_model(),
    ]);
    let decision = cascade(&snapshot, PREMIUM, &QuotaRows::new()).unwrap();
    assert_eq!(decision.effective_model, "premium-3");
    assert_eq!(
        decision.downgrade_reason,
        Some(DowngradeReason::ModelDisabled)
    );
}

#[test]
fn an_enabled_selected_model_is_chosen_even_when_another_model_is_the_default() {
    let snapshot = snap(vec![
        default_flag(named("premium-2", ModelTier::Premium, 2_500_000_000)),
        premium_model(),
        standard_model(),
    ]);
    let decision = cascade(&snapshot, PREMIUM, &QuotaRows::new()).unwrap();
    assert_eq!(decision.effective_model, PREMIUM);
    assert_eq!(decision.quota_decision, QuotaDecision::Allow);
}

#[test]
fn a_disabled_standard_selection_takes_another_enabled_standard_model() {
    let snapshot = snap(vec![
        premium_model(),
        disabled(standard_model()),
        named("standard-2", ModelTier::Standard, 1_000_000_000),
    ]);
    let decision = cascade(&snapshot, STANDARD, &QuotaRows::new()).unwrap();
    assert_eq!(decision.effective_model, "standard-2");
    assert_eq!(decision.tier, ModelTier::Standard);
    assert_eq!(decision.quota_decision, QuotaDecision::Downgrade);
    assert_eq!(
        decision.downgrade_reason,
        Some(DowngradeReason::ModelDisabled)
    );
}

#[test]
fn a_missing_selected_model_is_treated_as_premium_with_model_disabled() {
    let snapshot = snap(vec![premium_model(), standard_model()]);
    let decision = cascade(&snapshot, "ghost", &QuotaRows::new()).unwrap();
    assert_eq!(decision.effective_model, PREMIUM);
    assert_eq!(decision.quota_decision, QuotaDecision::Downgrade);
    assert_eq!(decision.downgrade_from.as_deref(), Some("ghost"));
    assert_eq!(
        decision.downgrade_reason,
        Some(DowngradeReason::ModelDisabled)
    );
}

#[test]
fn model_disabled_stays_the_reason_when_premium_quota_is_exhausted_afterwards() {
    let snapshot = snap(vec![premium_model(), standard_model()]);
    let decision = cascade(&snapshot, "ghost", &design_rows()).unwrap();
    assert_eq!(decision.effective_model, STANDARD);
    assert_eq!(
        decision.downgrade_reason,
        Some(DowngradeReason::ModelDisabled)
    );
}

#[test]
fn model_disabled_stays_the_reason_when_a_kill_switch_skips_premium_afterwards() {
    let snapshot = with_switches(
        snap(vec![
            disabled(premium_model()),
            named("premium-2", ModelTier::Premium, 2_500_000_000),
            standard_model(),
        ]),
        KillSwitches {
            force_standard_tier: true,
            ..KillSwitches::default()
        },
    );
    let decision = cascade(&snapshot, PREMIUM, &QuotaRows::new()).unwrap();
    assert_eq!(decision.effective_model, STANDARD);
    assert_eq!(
        decision.downgrade_reason,
        Some(DowngradeReason::ModelDisabled)
    );
}

#[test]
fn a_tier_without_an_enabled_model_is_skipped() {
    let snapshot = snap(vec![disabled(premium_model()), standard_model()]);
    let decision = cascade(&snapshot, PREMIUM, &QuotaRows::new()).unwrap();
    assert_eq!(decision.effective_model, STANDARD);
    assert_eq!(
        decision.downgrade_reason,
        Some(DowngradeReason::ModelDisabled)
    );
}

// ── a standard model never cascades up ───────────────────────────────────────

#[test]
fn a_standard_selection_never_upgrades_to_premium() {
    let snapshot = snap(vec![premium_model(), standard_model()]);
    for rows in [QuotaRows::new(), design_rows()] {
        let decision = cascade(&snapshot, STANDARD, &rows).unwrap();
        assert_eq!(decision.tier, ModelTier::Standard);
    }
}

#[test]
fn a_disabled_standard_selection_without_another_standard_model_rejects_instead_of_upgrading() {
    let snapshot = snap(vec![premium_model(), disabled(standard_model())]);
    assert!(cascade(&snapshot, STANDARD, &QuotaRows::new()).is_none());
}

// ── one candidate per tier ───────────────────────────────────────────────────

#[test]
fn only_one_candidate_per_tier_is_checked() {
    // the selected premium model is too expensive for the sub-cap, another premium model would
    // fit, but the cascade goes to the standard tier instead of trying it
    let expensive = named(PREMIUM, ModelTier::Premium, 10_000_000_000);
    let cheap = named("premium-2", ModelTier::Premium, 1_000_000_000);
    let snapshot = snap(vec![expensive, cheap, standard_model()]);
    let rows = QuotaRows::new().with(PeriodType::Daily, Bucket::TierPremium, spent(10_000_000));
    let decision = cascade(&snapshot, PREMIUM, &rows).unwrap();
    assert_eq!(decision.effective_model, STANDARD);
    assert_eq!(
        decision.downgrade_reason,
        Some(DowngradeReason::PremiumQuotaExhausted)
    );
}

// ── a candidate whose credit computation failed ──────────────────────────────

#[test]
fn a_premium_candidate_with_a_zero_multiplier_is_unavailable_and_downgrades() {
    let broken = named(PREMIUM, ModelTier::Premium, 0);
    let snapshot = snap(vec![broken, standard_model()]);
    let decision = cascade(&snapshot, PREMIUM, &QuotaRows::new()).unwrap();
    assert_eq!(decision.effective_model, STANDARD);
    assert_eq!(
        decision.downgrade_reason,
        Some(DowngradeReason::PremiumQuotaExhausted)
    );
    assert!(!decision.reserve.credit_computation_failed);
}

#[test]
fn when_every_candidate_fails_the_cascade_rejects_without_a_panic() {
    let snapshot = snap(vec![
        named(PREMIUM, ModelTier::Premium, 0),
        named(STANDARD, ModelTier::Standard, 10_000_000_001),
    ]);
    assert!(cascade(&snapshot, PREMIUM, &QuotaRows::new()).is_none());
    // also with non-zero counters in the rows
    let rows = QuotaRows::new().with(PeriodType::Daily, Bucket::Total, spent(i64::MAX - 1));
    assert!(cascade(&snapshot, PREMIUM, &rows).is_none());
}

// ── the estimate of the effective candidate ──────────────────────────────────

#[test]
fn each_candidate_is_estimated_with_its_own_tool_support() {
    let mut premium = premium_model();
    premium.general_config.tool_support.web_search = true;
    let mut standard = standard_model();
    standard.general_config.tool_support.web_search = false;
    let snapshot = snap(vec![premium, standard]);
    let facts = RequestFacts {
        web_search_enabled: true,
        ..plain_facts()
    };

    let allowed = cascade_with(&snapshot, PREMIUM, &QuotaRows::new(), &facts, 0).unwrap();
    assert_eq!(allowed.estimate.web_search_surcharge_tokens, 500);
    assert_eq!(allowed.estimate.estimated_input_tokens, 1500);
    assert_eq!(allowed.reserve.reserve_tokens, 2000);
    assert_eq!(allowed.reserve.reserved_credits_micro, 5_000_000);

    // the downgraded candidate has no web search: no surcharge is reserved for a tool that is
    // left out of the provider request
    let downgraded = cascade_with(&snapshot, PREMIUM, &design_rows(), &facts, 0).unwrap();
    assert_eq!(downgraded.effective_model, STANDARD);
    assert_eq!(downgraded.estimate.web_search_surcharge_tokens, 0);
    assert_eq!(downgraded.estimate.estimated_input_tokens, 1000);
    assert_eq!(downgraded.reserve.reserved_credits_micro, 1_500_000);
}

#[test]
fn each_candidate_is_estimated_with_its_own_budgets() {
    let premium = entry(
        PREMIUM,
        ModelTier::Premium,
        true,
        &json!({
            "input_tokens_credit_multiplier_micro": 2_500_000_000_u64,
            "output_tokens_credit_multiplier_micro": 2_500_000_000_u64,
            "max_output_tokens": 500,
            "estimation_budgets": {
                "bytes_per_token_conservative": 1, "fixed_overhead_tokens": 0, "safety_margin_pct": 0
            },
        }),
    );
    let snapshot = snap(vec![premium, standard_model()]);
    // premium: 4000 tokens (1 byte per token) is 10_000_000 + 1_250_000 = 11_250_000 credits;
    // standard: 1000 tokens
    let decision = cascade(&snapshot, PREMIUM, &QuotaRows::new()).unwrap();
    assert_eq!(decision.effective_model, PREMIUM);
    assert_eq!(decision.estimate.estimated_input_tokens, 4000);
    assert_eq!(decision.reserve.reserved_credits_micro, 11_250_000);
}

#[test]
fn the_prior_context_is_part_of_the_estimate_of_the_decision() {
    let decision = cascade_with(
        &snap(vec![premium_model(), standard_model()]),
        STANDARD,
        &QuotaRows::new(),
        &plain_facts(),
        300,
    )
    .unwrap();
    assert_eq!(decision.estimate.prior_context_tokens, 300);
    assert_eq!(decision.estimate.estimated_input_tokens, 1300);
    assert_eq!(decision.reserve.reserve_tokens, 1800);
}

#[test]
fn the_floor_of_the_decision_is_the_smaller_of_the_configured_floor_and_the_output_cap() {
    let decision = cascade(
        &snap(vec![premium_model(), standard_model()]),
        STANDARD,
        &QuotaRows::new(),
    )
    .unwrap();
    assert_eq!(decision.reserve.max_output_tokens_applied, 500);
    assert_eq!(decision.reserve.minimal_generation_floor_applied, 50);
}

// ── exposed capabilities ─────────────────────────────────────────────────────

#[test]
fn the_decision_exposes_the_vision_capability_and_provider_of_the_effective_model() {
    let mut standard = standard_model();
    standard.provider_id = "other_provider".to_owned();
    standard.max_input_tokens = 777;
    let snapshot = snap(vec![premium_model(), standard]);

    let premium = cascade(&snapshot, PREMIUM, &QuotaRows::new()).unwrap();
    assert!(premium.vision_input);
    assert_eq!(premium.provider_id, "azure_openai");

    // after the downgrade the image guard must use the standard model, which has no vision,
    // even though the selected premium model supports images
    let downgraded = cascade(&snapshot, PREMIUM, &design_rows()).unwrap();
    assert_eq!(downgraded.effective_model, STANDARD);
    assert!(!downgraded.vision_input);
    assert_eq!(downgraded.provider_id, "other_provider");
    assert_eq!(downgraded.max_input_tokens, 777);
}
