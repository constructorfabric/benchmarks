//! Quota service tests: rows are seeded through the `quota_usage` repo on the `TestApp`
//! database; preflight inputs are built directly (fixed `now`).

use std::sync::Arc;

use mini_chat_sdk::{
    EstimationBudgets, KillSwitches, ModelCatalogEntry, ModelPreference, PolicySnapshot,
    TierLimits, UserLimits,
};
use time::macros::{date, datetime};
use time::{Date, OffsetDateTime};
use uuid::Uuid;

use super::*;
use crate::infra::db::entity::quota_usage;
use crate::infra::db::repo::quota_usage::{self as repo, BucketDelta, BucketKey};
use crate::infra::llm::types::ProviderUsage;
use crate::test_support::app::{NO_KILL_SWITCHES, PREMIUM_LIMITS, STANDARD_LIMITS, TestApp, ctx};
use crate::test_support::catalog::{no_vision_model, premium_model, standard_model, test_catalog};

/// Fixed preflight clock: daily period 2026-10-04, monthly period 2026-10-01.
const NOW: OffsetDateTime = datetime!(2026-10-04 15:30:00 UTC);
const TODAY: Date = date!(2026 - 10 - 04);
const MONTH: Date = date!(2026 - 10 - 01);

#[derive(Clone, Copy)]
struct User {
    tenant: Uuid,
    user: Uuid,
}

fn new_user() -> User {
    User {
        tenant: Uuid::new_v4(),
        user: Uuid::new_v4(),
    }
}

fn start(period: Period) -> Date {
    match period {
        Period::Daily => TODAY,
        Period::Monthly => MONTH,
    }
}

/// Adds `delta` to the user's `bucket` row of the current `period` (creating the row).
async fn seed(app: &TestApp, u: User, period: Period, bucket: Bucket, delta: BucketDelta) {
    seed_at(app, u, period, start(period), bucket, delta).await;
}

async fn seed_at(
    app: &TestApp,
    u: User,
    period: Period,
    period_start: Date,
    bucket: Bucket,
    delta: BucketDelta,
) {
    let conn = app.services.db.conn().expect("conn");
    repo::add(
        &conn,
        &BucketKey {
            tenant_id: u.tenant,
            user_id: u.user,
            period,
            period_start,
            bucket,
        },
        &delta,
    )
    .await
    .expect("seed quota_usage");
}

fn spent(credits: i64) -> BucketDelta {
    BucketDelta {
        spent_credits_micro: credits,
        ..BucketDelta::default()
    }
}

/// The user's row of the current `period` and `bucket`.
async fn row(app: &TestApp, u: User, period: Period, bucket: Bucket) -> Option<quota_usage::Model> {
    let conn = app.services.db.conn().expect("conn");
    let starts = PeriodStarts {
        daily: TODAY,
        monthly: MONTH,
    };
    repo::load_current(&conn, &repo::owner_scope(u.tenant, u.user), &starts, false)
        .await
        .expect("load rows")
        .into_iter()
        .find(|r| {
            r.period_type == period.as_str()
                && r.period_start == start(period)
                && r.bucket == bucket.as_str()
        })
}

/// `(spent, reserved)` of the user's row (zeros when there is no row).
async fn credits(app: &TestApp, u: User, period: Period, bucket: Bucket) -> (i64, i64) {
    row(app, u, period, bucket).await.map_or((0, 0), |r| {
        (r.spent_credits_micro, r.reserved_credits_micro)
    })
}

/// Preflight input on the default test catalog, kill switches and limits: a 10-byte message,
/// no history, images or tools, `streaming.max_output_tokens = 32768`, floor 50.
fn input(u: User, selected: &str) -> PreflightInput {
    PreflightInput {
        tenant_id: u.tenant,
        user_id: u.user,
        selected_model: selected.to_owned(),
        snapshot: PolicySnapshot {
            policy_version: 1,
            model_catalog: test_catalog(),
            kill_switches: NO_KILL_SWITCHES,
        },
        limits: UserLimits {
            user_id: u.user,
            policy_version: 1,
            standard: STANDARD_LIMITS,
            premium: PREMIUM_LIMITS,
        },
        message_bytes: 10,
        prior_context_tokens: 0,
        image_count: 0,
        chat_has_ready_documents: false,
        chat_has_ready_ci_files: false,
        web_search_requested: false,
        streaming_max_output_tokens: 32768,
        minimal_generation_floor: 50,
        now: NOW,
    }
}

fn with_catalog(mut i: PreflightInput, catalog: Vec<ModelCatalogEntry>) -> PreflightInput {
    i.snapshot.model_catalog = catalog;
    i
}

fn with_kill_switches(mut i: PreflightInput, ks: KillSwitches) -> PreflightInput {
    i.snapshot.kill_switches = ks;
    i
}

async fn app() -> TestApp {
    TestApp::builder().build().await
}

fn quota(app: &TestApp) -> Arc<QuotaService> {
    Arc::clone(&app.services.quota)
}

fn assert_quota_exceeded(res: Result<PreflightDecision, DomainError>, expected: &str) {
    match res {
        Err(DomainError::QuotaExceeded { scope }) => assert_eq!(scope, expected),
        other => panic!("expected QuotaExceeded{{{expected}}}, got {other:?}"),
    }
}

/// Exact budgets: estimated text tokens == message bytes.
fn exact_budgets() -> EstimationBudgets {
    EstimationBudgets {
        bytes_per_token_conservative: 1,
        fixed_overhead_tokens: 0,
        safety_margin_pct: 0,
        ..EstimationBudgets::default()
    }
}

