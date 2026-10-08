#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use chrono::{DateTime, NaiveDate, TimeZone, Utc};
use mini_chat_sdk::{
    EstimationBudgets, KillSwitches, ModelCatalogEntry, ModelPreference, ModelTier,
    ModelToolSupport, PolicySnapshot, TierLimits, UserLimits,
};
use uuid::Uuid;

use super::*;
use crate::domain::model::{Bucket, PeriodType, QuotaScope};
use crate::testing::catalog::{premium_model, standard_model};

const TENANT: Uuid = Uuid::from_u128(1);
const USER: Uuid = Uuid::from_u128(2);
/// Credits of the D§5.10 premium model P for 1000 input + 500 output tokens.
const P_RESERVE: i64 = 3_750_000;
/// Credits of the D§5.10 standard model S for 1000 input + 500 output tokens.
const S_RESERVE: i64 = 1_500_000;

fn now() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 3, 15, 12, 0, 0).unwrap()
}

fn day() -> NaiveDate {
    NaiveDate::from_ymd_opt(2026, 3, 15).unwrap()
}

fn month() -> NaiveDate {
    NaiveDate::from_ymd_opt(2026, 3, 1).unwrap()
}

/// Budgets that make an empty message estimate exactly 1000 input tokens.
fn budgets_1000() -> EstimationBudgets {
    EstimationBudgets {
        bytes_per_token_conservative: 1,
        fixed_overhead_tokens: 1000,
        safety_margin_pct: 0,
        image_token_budget: 0,
        tool_surcharge_tokens: 0,
        web_search_surcharge_tokens: 0,
        code_interpreter_surcharge_tokens: 0,
        minimal_generation_floor: 50,
    }
}

fn with_mult(mut m: ModelCatalogEntry, mult: i64) -> ModelCatalogEntry {
    m.input_tokens_credit_multiplier_micro = mult;
    m.output_tokens_credit_multiplier_micro = mult;
    m.max_output_tokens = 500;
    m.estimation_budgets = budgets_1000();
    m
}

/// D§5.10 premium model P (2.5 credits / 1K tokens).
fn model_p(id: &str) -> ModelCatalogEntry {
    with_mult(premium_model(id), 2_500_000_000)
}

/// D§5.10 standard model S (1 credit / 1K tokens).
fn model_s(id: &str) -> ModelCatalogEntry {
    with_mult(standard_model(id), 1_000_000_000)
}

fn all_tools() -> ModelToolSupport {
    ModelToolSupport {
        web_search: true,
        file_search: true,
        image_generation: false,
        code_interpreter: true,
        mcp: false,
    }
}

fn no_ks() -> KillSwitches {
    KillSwitches {
        disable_premium_tier: false,
        force_standard_tier: false,
        disable_web_search: false,
        disable_file_search: false,
        disable_images: false,
        disable_code_interpreter: false,
    }
}

fn snapshot(models: Vec<ModelCatalogEntry>, ks: KillSwitches) -> Arc<PolicySnapshot> {
    Arc::new(PolicySnapshot {
        policy_version: 7,
        model_catalog: models,
        kill_switches: ks,
    })
}

/// D§5.10 limits: total 60M/600M, premium 22M/300M.
fn limits() -> UserLimits {
    UserLimits {
        user_id: USER,
        policy_version: 7,
        standard: TierLimits {
            limit_daily_credits_micro: 60_000_000,
            limit_monthly_credits_micro: 600_000_000,
        },
        premium: TierLimits {
            limit_daily_credits_micro: 22_000_000,
            limit_monthly_credits_micro: 300_000_000,
        },
    }
}

fn input(selected: &str, snap: Arc<PolicySnapshot>) -> PreflightInput {
    PreflightInput {
        tenant_id: TENANT,
        user_id: USER,
        selected_model_id: selected.to_owned(),
        snapshot: snap,
        user_limits: limits(),
        content: String::new(),
        image_count: 0,
        prior_context_tokens: 0,
        chat_has_ready_docs: false,
        chat_has_ready_xlsx: false,
        web_search_requested: false,
        now: now(),
    }
}

