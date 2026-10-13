#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use mini_chat_sdk::{EstimationBudgets, KillSwitches, ModelCatalogEntry, TierLimits, UsageTokens};
use time::macros::date;

use super::store::{self, Bucket, PeriodKind, RowDelta, UsageRows};
use super::*;
use crate::domain::service::test_support::{
    DenyPdp, TENANT_A, TestEnv, TestOptions, USER_A1, ctx_a1, default_catalog, model,
};
use crate::infra::db::entity::quota_usage;

// ── helpers ────────────────────────────────────────────────────────────────

fn input(selected: &str) -> PreflightInput {
    PreflightInput {
        tenant_id: TENANT_A,
        user_id: USER_A1,
        selected_model: selected.to_owned(),
        message_bytes: 0,
        image_count: 0,
        prior_context_tokens: 0,
        web_search_requested: false,
        has_ready_documents: false,
        has_ready_code_interpreter_files: false,
    }
}

fn exact_budgets() -> EstimationBudgets {
    EstimationBudgets {
        bytes_per_token_conservative: 1,
        fixed_overhead_tokens: 0,
        safety_margin_pct: 0,
        image_token_budget: 1000,
        tool_surcharge_tokens: 500,
        web_search_surcharge_tokens: 700,
        code_interpreter_surcharge_tokens: 1100,
        minimal_generation_floor: 50,
    }
}

fn subject(err: &DomainError) -> String {
    match err {
        DomainError::ResourceExhausted { subject, description, .. } => {
            assert_eq!(description, "quota_exceeded");
            subject.clone()
        }
        other => panic!("expected 429 quota_exceeded, got {other:?}"),
    }
}

fn today() -> QuotaPeriods {
    QuotaPeriods::of(OffsetDateTime::now_utc())
}

async fn seed(env: &TestEnv, periods: QuotaPeriods, period: PeriodKind, bucket: Bucket, d: RowDelta) {
    env.deps
        .db
        .transaction(move |tx| {
            Box::pin(async move {
                store::upsert_delta(
                    tx,
                    TENANT_A,
                    USER_A1,
                    period,
                    period.start(&periods),
                    bucket,
                    d,
                    OffsetDateTime::now_utc(),
                )
                .await
            })
        })
        .await
        .unwrap();
}

async fn spent(env: &TestEnv, period: PeriodKind, bucket: Bucket, spent: i64) {
    seed(env, today(), period, bucket, RowDelta { spent, ..RowDelta::default() }).await;
}

async fn rows(env: &TestEnv, periods: &QuotaPeriods) -> UsageRows {
    let conn = env.deps.db.conn().unwrap();
    store::load_rows(&conn, &store::user_scope(TENANT_A, USER_A1), TENANT_A, USER_A1, periods)
        .await
        .unwrap()
}

async fn row(env: &TestEnv, periods: &QuotaPeriods, p: PeriodKind, b: Bucket) -> quota_usage::Model {
    rows(env, periods).await.get(periods, p, b).cloned().expect("row")
}

async fn reserve(env: &TestEnv, decision: &PreflightDecision) -> Result<(), DomainError> {
    let quota = Arc::clone(&env.services.quota);
    let decision = decision.clone();
    env.deps
        .db
        .transaction(move |tx| Box::pin(async move { quota.reserve_in_tx(tx, TENANT_A, USER_A1, &decision).await }))
        .await
}

async fn settle(env: &TestEnv, input: &SettlementInput) -> Result<SettlementResult, DomainError> {
    let quota = Arc::clone(&env.services.quota);
    let input = input.clone();
    env.deps
        .db
        .transaction(move |tx| Box::pin(async move { quota.settle_in_tx(tx, &input).await }))
        .await
}

fn settlement(decision: &PreflightDecision, method: SettlementMethod, usage: Option<UsageTokens>) -> SettlementInput {
    SettlementInput {
        tenant_id: TENANT_A,
        user_id: USER_A1,
        effective_model: decision.effective.id.clone(),
        policy_version: decision.policy_version,
        reserve_tokens: decision.reserve_tokens,
        max_output_tokens_applied: i64::from(decision.max_output_tokens_applied),
        reserved_credits_micro: decision.reserved_credits_micro,
        minimal_generation_floor_applied: i64::from(decision.minimal_generation_floor_applied),
        periods: decision.periods,
        method,
        usage,
        web_search_calls: 2,
        code_interpreter_calls: 1,
    }
}

fn usage(i: i64, o: i64) -> Option<UsageTokens> {
    Some(UsageTokens {
        input_tokens: i,
        output_tokens: o,
        ..UsageTokens::default()
    })
}