#[tokio::test]
async fn design_example_downgrades_premium() {
    // DESIGN 5.10: P 2.5 credits / 1K tokens, S 1 credit / 1K tokens, 1000 input + 500 output.
    let app = app().await;
    let mut p = premium_model("P");
    p.input_tokens_credit_multiplier_micro = 2_500_000_000;
    p.output_tokens_credit_multiplier_micro = 2_500_000_000;
    p.estimation_budgets = exact_budgets();
    let mut s = standard_model("S");
    s.input_tokens_credit_multiplier_micro = 1_000_000_000;
    s.output_tokens_credit_multiplier_micro = 1_000_000_000;
    s.estimation_budgets = exact_budgets();
    let limits = |u: User| UserLimits {
        user_id: u.user,
        policy_version: 1,
        standard: TierLimits {
            limit_daily_credits_micro: 60_000_000,
            limit_monthly_credits_micro: 600_000_000,
        },
        premium: TierLimits {
            limit_daily_credits_micro: 22_000_000,
            limit_monthly_credits_micro: 300_000_000,
        },
    };
    let request = |u: User| {
        let mut i = with_catalog(input(u, "P"), vec![p.clone(), s.clone()]);
        i.limits = limits(u);
        i.message_bytes = 1000;
        i.streaming_max_output_tokens = 500;
        i
    };

    let u = new_user();
    seed(&app, u, Period::Daily, Bucket::Premium, spent(20_000_000)).await;
    seed(
        &app,
        u,
        Period::Monthly,
        Bucket::Premium,
        spent(200_000_000),
    )
    .await;
    seed(&app, u, Period::Daily, Bucket::Total, spent(25_000_000)).await;
    seed(&app, u, Period::Monthly, Bucket::Total, spent(240_000_000)).await;

    let d = quota(&app).preflight(request(u)).await.expect("downgrade");
    assert_eq!(d.effective_model.id, "S");
    assert!(!d.effective_is_premium);
    assert_eq!(d.quota_decision, QuotaDecision::Downgrade);
    assert_eq!(d.downgrade_reason, Some("premium_quota_exhausted"));
    assert_eq!(
        d.reserve,
        ReserveAmounts {
            estimated_input_tokens: 1000,
            max_output_tokens_applied: 500,
            reserve_tokens: 1500,
            reserved_credits_micro: 1_500_000,
            minimal_generation_floor_applied: 50,
        }
    );
    assert_eq!(d.policy_version, 1);
    assert_eq!(
        d.periods,
        PeriodStarts {
            daily: TODAY,
            monthly: MONTH
        }
    );
    assert_eq!(d.limits, limits(u));

    // Premium fits exactly (18.25M + 3.75M == 22M): allowed with the premium reserve.
    let u = new_user();
    seed(&app, u, Period::Daily, Bucket::Premium, spent(18_250_000)).await;
    let d = quota(&app).preflight(request(u)).await.expect("allow");
    assert_eq!(d.effective_model.id, "P");
    assert!(d.effective_is_premium);
    assert_eq!(d.quota_decision, QuotaDecision::Allow);
    assert_eq!(d.downgrade_reason, None);
    assert_eq!(d.reserve.reserved_credits_micro, 3_750_000);
}

#[tokio::test]
async fn cascade_rules() {
    let app = app().await;
    let q = quota(&app);
    let exhaust = |limit: i64| spent(limit);

    // Standard selected and available: allow.
    let u = new_user();
    let d = q.preflight(input(u, "gpt-standard")).await.expect("allow");
    assert_eq!(
        (
            d.effective_model.id.as_str(),
            d.quota_decision,
            d.downgrade_reason
        ),
        ("gpt-standard", QuotaDecision::Allow, None)
    );

    // Standard selected never upgrades to premium.
    let u = new_user();
    seed(&app, u, Period::Daily, Bucket::Total, exhaust(100_000_000)).await;
    assert_quota_exceeded(q.preflight(input(u, "gpt-standard")).await, "tokens");

    // Disabled selected model: the tier's default model, `model_disabled`.
    let d = q
        .preflight(input(new_user(), "gpt-disabled"))
        .await
        .expect("downgrade");
    assert_eq!(d.effective_model.id, "gpt-premium");
    assert_eq!(d.quota_decision, QuotaDecision::Downgrade);
    assert_eq!(d.downgrade_reason, Some("model_disabled"));

    // Missing selected model: treated as premium, `model_disabled`.
    let d = q
        .preflight(input(new_user(), "gone"))
        .await
        .expect("downgrade");
    assert_eq!(d.effective_model.id, "gpt-premium");
    assert!(d.effective_is_premium);
    assert_eq!(d.quota_decision, QuotaDecision::Downgrade);
    assert_eq!(d.downgrade_reason, Some("model_disabled"));

    // Kill switches skip the premium tier.
    let ks = KillSwitches {
        force_standard_tier: true,
        ..NO_KILL_SWITCHES
    };
    let d = q
        .preflight(with_kill_switches(input(new_user(), "gpt-premium"), ks))
        .await
        .expect("downgrade");
    assert_eq!(
        (
            d.effective_model.id.as_str(),
            d.quota_decision,
            d.downgrade_reason
        ),
        (
            "gpt-standard",
            QuotaDecision::Downgrade,
            Some("force_standard_tier")
        )
    );
    let ks = KillSwitches {
        disable_premium_tier: true,
        ..NO_KILL_SWITCHES
    };
    let d = q
        .preflight(with_kill_switches(input(new_user(), "gpt-premium"), ks))
        .await
        .expect("downgrade");
    assert_eq!(
        (d.effective_model.id.as_str(), d.downgrade_reason),
        ("gpt-standard", Some("disable_premium_tier"))
    );

    // Everything exhausted: 429 tokens.
    let u = new_user();
    seed(&app, u, Period::Daily, Bucket::Total, exhaust(100_000_000)).await;
    assert_quota_exceeded(q.preflight(input(u, "gpt-premium")).await, "tokens");

    // Monthly-only exhaustion of the premium subcap makes premium unavailable.
    let u = new_user();
    seed(
        &app,
        u,
        Period::Monthly,
        Bucket::Premium,
        exhaust(500_000_000),
    )
    .await;
    let d = q
        .preflight(input(u, "gpt-premium"))
        .await
        .expect("downgrade");
    assert_eq!(
        (d.effective_model.id.as_str(), d.downgrade_reason),
        ("gpt-standard", Some("premium_quota_exhausted"))
    );

    // Monthly-only exhaustion of the total cap rejects.
    let u = new_user();
    seed(
        &app,
        u,
        Period::Monthly,
        Bucket::Total,
        exhaust(1_000_000_000),
    )
    .await;
    assert_quota_exceeded(q.preflight(input(u, "gpt-premium")).await, "tokens");

    // An earlier reason is kept: missing model, then premium exhausted.
    let u = new_user();
    seed(&app, u, Period::Daily, Bucket::Premium, exhaust(50_000_000)).await;
    let d = q.preflight(input(u, "gone")).await.expect("downgrade");
    assert_eq!(
        (d.effective_model.id.as_str(), d.downgrade_reason),
        ("gpt-standard", Some("model_disabled"))
    );

    // Reserved credits of in-flight turns count like spent credits.
    let u = new_user();
    seed(
        &app,
        u,
        Period::Daily,
        Bucket::Premium,
        BucketDelta {
            reserved_credits_micro: 50_000_000,
            ..BucketDelta::default()
        },
    )
    .await;
    let d = q
        .preflight(input(u, "gpt-premium"))
        .await
        .expect("downgrade");
    assert_eq!(d.effective_model.id, "gpt-standard");
}