fn row(bucket: Bucket, period: PeriodType, spent: i64, reserved: i64) -> quota_usage::Model {
    quota_usage::Model {
        id: Uuid::new_v4(),
        tenant_id: TENANT,
        user_id: USER,
        period_type: period.as_str().to_owned(),
        period_start: match period {
            PeriodType::Daily => day(),
            PeriodType::Monthly => month(),
        },
        bucket: bucket.as_str().to_owned(),
        spent_credits_micro: spent,
        reserved_credits_micro: reserved,
        calls: 0,
        input_tokens: 0,
        output_tokens: 0,
        file_search_calls: 0,
        web_search_calls: 0,
        code_interpreter_calls: 0,
        rag_retrieval_calls: 0,
        image_inputs: 0,
        image_upload_bytes: 0,
        updated_at: None,
    }
}

fn cfg() -> MiniChatConfig {
    MiniChatConfig::default()
}

fn exceeded_scope(r: DomainResult<PreflightDecision>) -> QuotaScope {
    match r {
        Err(DomainError::QuotaExceeded { scope }) => scope,
        other => panic!("expected QuotaExceeded, got {other:?}"),
    }
}

#[test]
fn cascade_allows_selected_premium_without_rows() {
    let snap = snapshot(vec![model_p("P"), model_s("S")], no_ks());
    let d = decide(&input("P", snap), &cfg(), &[]).unwrap();
    assert_eq!(d.effective.id, "P");
    assert_eq!(d.effective_tier, ModelTier::Premium);
    assert_eq!(d.decision, QuotaDecision::Allow);
    assert_eq!(d.downgrade_reason, None);
    assert_eq!(d.estimated_input_tokens, 1000);
    assert_eq!(d.max_output_tokens_applied, 500);
    assert_eq!(d.reserve_tokens, 1500);
    assert_eq!(d.reserved_credits_micro, P_RESERVE);
    assert_eq!(d.minimal_generation_floor_applied, 50);
    assert_eq!(d.policy_version, 7);
    assert_eq!((d.daily_start, d.monthly_start), (day(), month()));
}

#[test]
fn max_output_is_capped_by_streaming_config_and_floor_by_max_output() {
    let mut c = cfg();
    c.streaming.max_output_tokens = 40;
    c.estimation_budgets.minimal_generation_floor = 40;
    let mut p = model_p("P");
    p.max_output_tokens = 30;
    let snap = snapshot(vec![p.clone()], no_ks());
    let inp = input("P", snap);
    let (est, max_out, credits) = candidate_reserve(&p, &inp, &c, &no_ks());
    assert_eq!((est, max_out), (1000, 30));
    assert_eq!(credits, 2_500_000 + 75_000);
    let d = decide(&inp, &c, &[]).unwrap();
    assert_eq!(d.max_output_tokens_applied, 30);
    assert_eq!(d.minimal_generation_floor_applied, 30);
}

#[test]
fn cascade_premium_bucket_daily_exhausted_downgrades() {
    // D§5.10: tier:premium day 20M + 3.75M > 22M; S fits the total bucket.
    let rows = [
        row(Bucket::TierPremium, PeriodType::Daily, 20_000_000, 0),
        row(Bucket::TierPremium, PeriodType::Monthly, 200_000_000, 0),
        row(Bucket::Total, PeriodType::Daily, 25_000_000, 0),
        row(Bucket::Total, PeriodType::Monthly, 240_000_000, 0),
    ];
    let snap = snapshot(vec![model_p("P"), model_s("S")], no_ks());
    let d = decide(&input("P", snap), &cfg(), &rows).unwrap();
    assert_eq!(d.effective.id, "S");
    assert_eq!(d.effective_tier, ModelTier::Standard);
    assert_eq!(d.decision, QuotaDecision::Downgrade);
    assert_eq!(
        d.downgrade_reason,
        Some(DowngradeReason::PremiumQuotaExhausted)
    );
    assert_eq!(d.reserved_credits_micro, S_RESERVE);
}