fn limits(sd: i64, sm: i64, pd: i64, pm: i64) -> UserLimits {
    UserLimits {
        user_id: USER_A1,
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

// ── credit arithmetic ──────────────────────────────────────────────────────

#[test]
fn credits_per_component_rounding() {
    assert_eq!(credits_micro(100, 50, 1_000_000, 3_000_000), Ok(250));
    // ceil(1/1e6) + ceil(1/1e6) = 2, not ceil(2/1e6) = 1
    assert_eq!(credits_micro(1, 1, 1, 1), Ok(2));
    assert_eq!(credits_micro(0, 0, 1, 1), Ok(0));
    assert_eq!(credits_micro(1_000, 500, 2_500_000_000, 2_500_000_000), Ok(3_750_000));
    assert_eq!(credits_micro(3, 0, 333_333, 1), Ok(1)); // 999_999 / 1e6 → 1
    assert_eq!(credits_micro(4, 0, 333_333, 1), Ok(2)); // 1_333_332 / 1e6 → 2
    // bounds are inclusive and the maximum product does not overflow
    assert_eq!(
        credits_micro(10_000_000, 10_000_000, 10_000_000_000, 10_000_000_000),
        Ok(200_000_000_000)
    );
}

#[test]
fn credits_errors_are_distinct() {
    assert_eq!(credits_micro(1, 1, 0, 1), Err(CreditError::ZeroMultiplier));
    assert_eq!(credits_micro(1, 1, 1, 0), Err(CreditError::ZeroMultiplier));
    assert_eq!(
        credits_micro(1, 1, 10_000_000_001, 1),
        Err(CreditError::InvalidMultiplier(10_000_000_001))
    );
    assert_eq!(credits_micro(1, 1, 1, -5), Err(CreditError::InvalidMultiplier(-5)));
    assert_eq!(
        credits_micro(10_000_001, 0, 1, 1),
        Err(CreditError::InvalidTokenCount(10_000_001))
    );
    assert_eq!(credits_micro(0, -1, 1, 1), Err(CreditError::InvalidTokenCount(-1)));
}

// ── estimation ─────────────────────────────────────────────────────────────

#[test]
fn text_estimate_formula() {
    let b = EstimationBudgets::default(); // 4 bytes/token, overhead 100, margin 10 %
    // ceil(10/4)=3; (3+100)*110/100 = 113.3 → 114
    assert_eq!(estimate_text_tokens(10, &b), 114);
    // empty message: overhead with margin
    assert_eq!(estimate_text_tokens(0, &b), 110);
    let mut z = b.clone();
    z.bytes_per_token_conservative = 0; // clamped to 1
    assert_eq!(estimate_text_tokens(10, &z), 121);
    assert_eq!(estimate_text_tokens(1000, &exact_budgets()), 1000);
}

#[test]
fn candidate_reserve_surcharges_and_caps() {
    let mut m = model("m", "standard");
    m.estimation_budgets = exact_budgets();
    m.max_output_tokens = 4096;
    let kill = KillSwitches::default();
    let mut i = input("m");
    i.message_bytes = 100;
    i.prior_context_tokens = 40;
    i.image_count = 2;
    let r = candidate_reserve(&m, &i, &kill, 1000, 50).unwrap();
    assert_eq!(r.estimated_input_tokens, 100 + 40 + 2000);
    assert_eq!(r.max_output_tokens_applied, 1000); // min(4096, streaming 1000)
    assert_eq!(r.reserve_tokens, 3140);
    assert_eq!(r.minimal_generation_floor_applied, 50);
    assert_eq!(r.reserved_credits_micro, credits_micro(2140, 1000, 1_000_000, 3_000_000).unwrap());
    assert_eq!(r.tools, ToolGates::default());

    i.web_search_requested = true;
    i.has_ready_documents = true;
    i.has_ready_code_interpreter_files = true;
    let r = candidate_reserve(&m, &i, &kill, 30, 50).unwrap();
    assert_eq!(r.estimated_input_tokens, 2140 + 500 + 700 + 1100);
    assert_eq!(r.max_output_tokens_applied, 30);
    assert_eq!(r.minimal_generation_floor_applied, 30, "floor = min(floor, max_output_applied)");
    assert_eq!(
        r.tools,
        ToolGates {
            web_search: true,
            file_search: true,
            code_interpreter: true
        }
    );

    // kill switches and tool support gate the surcharges per candidate
    let kill = KillSwitches {
        disable_file_search: true,
        disable_code_interpreter: true,
        ..KillSwitches::default()
    };
    m.general_config.tool_support.web_search = false;
    let r = candidate_reserve(&m, &i, &kill, 1000, 50).unwrap();
    assert_eq!(r.estimated_input_tokens, 2140);
    assert_eq!(r.tools, ToolGates::default());

    // a zero multiplier makes the reserve uncomputable
    m.input_tokens_credit_multiplier_micro = 0;
    assert_eq!(
        candidate_reserve(&m, &i, &kill, 1000, 50),
        Err(CreditError::ZeroMultiplier)
    );
}

// ── DESIGN §5.10 worked example ────────────────────────────────────────────

fn example_options() -> TestOptions {
    let mut p = model("model-p", "premium");
    p.input_tokens_credit_multiplier_micro = 2_500_000_000;
    p.output_tokens_credit_multiplier_micro = 2_500_000_000;
    p.max_output_tokens = 500;
    p.estimation_budgets = exact_budgets();
    let mut s = model("model-s", "standard");
    s.input_tokens_credit_multiplier_micro = 1_000_000_000;
    s.output_tokens_credit_multiplier_micro = 1_000_000_000;
    s.max_output_tokens = 500;
    s.estimation_budgets = exact_budgets();
    TestOptions {
        catalog: vec![p, s],
        standard_limits: TierLimits {
            limit_daily_credits_micro: 60_000_000,
            limit_monthly_credits_micro: 600_000_000,
        },
        premium_limits: TierLimits {
            limit_daily_credits_micro: 22_000_000,
            limit_monthly_credits_micro: 300_000_000,
        },
        ..TestOptions::default()
    }
}

#[tokio::test]
async fn design_5_10_worked_example() {
    let env = TestEnv::new(example_options()).await;
    spent(&env, PeriodKind::Daily, Bucket::Premium, 20_000_000).await;
    spent(&env, PeriodKind::Monthly, Bucket::Premium, 200_000_000).await;
    spent(&env, PeriodKind::Daily, Bucket::Total, 25_000_000).await;
    spent(&env, PeriodKind::Monthly, Bucket::Total, 240_000_000).await;

    let mut i = input("model-p");
    i.message_bytes = 1000;
    let d = env.services.quota.preflight(&i).await.unwrap();
    assert_eq!(d.effective.id, "model-s");
    assert_eq!(d.tier, ModelTier::Standard);
    assert_eq!(d.downgrade_reason, Some(DowngradeReason::PremiumQuotaExhausted));
    assert!(d.is_downgrade());
    assert_eq!(d.estimated_input_tokens, 1000);
    assert_eq!(d.max_output_tokens_applied, 500);
    assert_eq!(d.reserve_tokens, 1500);
    assert_eq!(d.reserved_credits_micro, 1_500_000);
    assert_eq!(d.minimal_generation_floor_applied, 50);
    assert_eq!(d.selected_model, "model-p");
    assert_eq!(d.policy_version, 1);
    assert_eq!(d.periods, today());

    reserve(&env, &d).await.unwrap();
    let p = d.periods;
    assert_eq!(row(&env, &p, PeriodKind::Daily, Bucket::Total).await.reserved_credits_micro, 1_500_000);
    assert_eq!(row(&env, &p, PeriodKind::Monthly, Bucket::Total).await.reserved_credits_micro, 1_500_000);
    assert_eq!(row(&env, &p, PeriodKind::Daily, Bucket::Premium).await.reserved_credits_micro, 0);

    let mut s = settlement(&d, SettlementMethod::Actual, usage(900, 300));
    s.web_search_calls = 0;
    s.code_interpreter_calls = 0;
    let res = settle(&env, &s).await.unwrap();
    assert_eq!(res.committed_credits_micro, 1_200_000);
    assert!(!res.overshoot_capped);
    let day = row(&env, &p, PeriodKind::Daily, Bucket::Total).await;
    let month = row(&env, &p, PeriodKind::Monthly, Bucket::Total).await;
    assert_eq!((day.spent_credits_micro, day.reserved_credits_micro), (26_200_000, 0));
    assert_eq!((month.spent_credits_micro, month.reserved_credits_micro), (241_200_000, 0));
    assert_eq!((day.calls, day.input_tokens, day.output_tokens), (1, 900, 300));
    let prem = row(&env, &p, PeriodKind::Daily, Bucket::Premium).await;
    assert_eq!((prem.spent_credits_micro, prem.calls), (20_000_000, 0));
    env.shutdown().await;
}

// ── cascade branches ───────────────────────────────────────────────────────

#[tokio::test]
async fn allow_on_selected_premium() {
    let env = TestEnv::default_env().await;
    let d = env.services.quota.preflight(&input("gpt-premium")).await.unwrap();
    assert_eq!(d.effective.id, "gpt-premium");
    assert_eq!(d.tier, ModelTier::Premium);
    assert_eq!(d.downgrade_reason, None);
    assert!(d.vision_supported);
    reserve(&env, &d).await.unwrap();
    let p = d.periods;
    for period in PeriodKind::ALL {
        for b in [Bucket::Total, Bucket::Premium] {
            assert_eq!(row(&env, &p, period, b).await.reserved_credits_micro, d.reserved_credits_micro);
        }
    }
    env.shutdown().await;
}

async fn env_with_kill(kill: KillSwitches) -> TestEnv {
    TestEnv::new(TestOptions {
        kill_switches: kill,
        ..TestOptions::default()
    })
    .await
}

#[tokio::test]
async fn force_standard_tier_downgrades() {
    let env = env_with_kill(KillSwitches {
        force_standard_tier: true,
        disable_premium_tier: true,
        ..KillSwitches::default()
    })
    .await;
    let d = env.services.quota.preflight(&input("gpt-premium")).await.unwrap();
    assert_eq!(d.effective.id, "gpt-standard");
    assert_eq!(d.downgrade_reason, Some(DowngradeReason::ForceStandardTier));
    env.shutdown().await;
}

#[tokio::test]
async fn disable_premium_tier_downgrades() {
    let env = env_with_kill(KillSwitches {
        disable_premium_tier: true,
        ..KillSwitches::default()
    })
    .await;
    let d = env.services.quota.preflight(&input("gpt-premium")).await.unwrap();
    assert_eq!(d.effective.id, "gpt-standard");
    assert_eq!(d.downgrade_reason, Some(DowngradeReason::DisablePremiumTier));
    assert_eq!(d.downgrade_reason.unwrap().as_str(), "disable_premium_tier");
    env.shutdown().await;
}

#[tokio::test]
async fn disabled_model_uses_tier_candidate() {
    let env = TestEnv::default_env().await;
    // gpt-disabled is a disabled standard model → standard tier, first enabled standard model
    let d = env.services.quota.preflight(&input("gpt-disabled")).await.unwrap();
    assert_eq!(d.effective.id, "gpt-standard");
    assert_eq!(d.downgrade_reason, Some(DowngradeReason::ModelDisabled));
    env.shutdown().await;
}

#[tokio::test]
async fn missing_model_starts_at_premium_default() {
    let env = TestEnv::default_env().await;
    let d = env.services.quota.preflight(&input("no-such-model")).await.unwrap();
    assert_eq!(d.effective.id, "gpt-premium");
    assert_eq!(d.tier, ModelTier::Premium);
    assert_eq!(d.downgrade_reason, Some(DowngradeReason::ModelDisabled));
    env.shutdown().await;
}

#[tokio::test]
async fn missing_model_keeps_first_reason_on_premium_exhaustion() {
    let env = TestEnv::default_env().await;
    spent(&env, PeriodKind::Monthly, Bucket::Premium, 500_000_000).await;
    let d = env.services.quota.preflight(&input("no-such-model")).await.unwrap();
    assert_eq!(d.effective.id, "gpt-standard");
    assert_eq!(d.downgrade_reason, Some(DowngradeReason::ModelDisabled));
    env.shutdown().await;
}

#[tokio::test]
async fn premium_monthly_exhaustion_downgrades() {
    let env = TestEnv::default_env().await;
    // daily fine, monthly premium full → premium unavailable
    spent(&env, PeriodKind::Monthly, Bucket::Premium, 499_999_999).await;
    let d = env.services.quota.preflight(&input("gpt-premium")).await.unwrap();
    assert_eq!(d.effective.id, "gpt-standard");
    assert_eq!(d.downgrade_reason, Some(DowngradeReason::PremiumQuotaExhausted));
    assert_eq!(d.downgrade_reason.unwrap().as_str(), "premium_quota_exhausted");
    env.shutdown().await;
}

#[tokio::test]
async fn all_tiers_exhausted_rejects_with_tokens() {
    let env = TestEnv::default_env().await;
    spent(&env, PeriodKind::Daily, Bucket::Total, 100_000_000).await;
    let err = env.services.quota.preflight(&input("gpt-premium")).await.unwrap_err();
    assert_eq!(subject(&err), "tokens");
    let err = env.services.quota.preflight(&input("gpt-standard")).await.unwrap_err();
    assert_eq!(subject(&err), "tokens");
    env.shutdown().await;
}

#[tokio::test]
async fn reserved_credits_count_against_availability() {
    let env = TestEnv::default_env().await;
    seed(
        &env,
        today(),
        PeriodKind::Monthly,
        Bucket::Total,
        RowDelta {
            reserved: 1_000_000_000,
            ..RowDelta::default()
        },
    )
    .await;
    let err = env.services.quota.preflight(&input("gpt-standard")).await.unwrap_err();
    assert_eq!(subject(&err), "tokens");
    env.shutdown().await;
}

#[test]
fn standard_never_upgrades_and_uncomputable_reserve_is_unavailable() {
    let mut catalog = default_catalog();
    // standard candidate with a zero multiplier: its reserve cannot be computed
    catalog[1].output_tokens_credit_multiplier_micro = 0;
    let snapshot = PolicySnapshot {
        policy_version: 1,
        model_catalog: catalog,
        kill_switches: KillSwitches::default(),
    };
    let l = limits(100_000_000, 1_000_000_000, 50_000_000, 500_000_000);
    let periods = today();
    let rows = UsageRows::default();
    // standard selection → [standard] only, no upgrade to the (available) premium tier
    assert!(run_cascade(&snapshot, &l, &rows, &periods, &input("gpt-standard"), 32_768, 50).is_none());
    // premium selection still works
    let o = run_cascade(&snapshot, &l, &rows, &periods, &input("gpt-premium"), 32_768, 50).unwrap();
    assert_eq!(o.effective.id, "gpt-premium");
}

#[test]
fn tier_candidate_prefers_default_then_first() {
    let mut a = model("std-a", "standard");
    let mut b = model("std-b", "standard");
    b.preference = Some(mini_chat_sdk::ModelPreference {
        is_default: true,
        sort_order: 1,
    });
    let p = model("prem", "premium");
    let snapshot = PolicySnapshot {
        policy_version: 1,
        model_catalog: vec![p, a.clone(), b.clone()],
        kill_switches: KillSwitches {
            force_standard_tier: true,
            ..KillSwitches::default()
        },
    };
    let l = limits(100_000_000, 1_000_000_000, 50_000_000, 500_000_000);
    let rows = UsageRows::default();
    let o = run_cascade(&snapshot, &l, &rows, &today(), &input("prem"), 32_768, 50).unwrap();
    assert_eq!(o.effective.id, "std-b", "enabled is_default model of the tier");
    // without a default: first enabled of the tier
    b.preference = None;
    a.enabled = false;
    let snapshot = PolicySnapshot {
        model_catalog: vec![model("prem", "premium"), a, b, model("std-c", "standard")],
        ..snapshot
    };
    let o = run_cascade(&snapshot, &l, &rows, &today(), &input("prem"), 32_768, 50).unwrap();
    assert_eq!(o.effective.id, "std-b");
    assert_eq!(o.downgrade_reason, Some(DowngradeReason::ForceStandardTier));
}

#[test]
fn tier_without_enabled_models_is_skipped() {
    let mut s = model("std", "standard");
    s.enabled = false;
    let snapshot = PolicySnapshot {
        policy_version: 1,
        model_catalog: vec![s],
        kill_switches: KillSwitches::default(),
    };
    let l = limits(100_000_000, 1_000_000_000, 50_000_000, 500_000_000);
    assert!(run_cascade(&snapshot, &l, &UsageRows::default(), &today(), &input("std"), 32_768, 50).is_none());
}

// ── tool daily quotas ──────────────────────────────────────────────────────

#[tokio::test]
async fn web_search_daily_quota() {
    let env = TestEnv::default_env().await;
    seed(
        &env,
        today(),
        PeriodKind::Daily,
        Bucket::Total,
        RowDelta {
            web_search_calls: 75,
            ..RowDelta::default()
        },
    )
    .await;
    let mut i = input("gpt-premium");
    i.web_search_requested = true;
    let err = env.services.quota.preflight(&i).await.unwrap_err();
    assert_eq!(subject(&err), "web_search");
    // not requested → not checked
    let d = env.services.quota.preflight(&input("gpt-premium")).await.unwrap();
    assert!(!d.tools.web_search);
    env.shutdown().await;
}

#[tokio::test]
async fn web_search_quota_not_checked_without_tool_support() {
    let mut catalog = default_catalog();
    catalog[0].general_config.tool_support.web_search = false;
    let env = TestEnv::new(TestOptions {
        catalog,
        ..TestOptions::default()
    })
    .await;
    seed(
        &env,
        today(),
        PeriodKind::Daily,
        Bucket::Total,
        RowDelta {
            web_search_calls: 1000,
            ..RowDelta::default()
        },
    )
    .await;
    let mut i = input("gpt-premium");
    i.web_search_requested = true;
    let d = env.services.quota.preflight(&i).await.unwrap();
    assert!(!d.tools.web_search);
    env.shutdown().await;
}

#[tokio::test]
async fn code_interpreter_daily_quota() {
    let env = TestEnv::default_env().await;
    seed(
        &env,
        today(),
        PeriodKind::Daily,
        Bucket::Total,
        RowDelta {
            code_interpreter_calls: 50,
            web_search_calls: 74,
            ..RowDelta::default()
        },
    )
    .await;
    let mut i = input("gpt-standard");
    i.has_ready_code_interpreter_files = true;
    i.web_search_requested = true;
    let err = env.services.quota.preflight(&i).await.unwrap_err();
    assert_eq!(subject(&err), "code_interpreter");
    i.has_ready_code_interpreter_files = false;
    i.has_ready_documents = true;
    let d = env.services.quota.preflight(&i).await.unwrap();
    assert_eq!(
        d.tools,
        ToolGates {
            web_search: true,
            file_search: true,
            code_interpreter: false
        }
    );
    env.shutdown().await;
}

#[tokio::test]
async fn code_interpreter_quota_skipped_under_kill_switch() {
    let env = env_with_kill(KillSwitches {
        disable_code_interpreter: true,
        disable_file_search: true,
        ..KillSwitches::default()
    })
    .await;
    seed(
        &env,
        today(),
        PeriodKind::Daily,
        Bucket::Total,
        RowDelta {
            code_interpreter_calls: 500,
            ..RowDelta::default()
        },
    )
    .await;
    let mut i = input("gpt-standard");
    i.has_ready_code_interpreter_files = true;
    i.has_ready_documents = true;
    let d = env.services.quota.preflight(&i).await.unwrap();
    assert_eq!(d.tools, ToolGates::default());
    env.shutdown().await;
}

// ── reserve + re-check ─────────────────────────────────────────────────────

#[tokio::test]
async fn reserve_recheck_rejects_and_rolls_back() {
    let env = TestEnv::default_env().await;
    let d = env.services.quota.preflight(&input("gpt-standard")).await.unwrap();
    // a concurrent request books most of the daily total after the preflight
    seed(
        &env,
        d.periods,
        PeriodKind::Daily,
        Bucket::Total,
        RowDelta {
            reserved: 100_000_000 - d.reserved_credits_micro + 1,
            ..RowDelta::default()
        },
    )
    .await;
    let err = reserve(&env, &d).await.unwrap_err();
    assert_eq!(subject(&err), "tokens");
    let p = d.periods;
    assert_eq!(
        row(&env, &p, PeriodKind::Daily, Bucket::Total).await.reserved_credits_micro,
        100_000_000 - d.reserved_credits_micro + 1,
        "rolled back"
    );
    assert!(rows(&env, &p).await.get(&p, PeriodKind::Monthly, Bucket::Total).is_none());
    env.shutdown().await;
}

#[tokio::test]
async fn reserve_exactly_at_limit_passes() {
    let env = TestEnv::default_env().await;
    let d = env.services.quota.preflight(&input("gpt-standard")).await.unwrap();
    spent(&env, PeriodKind::Daily, Bucket::Total, 100_000_000 - d.reserved_credits_micro).await;
    reserve(&env, &d).await.unwrap();
    let r = row(&env, &d.periods, PeriodKind::Daily, Bucket::Total).await;
    assert_eq!(r.spent_credits_micro + r.reserved_credits_micro, 100_000_000);
    reserve(&env, &d).await.unwrap_err();
    env.shutdown().await;
}

// ── settlement ─────────────────────────────────────────────────────────────

/// Standard model with exact budgets; reserve_tokens = 1000 input + 1000 output.
fn settle_options() -> TestOptions {
    let mut catalog = default_catalog();
    for m in &mut catalog {
        m.estimation_budgets = exact_budgets();
        m.max_output_tokens = 1000;
    }
    TestOptions {
        catalog,
        ..TestOptions::default()
    }
}

async fn reserved_turn(env: &TestEnv, model_id: &str) -> PreflightDecision {
    let mut i = input(model_id);
    i.message_bytes = 1000;
    let d = env.services.quota.preflight(&i).await.unwrap();
    assert_eq!(d.reserve_tokens, 2000);
    // 1000 * 1 + 1000 * 3
    assert_eq!(d.reserved_credits_micro, 4000);
    reserve(env, &d).await.unwrap();
    d
}

#[tokio::test]
async fn settle_actual_within_tolerance() {
    let env = TestEnv::new(settle_options()).await;
    let d = reserved_turn(&env, "gpt-standard").await;
    // 2100 / 2000 = 1.05 <= 1.10 → actual
    let res = settle(&env, &settlement(&d, SettlementMethod::Actual, usage(1100, 1000))).await.unwrap();
    assert_eq!(res.committed_credits_micro, 1100 + 3000);
    assert!(!res.overshoot_capped);
    for p in PeriodKind::ALL {
        let r = row(&env, &d.periods, p, Bucket::Total).await;
        assert_eq!(r.reserved_credits_micro, 0);
        assert_eq!(r.spent_credits_micro, 4100);
        assert_eq!((r.calls, r.input_tokens, r.output_tokens), (1, 1100, 1000));
        assert_eq!((r.web_search_calls, r.code_interpreter_calls), (2, 1));
    }
    assert!(rows(&env, &d.periods).await.get(&d.periods, PeriodKind::Daily, Bucket::Premium).is_none());
    env.shutdown().await;
}

#[tokio::test]
async fn settle_actual_beyond_tolerance_caps_at_reserve() {
    let env = TestEnv::new(settle_options()).await;
    let d = reserved_turn(&env, "gpt-premium").await;
    // 2400 / 2000 = 1.2 > 1.10 → committed = reserved
    let res = settle(&env, &settlement(&d, SettlementMethod::Actual, usage(1400, 1000))).await.unwrap();
    assert_eq!(res.committed_credits_micro, 4000);
    assert!(res.overshoot_capped);
    for p in PeriodKind::ALL {
        let t = row(&env, &d.periods, p, Bucket::Total).await;
        assert_eq!((t.reserved_credits_micro, t.spent_credits_micro), (0, 4000));
        assert_eq!((t.input_tokens, t.output_tokens), (1400, 1000), "actual tokens as telemetry");
        let pr = row(&env, &d.periods, p, Bucket::Premium).await;
        assert_eq!((pr.reserved_credits_micro, pr.spent_credits_micro, pr.calls), (0, 4000, 1));
        assert_eq!((pr.input_tokens, pr.web_search_calls), (0, 0), "telemetry only on total");
    }
    env.shutdown().await;
}

#[tokio::test]
async fn settle_actual_without_usage_charges_zero() {
    let env = TestEnv::new(settle_options()).await;
    let d = reserved_turn(&env, "gpt-standard").await;
    let res = settle(&env, &settlement(&d, SettlementMethod::Actual, None)).await.unwrap();
    assert_eq!(res.committed_credits_micro, 0);
    let r = row(&env, &d.periods, PeriodKind::Daily, Bucket::Total).await;
    assert_eq!((r.reserved_credits_micro, r.spent_credits_micro, r.calls), (0, 0, 1));
    env.shutdown().await;
}

#[tokio::test]
async fn settle_estimated() {
    let env = TestEnv::new(settle_options()).await;
    let d = reserved_turn(&env, "gpt-premium").await;
    let res = settle(&env, &settlement(&d, SettlementMethod::Estimated, usage(5, 5))).await.unwrap();
    // credits_micro(2000 - 1000, floor 50) = 1000 + 150
    assert_eq!(res.committed_credits_micro, 1150);
    for p in PeriodKind::ALL {
        let t = row(&env, &d.periods, p, Bucket::Total).await;
        assert_eq!((t.reserved_credits_micro, t.spent_credits_micro, t.calls), (0, 1150, 1));
        assert_eq!((t.input_tokens, t.output_tokens), (0, 0), "no tokens on estimated");
        assert_eq!((t.web_search_calls, t.code_interpreter_calls), (2, 1));
        let pr = row(&env, &d.periods, p, Bucket::Premium).await;
        assert_eq!((pr.reserved_credits_micro, pr.spent_credits_micro, pr.calls), (0, 1150, 1));
    }
    env.shutdown().await;
}

#[tokio::test]
async fn settle_released() {
    let env = TestEnv::new(settle_options()).await;
    let d = reserved_turn(&env, "gpt-standard").await;
    let res = settle(&env, &settlement(&d, SettlementMethod::Released, usage(5, 5))).await.unwrap();
    assert_eq!(res.committed_credits_micro, 0);
    let t = row(&env, &d.periods, PeriodKind::Monthly, Bucket::Total).await;
    assert_eq!((t.reserved_credits_micro, t.spent_credits_micro, t.calls), (0, 0, 1));
    assert_eq!((t.web_search_calls, t.code_interpreter_calls, t.input_tokens), (0, 0, 0));
    env.shutdown().await;
}

#[tokio::test]
async fn settle_targets_persisted_periods() {
    let env = TestEnv::new(settle_options()).await;
    let d = reserved_turn(&env, "gpt-standard").await;
    let mut s = settlement(&d, SettlementMethod::Actual, usage(10, 10));
    s.periods = QuotaPeriods {
        daily_start: date!(2020 - 01 - 31),
        monthly_start: date!(2020 - 01 - 01),
    };
    settle(&env, &s).await.unwrap();
    // current rows keep the reserve; the old period rows got the settlement
    assert_eq!(row(&env, &d.periods, PeriodKind::Daily, Bucket::Total).await.reserved_credits_micro, 4000);
    let old = row(&env, &s.periods, PeriodKind::Daily, Bucket::Total).await;
    assert_eq!((old.spent_credits_micro, old.reserved_credits_micro, old.calls), (40, 0, 1));
    env.shutdown().await;
}

#[tokio::test]
async fn settle_errors() {
    let env = TestEnv::new(settle_options()).await;
    let d = reserved_turn(&env, "gpt-standard").await;
    let mut s = settlement(&d, SettlementMethod::Actual, usage(10_000_001, 0));
    assert!(matches!(settle(&env, &s).await, Err(DomainError::Internal(_))));
    s.usage = usage(1, 1);
    s.effective_model = "gone".into();
    assert!(matches!(settle(&env, &s).await, Err(DomainError::Internal(_))));
    s.effective_model = "gpt-standard".into();
    s.policy_version = 99;
    assert!(matches!(settle(&env, &s).await, Err(DomainError::Internal(_))));
    // nothing settled
    let r = row(&env, &d.periods, PeriodKind::Daily, Bucket::Total).await;
    assert_eq!((r.reserved_credits_micro, r.calls), (4000, 0));
    env.shutdown().await;
}

// ── warnings / status ──────────────────────────────────────────────────────

#[test]
fn figures_math() {
    let f = status::figures(1000, 150, 80).unwrap();
    assert_eq!((f.remaining, f.remaining_percentage, f.warning, f.exhausted), (850, 85, false, false));
    let f = status::figures(1000, 800, 80).unwrap();
    assert_eq!((f.remaining_percentage, f.warning, f.exhausted), (20, true, false));
    let f = status::figures(1000, 791, 80).unwrap();
    assert_eq!((f.remaining_percentage, f.warning), (20, true), "floor(20.9) = 20");
    let f = status::figures(1000, 789, 80).unwrap();
    assert_eq!((f.remaining_percentage, f.warning), (21, false));
    let f = status::figures(1000, 995, 80).unwrap();
    assert_eq!((f.remaining_percentage, f.exhausted), (0, true), "< 1 % left is exhausted");
    let f = status::figures(1000, 5000, 99).unwrap();
    assert_eq!((f.remaining, f.remaining_percentage, f.warning, f.exhausted), (0, 0, true, true));
    let f = status::figures(i64::MAX, 0, 1).unwrap();
    assert_eq!((f.remaining_percentage, f.warning), (100, false));
    assert!(status::figures(0, 0, 80).is_none());
    assert!(status::figures(-1, 0, 80).is_none());
}

#[test]
fn next_reset_boundaries() {
    let p = QuotaPeriods::of(time::macros::datetime!(2026-12-31 23:59:59 UTC));
    assert_eq!(p.daily_start, date!(2026 - 12 - 31));
    assert_eq!(p.monthly_start, date!(2026 - 12 - 01));
    assert_eq!(
        status::next_reset(PeriodKind::Daily, &p),
        time::macros::datetime!(2027-01-01 00:00 UTC)
    );
    assert_eq!(
        status::next_reset(PeriodKind::Monthly, &p),
        time::macros::datetime!(2027-01-01 00:00 UTC)
    );
    let p = QuotaPeriods::of(time::macros::datetime!(2028-02-28 10:00 +05:00));
    assert_eq!(
        status::next_reset(PeriodKind::Daily, &p),
        time::macros::datetime!(2028-02-29 00:00 UTC)
    );
    assert_eq!(
        status::next_reset(PeriodKind::Monthly, &p),
        time::macros::datetime!(2028-03-01 00:00 UTC)
    );
}

#[test]
fn warnings_skip_zero_limits_and_order() {
    let l = limits(1000, 0, 100, 200);
    let periods = today();
    let w = status::warnings(&l, &UsageRows::default(), &periods, 80);
    let keys: Vec<_> = w.iter().map(|x| (x.tier, x.period)).collect();
    use crate::api::rest::dto::{QuotaPeriod, QuotaTier};
    assert_eq!(
        keys,
        vec![
            (QuotaTier::Premium, QuotaPeriod::Daily),
            (QuotaTier::Premium, QuotaPeriod::Monthly),
            (QuotaTier::Total, QuotaPeriod::Daily),
        ]
    );
    assert!(w.iter().all(|x| x.remaining_percentage == 100 && !x.warning && x.next_reset.is_none()));
}

#[tokio::test]
async fn quota_warnings_and_status_after_usage() {
    let env = TestEnv::default_env().await; // standard 100M / 1000M, premium 50M / 500M
    spent(&env, PeriodKind::Daily, Bucket::Premium, 45_000_000).await;
    seed(
        &env,
        today(),
        PeriodKind::Daily,
        Bucket::Total,
        RowDelta {
            spent: 45_000_000,
            reserved: 5_000_000,
            ..RowDelta::default()
        },
    )
    .await;
    let w = env.services.quota.quota_warnings(TENANT_A, USER_A1).await.unwrap();
    assert_eq!(w.len(), 4);
    // premium daily: 5M of 50M left = 10 % → warning
    assert_eq!((w[0].remaining_percentage, w[0].warning, w[0].exhausted), (10, true, false));
    assert_eq!(w[0].next_reset, Some(status::next_reset(PeriodKind::Daily, &today())));
    assert_eq!((w[1].remaining_percentage, w[1].warning), (100, false));
    assert!(w[1].next_reset.is_none());
    // total daily: 50M of 100M → 50 %
    assert_eq!((w[2].remaining_percentage, w[2].warning), (50, false));

    let s = env.services.quota.quota_status(&ctx_a1()).await.unwrap();
    assert_eq!(s.warning_threshold_pct, 80);
    assert_eq!(s.tiers.len(), 2);
    let total_daily = &s.tiers[1].periods[0];
    assert_eq!(total_daily.limit_credits_micro, 100_000_000);
    assert_eq!(total_daily.used_credits_micro, 50_000_000);
    assert_eq!(total_daily.remaining_credits_micro, 50_000_000);
    assert_eq!(total_daily.remaining_percentage, 50);
    assert_eq!(total_daily.next_reset, status::next_reset(PeriodKind::Daily, &today()));
    let v = serde_json::to_value(&s).unwrap();
    assert_eq!(v["tiers"][0]["tier"], "premium");
    assert_eq!(v["tiers"][1]["tier"], "total");
    assert_eq!(v["tiers"][0]["periods"][0]["period"], "daily");
    assert!(v["tiers"][0]["periods"][0]["next_reset"].as_str().unwrap().ends_with('Z'));

    // another user sees nothing of A1's usage
    let other = env
        .services
        .quota
        .quota_status(&crate::domain::service::test_support::ctx(
            crate::domain::service::test_support::USER_A2,
            TENANT_A,
        ))
        .await
        .unwrap();
    assert_eq!(other.tiers[1].periods[0].used_credits_micro, 0);
    env.shutdown().await;
}

#[tokio::test]
async fn quota_status_denied() {
    let env = TestEnv::new(TestOptions {
        pdp: Arc::new(DenyPdp),
        ..TestOptions::default()
    })
    .await;
    let err = env.services.quota.quota_status(&ctx_a1()).await.unwrap_err();
    assert!(matches!(err, DomainError::PermissionDenied { .. }));
    env.shutdown().await;
}

#[test]
fn settlement_credits_pure() {
    let m: ModelCatalogEntry = model("m", "standard");
    let base = SettlementInput {
        tenant_id: TENANT_A,
        user_id: USER_A1,
        effective_model: "m".into(),
        policy_version: 1,
        reserve_tokens: 1000,
        max_output_tokens_applied: 400,
        reserved_credits_micro: 1800,
        minimal_generation_floor_applied: 50,
        periods: today(),
        method: SettlementMethod::Actual,
        usage: usage(1050, 50), // 1100 / 1000 = 1.1 → not above tolerance
        web_search_calls: 0,
        code_interpreter_calls: 0,
    };
    let r = settlement_credits(&m, &base, 1.10).unwrap();
    assert_eq!((r.committed_credits_micro, r.overshoot_capped), (1050 + 150, false));
    let r = settlement_credits(&m, &SettlementInput { usage: usage(1051, 50), ..base.clone() }, 1.10).unwrap();
    assert_eq!((r.committed_credits_micro, r.overshoot_capped), (1800, true));
    let r = settlement_credits(&m, &SettlementInput { reserve_tokens: 0, ..base.clone() }, 1.5).unwrap();
    assert!(r.overshoot_capped, "zero reserve with usage is capped");
    let r = settlement_credits(&m, &SettlementInput { method: SettlementMethod::Estimated, ..base }, 1.1).unwrap();
    assert_eq!(r.committed_credits_micro, 600 + 150);
}