#[tokio::test]
async fn cascade_candidate_selection() {
    let app = app().await;
    let q = quota(&app);
    let default_of = |mut m: ModelCatalogEntry| {
        m.preference = Some(ModelPreference {
            is_default: true,
            sort_order: 0,
        });
        m
    };
    let disabled = |mut m: ModelCatalogEntry| {
        m.enabled = false;
        m
    };

    // The tenant default of the tier wins over catalog order.
    let catalog = vec![
        premium_model("p-first"),
        default_of(premium_model("p-default")),
        standard_model("s"),
    ];
    let d = q
        .preflight(with_catalog(input(new_user(), "gone"), catalog))
        .await
        .expect("downgrade");
    assert_eq!(d.effective_model.id, "p-default");

    // A disabled default is skipped: first enabled model of the tier.
    let catalog = vec![
        disabled(default_of(premium_model("p-default"))),
        premium_model("p-first"),
        standard_model("s"),
    ];
    let d = q
        .preflight(with_catalog(input(new_user(), "gone"), catalog))
        .await
        .expect("downgrade");
    assert_eq!(d.effective_model.id, "p-first");

    // A tier without enabled models is skipped.
    let catalog = vec![disabled(premium_model("p")), standard_model("s")];
    let d = q
        .preflight(with_catalog(input(new_user(), "p"), catalog))
        .await
        .expect("downgrade");
    assert_eq!(
        (d.effective_model.id.as_str(), d.downgrade_reason),
        ("s", Some("model_disabled"))
    );

    // Disabled standard model: another standard model, `model_disabled`.
    let catalog = vec![
        premium_model("p"),
        disabled(standard_model("s-off")),
        standard_model("s-on"),
    ];
    let d = q
        .preflight(with_catalog(input(new_user(), "s-off"), catalog))
        .await
        .expect("downgrade");
    assert_eq!(
        (d.effective_model.id.as_str(), d.downgrade_reason),
        ("s-on", Some("model_disabled"))
    );

    // No enabled model at all: 429 tokens.
    let catalog = vec![disabled(standard_model("s"))];
    assert_quota_exceeded(
        q.preflight(with_catalog(input(new_user(), "s"), catalog))
            .await,
        "tokens",
    );

    // A candidate whose reserve cannot be computed (zero multiplier) is unavailable.
    let mut broken = premium_model("p");
    broken.input_tokens_credit_multiplier_micro = 0;
    let d = q
        .preflight(with_catalog(
            input(new_user(), "p"),
            vec![broken, standard_model("s")],
        ))
        .await
        .expect("downgrade");
    assert_eq!(
        (d.effective_model.id.as_str(), d.downgrade_reason),
        ("s", Some("premium_quota_exhausted"))
    );
}

#[tokio::test]
async fn surcharges_follow_candidate_tools() {
    let app = app().await;
    let q = quota(&app);
    let full = |u: User, selected: &str| {
        let mut i = input(u, selected);
        i.message_bytes = 10; // 114 tokens with the default budgets
        i.prior_context_tokens = 100;
        i.image_count = 2; // 2 * 1000
        i.chat_has_ready_documents = true; // + 500
        i.web_search_requested = true; // + 500
        i.chat_has_ready_ci_files = true; // + 1000
        i
    };

    let d = q
        .preflight(full(new_user(), "gpt-premium"))
        .await
        .expect("allow");
    assert_eq!(
        d.tools,
        ToolGates {
            file_search: true,
            web_search: true,
            code_interpreter: true
        }
    );
    assert!(d.vision_supported);
    assert_eq!(
        d.reserve,
        ReserveAmounts {
            estimated_input_tokens: 4214,
            max_output_tokens_applied: 4096,
            reserve_tokens: 8310,
            // ceil(4214 * 3) + ceil(4096 * 3)
            reserved_credits_micro: 24_930,
            minimal_generation_floor_applied: 50,
        }
    );

    // A model without tools gets no tool surcharge (images still count).
    let d = q
        .preflight(full(new_user(), "gpt-mini-novision"))
        .await
        .expect("allow");
    assert_eq!(d.tools, ToolGates::default());
    assert!(!d.vision_supported);
    assert_eq!(d.reserve.estimated_input_tokens, 2214);
    assert_eq!(d.reserve.reserved_credits_micro, 2214 + 4096);

    // Kill switches remove file search and code interpreter (and their surcharges).
    let ks = KillSwitches {
        disable_file_search: true,
        disable_code_interpreter: true,
        ..NO_KILL_SWITCHES
    };
    let d = q
        .preflight(with_kill_switches(full(new_user(), "gpt-premium"), ks))
        .await
        .expect("allow");
    assert_eq!(
        d.tools,
        ToolGates {
            file_search: false,
            web_search: true,
            code_interpreter: false
        }
    );
    assert_eq!(d.reserve.estimated_input_tokens, 2714);

    // A downgraded candidate is estimated with its own tool support.
    let ks = KillSwitches {
        force_standard_tier: true,
        ..NO_KILL_SWITCHES
    };
    let catalog = vec![premium_model("gpt-premium"), no_vision_model("plain")];
    let d = q
        .preflight(with_kill_switches(
            with_catalog(full(new_user(), "gpt-premium"), catalog),
            ks,
        ))
        .await
        .expect("downgrade");
    assert_eq!(d.effective_model.id, "plain");
    assert_eq!(d.tools, ToolGates::default());
    assert_eq!(d.reserve.estimated_input_tokens, 2214);

    // max_output_tokens_applied = min(model, streaming); floor = min(floor, max output).
    let mut i = input(new_user(), "gpt-standard");
    i.streaming_max_output_tokens = 30;
    let d = q.preflight(i).await.expect("allow");
    assert_eq!(d.reserve.max_output_tokens_applied, 30);
    assert_eq!(d.reserve.minimal_generation_floor_applied, 30);
    assert_eq!(d.reserve.reserve_tokens, 114 + 30);
}