#[test]
fn cascade_monthly_exhausted_also_downgrades() {
    // Daily fine everywhere; the premium monthly subcap cannot take 3.75M.
    let rows = [row(
        Bucket::TierPremium,
        PeriodType::Monthly,
        297_000_000,
        0,
    )];
    let snap = snapshot(vec![model_p("P"), model_s("S")], no_ks());
    let d = decide(&input("P", snap.clone()), &cfg(), &rows).unwrap();
    assert_eq!(d.effective.id, "S");
    assert_eq!(
        d.downgrade_reason,
        Some(DowngradeReason::PremiumQuotaExhausted)
    );

    // Reserved credits count like spent ones; the total bucket also gates premium.
    let rows = [row(
        Bucket::Total,
        PeriodType::Monthly,
        590_000_000,
        7_000_000,
    )];
    let d = decide(&input("P", snap), &cfg(), &rows).unwrap();
    assert_eq!(d.effective.id, "S");
    assert_eq!(d.reserved_credits_micro, S_RESERVE);
}

#[test]
fn cascade_limit_is_inclusive() {
    let rows = [row(
        Bucket::TierPremium,
        PeriodType::Daily,
        22_000_000 - P_RESERVE,
        0,
    )];
    let snap = snapshot(vec![model_p("P"), model_s("S")], no_ks());
    let d = decide(&input("P", snap), &cfg(), &rows).unwrap();
    assert_eq!(d.effective.id, "P");
    assert_eq!(d.decision, QuotaDecision::Allow);
}

#[test]
fn cascade_all_exhausted_is_quota_exceeded_tokens() {
    let rows = [row(Bucket::Total, PeriodType::Daily, 59_000_000, 0)];
    let snap = snapshot(vec![model_p("P"), model_s("S")], no_ks());
    assert_eq!(
        exceeded_scope(decide(&input("P", snap), &cfg(), &rows)),
        QuotaScope::Tokens
    );
}

#[test]
fn force_standard_tier_and_disable_premium_reasons() {
    for (ks, reason) in [
        (
            KillSwitches {
                force_standard_tier: true,
                ..no_ks()
            },
            DowngradeReason::ForceStandardTier,
        ),
        (
            KillSwitches {
                disable_premium_tier: true,
                ..no_ks()
            },
            DowngradeReason::DisablePremiumTier,
        ),
    ] {
        let snap = snapshot(vec![model_p("P"), model_s("S")], ks);
        let d = decide(&input("P", snap), &cfg(), &[]).unwrap();
        assert_eq!(d.effective.id, "S");
        assert_eq!(d.decision, QuotaDecision::Downgrade);
        assert_eq!(d.downgrade_reason, Some(reason));
        assert_eq!(d.reserved_credits_micro, S_RESERVE);
    }
}

#[test]
fn cascade_disabled_selected_model_downgrades_with_model_disabled() {
    // A disabled premium model falls back to another enabled premium model.
    let mut p = model_p("P");
    p.enabled = false;
    let snap = snapshot(vec![p, model_p("P2"), model_s("S")], no_ks());
    let d = decide(&input("P", snap.clone()), &cfg(), &[]).unwrap();
    assert_eq!(d.effective.id, "P2");
    assert_eq!(d.decision, QuotaDecision::Downgrade);
    assert_eq!(d.downgrade_reason, Some(DowngradeReason::ModelDisabled));

    // A model missing from the catalog starts at premium with `model_disabled`.
    let d = decide(&input("gone", snap), &cfg(), &[]).unwrap();
    assert_eq!(d.effective.id, "P2");
    assert_eq!(d.downgrade_reason, Some(DowngradeReason::ModelDisabled));

    // The `model_disabled` reason wins over a later premium exhaustion.
    let mut p = model_p("P");
    p.enabled = false;
    let snap = snapshot(vec![p, model_p("P2"), model_s("S")], no_ks());
    let rows = [row(Bucket::TierPremium, PeriodType::Daily, 21_000_000, 0)];
    let d = decide(&input("P", snap), &cfg(), &rows).unwrap();
    assert_eq!(d.effective.id, "S");
    assert_eq!(d.downgrade_reason, Some(DowngradeReason::ModelDisabled));
}

#[test]
fn standard_never_upgrades() {
    let mut s = model_s("S");
    s.enabled = false;
    let snap = snapshot(vec![model_p("P"), s], no_ks());
    assert_eq!(
        exceeded_scope(decide(&input("S", snap), &cfg(), &[])),
        QuotaScope::Tokens
    );

    let rows = [row(Bucket::Total, PeriodType::Daily, 59_000_000, 0)];
    let snap = snapshot(vec![model_p("P"), model_s("S")], no_ks());
    assert_eq!(
        exceeded_scope(decide(&input("S", snap), &cfg(), &rows)),
        QuotaScope::Tokens
    );
}

#[test]
fn tier_without_enabled_model_skipped() {
    let mut p = model_p("P");
    p.enabled = false;
    let snap = snapshot(vec![p, model_s("S")], no_ks());
    let d = decide(&input("P", snap), &cfg(), &[]).unwrap();
    assert_eq!(d.effective.id, "S");
    assert_eq!(d.downgrade_reason, Some(DowngradeReason::ModelDisabled));
}

#[test]
fn candidate_prefers_selected_then_default_then_catalog_order() {
    let mut s_default = model_s("S-default");
    s_default.preference = Some(ModelPreference {
        is_default: true,
        sort_order: 0,
    });
    let snap = snapshot(
        vec![
            model_p("P"),
            model_s("S-first"),
            s_default,
            model_s("S-picked"),
        ],
        KillSwitches {
            force_standard_tier: true,
            ..no_ks()
        },
    );
    let d = decide(&input("P", snap.clone()), &cfg(), &[]).unwrap();
    assert_eq!(d.effective.id, "S-default");
    let d = decide(&input("S-picked", snap), &cfg(), &[]).unwrap();
    assert_eq!(d.effective.id, "S-picked");
    assert_eq!(d.decision, QuotaDecision::Allow);

    let snap = snapshot(
        vec![model_p("P"), model_s("S-first"), model_s("S-second")],
        KillSwitches {
            force_standard_tier: true,
            ..no_ks()
        },
    );
    let d = decide(&input("P", snap), &cfg(), &[]).unwrap();
    assert_eq!(d.effective.id, "S-first");
}

#[test]
fn candidate_with_invalid_multiplier_is_unavailable() {
    let mut p = model_p("P");
    p.input_tokens_credit_multiplier_micro = 0;
    let snap = snapshot(vec![p.clone(), model_s("S")], no_ks());
    let inp = input("P", snap);
    let (_, _, credits) = candidate_reserve(&p, &inp, &cfg(), &no_ks());
    assert_eq!(credits, i64::MAX);
    let d = decide(&inp, &cfg(), &[]).unwrap();
    assert_eq!(d.effective.id, "S");
    assert_eq!(
        d.downgrade_reason,
        Some(DowngradeReason::PremiumQuotaExhausted)
    );
}

#[test]
fn uncomputable_reserve_is_unavailable_even_under_an_unbounded_limit() {
    let mut p = model_p("P");
    p.input_tokens_credit_multiplier_micro = 0;
    let snap = snapshot(vec![p, model_s("S")], no_ks());
    let mut inp = input("P", snap);
    let unbounded = TierLimits {
        limit_daily_credits_micro: i64::MAX,
        limit_monthly_credits_micro: i64::MAX,
    };
    inp.user_limits.standard = unbounded;
    inp.user_limits.premium = unbounded;
    let d = decide(&inp, &cfg(), &[]).unwrap();
    assert_eq!(d.effective.id, "S");
    assert_eq!(
        d.downgrade_reason,
        Some(DowngradeReason::PremiumQuotaExhausted)
    );
}