#[tokio::test]
async fn web_search_kill_switch_before_quota() {
    let app = app().await;
    let q = quota(&app);
    let ks = KillSwitches {
        disable_web_search: true,
        ..NO_KILL_SWITCHES
    };
    let u = new_user();
    seed(&app, u, Period::Daily, Bucket::Total, spent(100_000_000)).await;
    let mut i = with_kill_switches(input(u, "gpt-premium"), ks);
    i.web_search_requested = true;
    match q.preflight(i).await {
        Err(DomainError::FeatureDisabled { subject }) => assert_eq!(subject, "web_search"),
        other => panic!("expected FeatureDisabled{{web_search}}, got {other:?}"),
    }

    // Not requested: the kill switch does not reject.
    let d = q
        .preflight(with_kill_switches(input(new_user(), "gpt-premium"), ks))
        .await
        .expect("allow");
    assert!(!d.tools.web_search);
}

#[tokio::test]
async fn daily_tool_quotas() {
    let app = app().await;
    let q = quota(&app);
    let calls = |web: i32, ci: i32| BucketDelta {
        web_search_calls: web,
        code_interpreter_calls: ci,
        ..BucketDelta::default()
    };
    let web = |u: User, model: &str| {
        let mut i = input(u, model);
        i.web_search_requested = true;
        i
    };

    let u = new_user();
    seed(&app, u, Period::Daily, Bucket::Total, calls(75, 0)).await;
    assert_quota_exceeded(q.preflight(web(u, "gpt-premium")).await, "web_search");
    // The model without web search sends no tool: not checked.
    let d = q
        .preflight(web(u, "gpt-mini-novision"))
        .await
        .expect("allowed without the tool");
    assert!(!d.tools.web_search);
    // Below the quota: allowed.
    let u = new_user();
    seed(&app, u, Period::Daily, Bucket::Total, calls(74, 0)).await;
    assert!(
        q.preflight(web(u, "gpt-premium"))
            .await
            .expect("allow")
            .tools
            .web_search
    );
    // Only the daily row counts.
    let u = new_user();
    seed(&app, u, Period::Monthly, Bucket::Total, calls(500, 0)).await;
    q.preflight(web(u, "gpt-premium"))
        .await
        .expect("monthly calls are not limited");

    let u = new_user();
    seed(&app, u, Period::Daily, Bucket::Total, calls(0, 50)).await;
    // Without ready code-interpreter files the quota is not checked.
    let d = q.preflight(input(u, "gpt-standard")).await.expect("allow");
    assert!(!d.tools.code_interpreter);
    let mut i = input(u, "gpt-standard");
    i.chat_has_ready_ci_files = true;
    assert_quota_exceeded(q.preflight(i.clone()).await, "code_interpreter");
    // Kill switch: tool not sent, quota not checked.
    let i = with_kill_switches(
        i,
        KillSwitches {
            disable_code_interpreter: true,
            ..NO_KILL_SWITCHES
        },
    );
    assert!(!q.preflight(i).await.expect("allow").tools.code_interpreter);
}

/// Runs `reserve` in its own transaction.
async fn reserve(app: &TestApp, u: User, d: &PreflightDecision) -> Result<(), DomainError> {
    let q = quota(app);
    let d = d.clone();
    let recorder = quota(app);
    let facts = app
        .services
        .db
        .transaction(move |tx| Box::pin(async move { q.reserve(tx, u.tenant, u.user, &d).await }))
        .await?;
    recorder.record_facts(facts);
    Ok(())
}

/// Runs `settle` in its own transaction.
async fn settle(app: &TestApp, input: SettleInput) -> SettleResult {
    let q = quota(app);
    let result = app
        .services
        .db
        .transaction(move |tx| Box::pin(async move { q.settle(tx, input).await }))
        .await
        .expect("settle");
    quota(app).record_facts(result.metrics);
    result
}