fn tool_row(web: i32, ci: i32) -> quota_usage::Model {
    let mut r = row(Bucket::Total, PeriodType::Daily, 0, 0);
    r.web_search_calls = web;
    r.code_interpreter_calls = ci;
    r
}

#[test]
fn web_search_daily_quota_checked_only_when_requested_and_supported() {
    let mut p = model_p("P");
    p.general_config.tool_support = all_tools();
    let snap = snapshot(vec![p, model_s("S")], no_ks());
    let rows = [tool_row(75, 0)];

    let mut inp = input("P", snap.clone());
    inp.web_search_requested = true;
    assert_eq!(
        exceeded_scope(decide(&inp, &cfg(), &rows)),
        QuotaScope::WebSearch
    );
    // Below the quota it passes and the tool is sent.
    let d = decide(&inp, &cfg(), &[tool_row(74, 0)]).unwrap();
    assert!(d.send_web_search);

    // Not requested: never checked.
    let d = decide(&input("P", snap), &cfg(), &rows).unwrap();
    assert!(!d.send_web_search);

    // Requested on a model without web search: not checked, not sent.
    let snap = snapshot(vec![model_p("P"), model_s("S")], no_ks());
    let mut inp = input("P", snap);
    inp.web_search_requested = true;
    let d = decide(&inp, &cfg(), &rows).unwrap();
    assert!(!d.send_web_search);
}

#[test]
fn code_interpreter_daily_quota_only_with_ready_xlsx() {
    let mut p = model_p("P");
    p.general_config.tool_support = all_tools();
    let rows = [tool_row(0, 50)];

    let snap = snapshot(vec![p.clone(), model_s("S")], no_ks());
    let d = decide(&input("P", snap.clone()), &cfg(), &rows).unwrap();
    assert!(!d.send_code_interpreter);

    let mut inp = input("P", snap);
    inp.chat_has_ready_xlsx = true;
    assert_eq!(
        exceeded_scope(decide(&inp, &cfg(), &rows)),
        QuotaScope::CodeInterpreter
    );
    let d = decide(&inp, &cfg(), &[tool_row(0, 49)]).unwrap();
    assert!(d.send_code_interpreter);

    // Kill switch: tool not sent, quota not checked.
    let snap = snapshot(
        vec![p, model_s("S")],
        KillSwitches {
            disable_code_interpreter: true,
            ..no_ks()
        },
    );
    let mut inp = input("P", snap);
    inp.chat_has_ready_xlsx = true;
    let d = decide(&inp, &cfg(), &rows).unwrap();
    assert!(!d.send_code_interpreter);
}

#[test]
fn surcharges_follow_candidate_tool_support_and_kill_switches() {
    let budgets = EstimationBudgets {
        image_token_budget: 300,
        tool_surcharge_tokens: 500,
        web_search_surcharge_tokens: 700,
        code_interpreter_surcharge_tokens: 1100,
        ..budgets_1000()
    };
    let mut tools = model_p("P");
    tools.estimation_budgets = budgets.clone();
    tools.general_config.tool_support = all_tools();
    let mut plain = model_s("S");
    plain.estimation_budgets = budgets;

    let snap = snapshot(vec![tools.clone(), plain.clone()], no_ks());
    let mut inp = input("P", snap);
    inp.image_count = 2;
    inp.prior_context_tokens = 40;
    inp.chat_has_ready_docs = true;
    inp.chat_has_ready_xlsx = true;
    inp.web_search_requested = true;

    let base = 1000 + 40 + 2 * 300;
    let (est, max_out, credits) = candidate_reserve(&tools, &inp, &cfg(), &no_ks());
    assert_eq!(est, base + 500 + 700 + 1100);
    assert_eq!(max_out, 500);
    assert_eq!(
        credits,
        crate::domain::credits::credits_micro(est, 500, 2_500_000_000, 2_500_000_000).unwrap()
    );
    assert_eq!(candidate_reserve(&plain, &inp, &cfg(), &no_ks()).0, base);

    let ks = KillSwitches {
        disable_file_search: true,
        disable_code_interpreter: true,
        ..no_ks()
    };
    assert_eq!(candidate_reserve(&tools, &inp, &cfg(), &ks).0, base + 700);

    // Without the feature inputs no surcharge applies.
    let mut no_features = inp.clone();
    no_features.chat_has_ready_docs = false;
    no_features.chat_has_ready_xlsx = false;
    no_features.web_search_requested = false;
    assert_eq!(
        candidate_reserve(&tools, &no_features, &cfg(), &no_ks()).0,
        base
    );

    // The decision books the effective model's estimate and its tool flags.
    let d = decide(&inp, &cfg(), &[]).unwrap();
    assert_eq!(d.estimated_input_tokens, base + 500 + 700 + 1100);
    assert!(d.send_file_search && d.send_web_search && d.send_code_interpreter);
    let snap = snapshot(
        vec![tools, plain],
        KillSwitches {
            force_standard_tier: true,
            ..no_ks()
        },
    );
    let mut down = inp;
    down.snapshot = snap;
    let d = decide(&down, &cfg(), &[]).unwrap();
    assert_eq!(d.effective.id, "S");
    assert_eq!(d.estimated_input_tokens, base);
    assert_eq!(d.reserve_tokens, base + 500);
    assert!(!d.send_file_search && !d.send_web_search && !d.send_code_interpreter);
}

#[test]
fn rows_of_other_periods_are_ignored() {
    let mut old = row(Bucket::Total, PeriodType::Daily, 59_000_000, 0);
    old.period_start = NaiveDate::from_ymd_opt(2026, 3, 14).unwrap();
    let snap = snapshot(vec![model_p("P"), model_s("S")], no_ks());
    let d = decide(&input("P", snap), &cfg(), &[old]).unwrap();
    assert_eq!(d.effective.id, "P");
}

#[test]
fn overshoot_within_tolerance_charges_actual() {
    let s = model_s("S");
    // 10500 / 10000 = 1.05 <= 1.10.
    let (committed, capped) = committed_credits(10_000, 500, 10_000, 9_999, 1.10, &s).unwrap();
    assert_eq!(committed, 10_500_000);
    assert!(!capped);
    // Below the reserve the actual credits are charged.
    let (committed, capped) = committed_credits(900, 300, 1500, 1_500_000, 1.10, &s).unwrap();
    assert_eq!((committed, capped), (1_200_000, false));
}

#[test]
fn overshoot_beyond_tolerance_caps_at_reserved() {
    let s = model_s("S");
    // 11500 / 10000 = 1.15 > 1.10.
    let (committed, capped) = committed_credits(11_000, 500, 10_000, 2_500_000, 1.10, &s).unwrap();
    assert_eq!(committed, 2_500_000);
    assert!(capped);
}

#[test]
fn committed_credits_rejects_out_of_range_usage() {
    let s = model_s("S");
    assert!(matches!(
        committed_credits(10_000_001, 0, 10, 10, 1.10, &s),
        Err(DomainError::Internal(_))
    ));
}

#[test]
fn estimated_credits_formula() {
    let s = model_s("S");
    // credits_micro(1500 - 500, 50) = 1_000_000 + 50_000.
    assert_eq!(estimated_credits(1500, 500, 50, &s).unwrap(), 1_050_000);
    let mut odd = model_s("S");
    odd.input_tokens_credit_multiplier_micro = 3;
    odd.output_tokens_credit_multiplier_micro = 7;
    // Per-component ceil: ceil(1000*3/1e6) + ceil(50*7/1e6) = 1 + 1.
    assert_eq!(estimated_credits(1500, 500, 50, &odd).unwrap(), 2);
}