#[tokio::test]
async fn reserve_rechecks_limits() {
    let app = app().await;
    let q = quota(&app);
    let u = new_user();
    // gpt-standard reserve: 114 input + 4096 output tokens at 1 micro-credit per token.
    let mut i = input(u, "gpt-standard");
    i.limits.standard.limit_daily_credits_micro = 6_000;
    let d = q.preflight(i).await.expect("allow");
    assert_eq!(d.reserve.reserved_credits_micro, 4_210);

    reserve(&app, u, &d).await.expect("first reserve fits");
    assert_eq!(
        credits(&app, u, Period::Daily, Bucket::Total).await,
        (0, 4_210)
    );
    assert_eq!(
        credits(&app, u, Period::Monthly, Bucket::Total).await,
        (0, 4_210)
    );
    assert!(row(&app, u, Period::Daily, Bucket::Premium).await.is_none());

    // The second reserve of a decision taken before the first one was booked is rejected and
    // rolled back.
    match reserve(&app, u, &d).await {
        Err(DomainError::QuotaExceeded { scope }) => assert_eq!(scope, "tokens"),
        other => panic!("expected QuotaExceeded{{tokens}}, got {other:?}"),
    }
    assert_eq!(
        credits(&app, u, Period::Daily, Bucket::Total).await,
        (0, 4_210)
    );
    assert_eq!(
        credits(&app, u, Period::Monthly, Bucket::Total).await,
        (0, 4_210)
    );

    // A premium reserve books the `tier:premium` rows too.
    let u = new_user();
    let d = q.preflight(input(u, "gpt-premium")).await.expect("allow");
    assert_eq!(d.reserve.reserved_credits_micro, 12_630);
    reserve(&app, u, &d).await.expect("reserve");
    for period in Period::ALL {
        for bucket in [Bucket::Total, Bucket::Premium] {
            assert_eq!(credits(&app, u, period, bucket).await, (0, 12_630));
        }
    }

    // The premium subcap is re-checked as well.
    let u = new_user();
    let mut i = input(u, "gpt-premium");
    i.limits.premium.limit_monthly_credits_micro = 20_000;
    let d = q.preflight(i).await.expect("allow");
    reserve(&app, u, &d).await.expect("first");
    assert!(matches!(
        reserve(&app, u, &d).await,
        Err(DomainError::QuotaExceeded { scope: "tokens" })
    ));
    assert_eq!(
        credits(&app, u, Period::Monthly, Bucket::Premium).await,
        (0, 12_630)
    );
}

fn settle_input(u: User, method: SettlementMethod) -> SettleInput {
    SettleInput {
        tenant_id: u.tenant,
        user_id: u.user,
        is_premium: false,
        periods: PeriodStarts {
            daily: TODAY,
            monthly: MONTH,
        },
        turn_reserved_credits_micro: 1_500_000,
        reserve_tokens: 1_500,
        max_output_tokens_applied: 500,
        minimal_generation_floor_applied: 50,
        in_mult: 1_000_000_000,
        out_mult: 1_000_000_000,
        method,
        usage: None,
        web_search_calls: 2,
        code_interpreter_calls: 1,
    }
}

fn usage(input_tokens: i64, output_tokens: i64) -> ProviderUsage {
    ProviderUsage {
        input_tokens,
        output_tokens,
        ..ProviderUsage::default()
    }
}

/// Books `amount` reserved credits on the user's `total` (and premium) rows, like `reserve`.
async fn seed_reserved(app: &TestApp, u: User, amount: i64, premium: bool) {
    let delta = BucketDelta {
        reserved_credits_micro: amount,
        ..BucketDelta::default()
    };
    for period in Period::ALL {
        seed(app, u, period, Bucket::Total, delta).await;
        if premium {
            seed(app, u, period, Bucket::Premium, delta).await;
        }
    }
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn settle_actual_estimated_released_and_overshoot() {
    let app = app().await;

    // Actual (DESIGN 5.10.4): 900 + 300 tokens at 1 credit / 1K tokens.
    let u = new_user();
    seed_reserved(&app, u, 1_500_000, false).await;
    let res = settle(
        &app,
        SettleInput {
            usage: Some(usage(900, 300)),
            ..settle_input(u, SettlementMethod::Actual)
        },
    )
    .await;
    assert_eq!(
        res,
        SettleResult {
            committed_credits_micro: 1_200_000,
            overshoot_capped: false,
            ..res
        }
    );
    for period in Period::ALL {
        let r = row(&app, u, period, Bucket::Total)
            .await
            .expect("total row");
        assert_eq!(
            (
                r.spent_credits_micro,
                r.reserved_credits_micro,
                r.calls,
                r.input_tokens,
                r.output_tokens,
                r.web_search_calls,
                r.code_interpreter_calls
            ),
            (1_200_000, 0, 1, 900, 300, 2, 1),
            "{period:?}"
        );
        assert!(row(&app, u, period, Bucket::Premium).await.is_none());
    }

    // Estimated: credits(1500 - 500, 50) = 1_000_000 + 50_000; no token telemetry.
    let u = new_user();
    seed_reserved(&app, u, 1_500_000, false).await;
    let res = settle(&app, settle_input(u, SettlementMethod::Estimated)).await;
    assert_eq!(res.committed_credits_micro, 1_050_000);
    for period in Period::ALL {
        let r = row(&app, u, period, Bucket::Total)
            .await
            .expect("total row");
        assert_eq!(
            (
                r.spent_credits_micro,
                r.reserved_credits_micro,
                r.calls,
                r.input_tokens,
                r.output_tokens,
                r.web_search_calls,
                r.code_interpreter_calls
            ),
            (1_050_000, 0, 1, 0, 0, 2, 1),
            "{period:?}"
        );
    }

    // Released: nothing charged, reserve returned, the settlement is still counted.
    let u = new_user();
    seed_reserved(&app, u, 1_500_000, false).await;
    let res = settle(
        &app,
        SettleInput {
            usage: Some(usage(900, 300)),
            ..settle_input(u, SettlementMethod::Released)
        },
    )
    .await;
    assert_eq!(res.committed_credits_micro, 0);
    let r = row(&app, u, Period::Daily, Bucket::Total)
        .await
        .expect("row");
    assert_eq!(
        (
            r.spent_credits_micro,
            r.reserved_credits_micro,
            r.calls,
            r.input_tokens,
            r.web_search_calls,
            r.code_interpreter_calls
        ),
        (0, 0, 1, 0, 0, 0)
    );

    // Overshoot (DESIGN 5.4.5 example): 11000 + 500 vs reserve 10000 = 1.15 > 1.10 -> capped.
    let overshoot = |u: User, input_tokens: i64| SettleInput {
        turn_reserved_credits_micro: 2_500_000,
        reserve_tokens: 10_000,
        max_output_tokens_applied: 500,
        in_mult: 250_000_000,
        out_mult: 250_000_000,
        usage: Some(usage(input_tokens, 500)),
        ..settle_input(u, SettlementMethod::Actual)
    };
    let u = new_user();
    seed_reserved(&app, u, 2_500_000, false).await;
    let res = settle(&app, overshoot(u, 11_000)).await;
    assert_eq!(
        res,
        SettleResult {
            committed_credits_micro: 2_500_000,
            overshoot_capped: true,
            ..res
        }
    );
    let r = row(&app, u, Period::Daily, Bucket::Total)
        .await
        .expect("row");
    assert_eq!(
        (
            r.spent_credits_micro,
            r.reserved_credits_micro,
            r.input_tokens,
            r.output_tokens
        ),
        (2_500_000, 0, 11_000, 500)
    );
    // Exactly at the tolerance (11000 / 10000 = 1.10): actual credits are charged.
    let u = new_user();
    seed_reserved(&app, u, 2_500_000, false).await;
    let res = settle(&app, overshoot(u, 10_500)).await;
    assert_eq!(
        res,
        SettleResult {
            committed_credits_micro: 2_750_000,
            overshoot_capped: false,
            ..res
        }
    );
    assert_eq!(
        credits(&app, u, Period::Monthly, Bucket::Total).await,
        (2_750_000, 0)
    );

    // Premium turn: `tier:premium` reserved / spent / calls too, but no tokens or tool calls.
    let u = new_user();
    seed_reserved(&app, u, 1_500_000, true).await;
    settle(
        &app,
        SettleInput {
            is_premium: true,
            usage: Some(usage(900, 300)),
            ..settle_input(u, SettlementMethod::Actual)
        },
    )
    .await;
    for period in Period::ALL {
        let r = row(&app, u, period, Bucket::Premium)
            .await
            .expect("premium row");
        assert_eq!(
            (
                r.spent_credits_micro,
                r.reserved_credits_micro,
                r.calls,
                r.input_tokens,
                r.output_tokens,
                r.web_search_calls,
                r.code_interpreter_calls
            ),
            (1_200_000, 0, 1, 0, 0, 0, 0),
            "{period:?}"
        );
        assert_eq!(
            credits(&app, u, period, Bucket::Total).await,
            (1_200_000, 0)
        );
    }

    // Other in-flight reserves of the bucket stay booked.
    let u = new_user();
    seed_reserved(&app, u, 1_500_000 + 700, false).await;
    settle(&app, settle_input(u, SettlementMethod::Released)).await;
    assert_eq!(
        credits(&app, u, Period::Daily, Bucket::Total).await,
        (0, 700)
    );

    // Credits that cannot be computed fail the settlement.
    let u = new_user();
    seed_reserved(&app, u, 1_500_000, false).await;
    let q = quota(&app);
    let bad = SettleInput {
        in_mult: 0,
        usage: Some(usage(900, 300)),
        ..settle_input(u, SettlementMethod::Actual)
    };
    let err = app
        .services
        .db
        .transaction(move |tx| Box::pin(async move { q.settle(tx, bad).await }))
        .await
        .expect_err("zero multiplier");
    assert!(matches!(err, DomainError::Internal(_)), "{err:?}");
}

#[tokio::test]
async fn status_and_warnings() {
    let app = app().await;
    let q = quota(&app);
    let u = new_user();
    let limits = UserLimits {
        user_id: u.user,
        policy_version: 1,
        standard: TierLimits {
            limit_daily_credits_micro: 100,
            limit_monthly_credits_micro: 1000,
        },
        premium: TierLimits {
            limit_daily_credits_micro: 1000,
            limit_monthly_credits_micro: 0,
        },
    };
    seed(
        &app,
        u,
        Period::Daily,
        Bucket::Total,
        BucketDelta {
            spent_credits_micro: 80,
            reserved_credits_micro: 5,
            ..BucketDelta::default()
        },
    )
    .await;
    seed(&app, u, Period::Monthly, Bucket::Total, spent(85)).await;
    seed(&app, u, Period::Daily, Bucket::Premium, spent(995)).await;
    // Yesterday's row is not part of the current period.
    seed_at(
        &app,
        u,
        Period::Daily,
        date!(2026 - 10 - 03),
        Bucket::Total,
        spent(100),
    )
    .await;

    let conn = app.services.db.conn().expect("conn");
    let warnings = q
        .warnings(&conn, u.tenant, u.user, &limits, NOW)
        .await
        .expect("warnings");
    let tomorrow = Some(datetime!(2026-10-05 00:00:00 UTC));
    assert_eq!(
        warnings,
        vec![
            // 5 of 1000 left: floor(0.5) == 0 -> exhausted
            QuotaWarning {
                tier: Bucket::Premium,
                period: Period::Daily,
                remaining_percentage: 0,
                warning: true,
                exhausted: true,
                next_reset: tomorrow,
            },
            // 15 of 100 left (spent 80 + reserved 5)
            QuotaWarning {
                tier: Bucket::Total,
                period: Period::Daily,
                remaining_percentage: 15,
                warning: true,
                exhausted: false,
                next_reset: tomorrow,
            },
            // premium monthly (limit 0) omitted; 915 of 1000 left
            QuotaWarning {
                tier: Bucket::Total,
                period: Period::Monthly,
                remaining_percentage: 91,
                warning: false,
                exhausted: false,
                next_reset: None,
            },
        ]
    );

    // The warning threshold is inclusive (80 % -> warn at <= 20 % remaining).
    let (app_ref, q_ref, conn_ref, limits_ref) = (&app, &q, &conn, &limits);
    let edge = |used: i64| async move {
        let u = new_user();
        seed(app_ref, u, Period::Daily, Bucket::Total, spent(used)).await;
        let w = q_ref
            .warnings(conn_ref, u.tenant, u.user, limits_ref, NOW)
            .await
            .expect("warnings");
        let daily = w
            .iter()
            .find(|w| w.tier == Bucket::Total && w.period == Period::Daily)
            .expect("total daily")
            .clone();
        (daily.remaining_percentage, daily.warning)
    };
    assert_eq!(edge(80).await, (20, true));
    assert_eq!(edge(79).await, (21, false));
    // Overspent: 0 %, exhausted.
    assert_eq!(edge(150).await, (0, true));
}

#[tokio::test]
async fn status_endpoint_reports_own_usage() {
    let app = app().await;
    let u = new_user();
    // The service reads the wall clock; keep the whole test inside one UTC day.
    let mut now = OffsetDateTime::now_utc();
    let to_midnight = periods::next_reset(Period::Daily, now) - now;
    if to_midnight < time::Duration::seconds(30) {
        tokio::time::sleep((to_midnight + time::Duration::seconds(1)).unsigned_abs()).await;
        now = OffsetDateTime::now_utc();
    }
    let today = now.date();
    let month = today.replace_day(1).unwrap();
    let seed_now = |u: User, period: Period, bucket: Bucket, delta: BucketDelta| {
        let app = &app;
        async move {
            let start = match period {
                Period::Daily => today,
                Period::Monthly => month,
            };
            seed_at(app, u, period, start, bucket, delta).await;
        }
    };
    seed_now(
        u,
        Period::Daily,
        Bucket::Total,
        BucketDelta {
            spent_credits_micro: 80_000_000,
            reserved_credits_micro: 5_000_000,
            ..BucketDelta::default()
        },
    )
    .await;
    // Another user of the same tenant: not visible.
    let other = User {
        tenant: u.tenant,
        user: Uuid::new_v4(),
    };
    seed_now(other, Period::Daily, Bucket::Premium, spent(50_000_000)).await;

    let res = app
        .call(
            "GET",
            "/mini-chat/v1/quota/status",
            &ctx(u.tenant, u.user),
            None,
        )
        .await;
    assert_eq!(res.status, 200, "{}", res.json);
    let body = &res.json;
    assert_eq!(body["warning_threshold_pct"], 80);
    let tiers: Vec<&str> = body["tiers"]
        .as_array()
        .expect("tiers")
        .iter()
        .map(|t| t["tier"].as_str().unwrap())
        .collect();
    assert_eq!(tiers, ["premium", "total"]);

    let tomorrow = format!("{}T00:00:00Z", today.next_day().unwrap());
    let premium_daily = &body["tiers"][0]["periods"][0];
    assert_eq!(premium_daily["period"], "daily");
    assert_eq!(premium_daily["limit_credits_micro"], 50_000_000);
    assert_eq!(premium_daily["used_credits_micro"], 0);
    assert_eq!(premium_daily["remaining_percentage"], 100);
    assert_eq!(premium_daily["warning"], false);

    let total = body["tiers"][1]["periods"].as_array().expect("periods");
    assert_eq!(
        total[0],
        serde_json::json!({
            "period": "daily",
            "limit_credits_micro": 100_000_000,
            "used_credits_micro": 85_000_000,
            "remaining_credits_micro": 15_000_000,
            "remaining_percentage": 15,
            "next_reset": tomorrow,
            "warning": true,
            "exhausted": false,
        })
    );
    assert_eq!(total[1]["period"], "monthly");
    assert_eq!(total[1]["used_credits_micro"], 0);
    assert_eq!(total[1]["remaining_percentage"], 100);

    // A limit of 0 drops the period.
    app.usage.set_limits(
        STANDARD_LIMITS,
        TierLimits {
            limit_daily_credits_micro: 50_000_000,
            limit_monthly_credits_micro: 0,
        },
    );
    let res = app
        .call(
            "GET",
            "/mini-chat/v1/quota/status",
            &ctx(u.tenant, u.user),
            None,
        )
        .await;
    let premium_periods: Vec<&str> = res.json["tiers"][0]["periods"]
        .as_array()
        .expect("periods")
        .iter()
        .map(|p| p["period"].as_str().unwrap())
        .collect();
    assert_eq!(premium_periods, ["daily"]);
}

#[tokio::test]
async fn status_endpoint_requires_quota_permission() {
    let app = TestApp::builder()
        .pdp(crate::test_support::pdp::PdpMode::Deny)
        .build()
        .await;
    let u = new_user();
    let res = app
        .call(
            "GET",
            "/mini-chat/v1/quota/status",
            &ctx(u.tenant, u.user),
            None,
        )
        .await;
    assert_eq!(res.status, 403, "{}", res.json);
}

/// Concurrent reserves on a file-backed `SQLite` database (several connections, real locking):
/// each reserve fits alone, the limit admits exactly three; the others are rejected and leave
/// nothing behind.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_reserves_never_exceed_the_limit() {
    use authz_resolver_sdk::PolicyEnforcer;
    use toolkit::contracts::DatabaseCapability as _;
    use toolkit_db::migration_runner::run_migrations_for_testing;
    use toolkit_db::{ConnectOpts, DBProvider, connect_db};

    use crate::infra::gateways::policy::DirectPolicyGateway;
    use crate::metrics::Metrics;
    use crate::test_support::pdp::{FakePdp, PdpMode};
    use crate::test_support::plugins::RecordingPolicy;

    let dir = tempfile::tempdir().expect("tempdir");
    let dsn = format!(
        "sqlite://{}?mode=rwc",
        dir.path().join("quota.db").display()
    );
    let db = connect_db(
        &dsn,
        ConnectOpts {
            max_conns: Some(8),
            ..Default::default()
        },
    )
    .await
    .expect("connect file sqlite");
    run_migrations_for_testing(&db, crate::gear::MiniChatGear::default().migrations())
        .await
        .expect("migrations");
    let provider = Arc::new(DBProvider::<DomainError>::new(db));
    let policy = Arc::new(RecordingPolicy::new(
        test_catalog(),
        NO_KILL_SWITCHES,
        STANDARD_LIMITS,
        PREMIUM_LIMITS,
    ));
    let q = Arc::new(QuotaService::new(
        Arc::clone(&provider),
        Arc::new(crate::domain::authz::Authz::new(PolicyEnforcer::new(
            Arc::new(FakePdp::new(PdpMode::TenantConstraint)),
        ))),
        Arc::new(DirectPolicyGateway(policy)),
        crate::config::QuotaConfig::default(),
        Arc::new(Metrics::new("")),
    ));

    let u = new_user();
    let mut i = input(u, "gpt-standard");
    i.limits.standard.limit_daily_credits_micro = 3 * 4_210 + 100;
    let d = q.preflight(i).await.expect("allow");
    assert_eq!(d.reserve.reserved_credits_micro, 4_210);

    let tasks: Vec<_> = (0..8)
        .map(|_| {
            let (q, provider, d) = (Arc::clone(&q), Arc::clone(&provider), d.clone());
            tokio::spawn(async move {
                provider
                    .transaction(move |tx| {
                        Box::pin(async move { q.reserve(tx, u.tenant, u.user, &d).await })
                    })
                    .await
            })
        })
        .collect();
    let mut admitted = 0;
    for task in tasks {
        match task.await.expect("join") {
            Ok(_) => admitted += 1,
            Err(DomainError::QuotaExceeded { scope: "tokens" }) => {}
            Err(other) => panic!("unexpected error: {other}"),
        }
    }
    assert_eq!(admitted, 3);

    let conn = provider.conn().expect("conn");
    let rows = repo::load_current(
        &conn,
        &repo::owner_scope(u.tenant, u.user),
        &d.periods,
        false,
    )
    .await
    .expect("rows");
    let reserved: Vec<(String, i64)> = rows
        .iter()
        .map(|r| (r.period_type.clone(), r.reserved_credits_micro))
        .collect();
    assert_eq!(rows.len(), 2, "{reserved:?}");
    for r in rows {
        assert_eq!(r.reserved_credits_micro, 3 * 4_210, "{}", r.period_type);
    }
}

#[tokio::test]
async fn reserve_and_settle_lock_rows_in_the_preflight_order() {
    // The preflight's locking select orders by (period_type, bucket) text:
    // "daily" < "monthly", "tier:premium" < "total".
    assert_eq!(
        lock_order(true),
        [
            (Period::Daily, Bucket::Premium),
            (Period::Daily, Bucket::Total),
            (Period::Monthly, Bucket::Premium),
            (Period::Monthly, Bucket::Total),
        ]
    );
    assert_eq!(
        lock_order(false),
        [
            (Period::Daily, Bucket::Total),
            (Period::Monthly, Bucket::Total)
        ]
    );
    let mut by_text = lock_order(true);
    by_text.sort_by_key(|(p, b)| (p.as_str(), b.as_str()));
    assert_eq!(by_text, lock_order(true));

    // The locking select returns (and so locks) the rows in that order, whatever the insert
    // order.
    let app = app().await;
    let u = new_user();
    for (period, bucket) in lock_order(true).into_iter().rev() {
        seed(&app, u, period, bucket, spent(1)).await;
    }
    let conn = app.services.db.conn().expect("conn");
    let starts = PeriodStarts {
        daily: TODAY,
        monthly: MONTH,
    };
    let rows = repo::load_current(&conn, &repo::owner_scope(u.tenant, u.user), &starts, true)
        .await
        .expect("rows");
    let order: Vec<(String, String)> = rows
        .into_iter()
        .map(|r| (r.period_type, r.bucket))
        .collect();
    let expected: Vec<(String, String)> = lock_order(true)
        .into_iter()
        .map(|(p, b)| (p.as_str().to_owned(), b.as_str().to_owned()))
        .collect();
    assert_eq!(order, expected);
}

#[test]
fn quota_warning_serializes_for_the_done_event() {
    let warn = QuotaWarning {
        tier: Bucket::Premium,
        period: Period::Daily,
        remaining_percentage: 0,
        warning: true,
        exhausted: true,
        next_reset: Some(datetime!(2026-10-05 00:00:00 UTC)),
    };
    let quiet = QuotaWarning {
        tier: Bucket::Total,
        period: Period::Monthly,
        remaining_percentage: 91,
        warning: false,
        exhausted: false,
        next_reset: None,
    };
    assert_eq!(
        serde_json::to_value([warn, quiet]).unwrap(),
        serde_json::json!([
            {
                "tier": "premium",
                "period": "daily",
                "remaining_percentage": 0,
                "warning": true,
                "exhausted": true,
                "next_reset": "2026-10-05T00:00:00Z"
            },
            {
                "tier": "total",
                "period": "monthly",
                "remaining_percentage": 91,
                "warning": false,
                "exhausted": false
            }
        ])
    );
}

#[tokio::test]
async fn release_beyond_the_booked_reserve_clamps_at_zero() {
    let app = app().await;
    let u = new_user();
    seed_reserved(&app, u, 1_000, false).await;
    // The turn's reserve (1_500_000) exceeds what the rows hold (e.g. rows reset externally).
    settle(&app, settle_input(u, SettlementMethod::Released)).await;
    for period in Period::ALL {
        assert_eq!(credits(&app, u, period, Bucket::Total).await, (0, 0));
    }
}
