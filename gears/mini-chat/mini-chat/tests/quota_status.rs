#![allow(clippy::unwrap_used, clippy::expect_used)]

//! Quota status API (`GET /mini-chat/v1/quota/status`) and the quota
//! service's reserve / settle paths over the database.

mod common;

use std::sync::Arc;

use axum::http::StatusCode;
use common::{PdpMode, TestApp, premium_model, quota_row, standard_model, tenant_scope};
use mini_chat::domain::billing::{Settlement, SettlementMethod, TurnReserve};
use mini_chat::domain::error::{DomainError, QuotaScope};
use mini_chat::domain::estimation::{ReserveInputs, candidate_reserve};
use mini_chat::domain::services::quota_service::{
    Bucket, Period, PreflightDecision, SettleInput, period_starts,
};
use mini_chat::infra::db::entity::quota_usage;
use mini_chat::infra::db::repos::QuotaUsageRepo;
use mini_chat_sdk::{KillSwitches, ModelTier, TierLimits, UserLimits};
use time::macros::date;
use time::{Date, OffsetDateTime};
use uuid::Uuid;

const STATUS: &str = "/mini-chat/v1/quota/status";

fn ids() -> (Uuid, Uuid) {
    (Uuid::new_v4(), Uuid::new_v4())
}

/// The harness clock (`CLOCK_START`) is 2025-10-09T08:53:20Z.
const TODAY: Date = date!(2025 - 10 - 09);
const MONTH: Date = date!(2025 - 10 - 01);

fn row(
    tenant: Uuid,
    user: Uuid,
    period: &str,
    start: Date,
    bucket: &str,
    spent: i64,
    reserved: i64,
) -> quota_usage::Model {
    let mut r = quota_row(tenant, user, bucket);
    period.clone_into(&mut r.period_type);
    r.period_start = start;
    r.spent_credits_micro = spent;
    r.reserved_credits_micro = reserved;
    r
}

async fn seed(app: &TestApp, rows: Vec<quota_usage::Model>) {
    let conn = app.db.conn().unwrap();
    for r in rows {
        let scope = tenant_scope(r.tenant_id, r.user_id);
        QuotaUsageRepo.insert(&conn, &scope, r).await.unwrap();
    }
}

fn limits(std_d: i64, std_m: i64, prem_d: i64, prem_m: i64) -> (TierLimits, TierLimits) {
    (
        TierLimits {
            limit_daily_credits_micro: std_d,
            limit_monthly_credits_micro: std_m,
        },
        TierLimits {
            limit_daily_credits_micro: prem_d,
            limit_monthly_credits_micro: prem_m,
        },
    )
}

#[tokio::test]
async fn status_reports_premium_and_total_daily_monthly() {
    let (std_l, prem_l) = limits(2_000_000, 30_000_000, 1_000_000, 10_000_000);
    let app = TestApp::builder().limits(std_l, prem_l).build().await;
    let (user, tenant) = ids();
    let (other_user, other_tenant) = ids();
    seed(
        &app,
        vec![
            row(
                tenant,
                user,
                "daily",
                TODAY,
                "tier:premium",
                800_000,
                50_000,
            ),
            row(tenant, user, "monthly", MONTH, "tier:premium", 1_000_000, 0),
            row(tenant, user, "daily", TODAY, "total", 2_400_000, 100_000),
            row(tenant, user, "monthly", MONTH, "total", 10_000_001, 0),
            // Ignored: yesterday, last month, another user, another tenant.
            row(tenant, user, "daily", date!(2025 - 10 - 08), "total", 9, 9),
            row(
                tenant,
                user,
                "monthly",
                date!(2025 - 09 - 01),
                "total",
                9,
                9,
            ),
            row(tenant, other_user, "daily", TODAY, "total", 1_999_999, 0),
            row(other_tenant, user, "daily", TODAY, "total", 1_999_999, 0),
        ],
    )
    .await;

    let resp = app.as_user(user, tenant).get(STATUS).await;
    assert_eq!(resp.status, StatusCode::OK, "{}", resp.text());
    let body = resp.json();

    assert_eq!(body["warning_threshold_pct"], 80);
    let tiers = body["tiers"].as_array().unwrap();
    assert_eq!(tiers.len(), 2);
    assert_eq!(tiers[0]["tier"], "premium");
    assert_eq!(tiers[1]["tier"], "total");

    let p = &tiers[0]["periods"];
    assert_eq!(
        p[0],
        serde_json::json!({
            "period": "daily",
            "limit_credits_micro": 1_000_000,
            "used_credits_micro": 850_000,
            "remaining_credits_micro": 150_000,
            "remaining_percentage": 15,
            "next_reset": "2025-10-10T00:00:00Z",
            "warning": true,
            "exhausted": false
        })
    );
    assert_eq!(
        p[1],
        serde_json::json!({
            "period": "monthly",
            "limit_credits_micro": 10_000_000,
            "used_credits_micro": 1_000_000,
            "remaining_credits_micro": 9_000_000,
            "remaining_percentage": 90,
            "next_reset": "2025-11-01T00:00:00Z",
            "warning": false,
            "exhausted": false
        })
    );
    let t = &tiers[1]["periods"];
    assert_eq!(
        t[0],
        serde_json::json!({
            "period": "daily",
            "limit_credits_micro": 2_000_000,
            "used_credits_micro": 2_500_000,
            "remaining_credits_micro": 0,
            "remaining_percentage": 0,
            "next_reset": "2025-10-10T00:00:00Z",
            "warning": true,
            "exhausted": true
        })
    );
    // floor(19_999_999 * 100 / 30_000_000) = 66
    assert_eq!(t[1]["period"], "monthly");
    assert_eq!(t[1]["used_credits_micro"], 10_000_001);
    assert_eq!(t[1]["remaining_credits_micro"], 19_999_999);
    assert_eq!(t[1]["remaining_percentage"], 66);
    assert_eq!(t[1]["warning"], false);

    // A user without rows sees everything unused.
    let fresh = app.as_user(Uuid::new_v4(), tenant).get(STATUS).await.json();
    assert_eq!(fresh["tiers"][1]["periods"][0]["used_credits_micro"], 0);
    assert_eq!(fresh["tiers"][1]["periods"][0]["remaining_percentage"], 100);

    // Registered with the OpenAPI operation id and response schema.
    let ops = app.operations();
    let op = ops
        .iter()
        .find(|op| op.operation_id.as_deref() == Some("mini_chat.get_quota_status"))
        .expect("quota status operation");
    assert_eq!(op.path, STATUS);
    assert_eq!(op.tags, ["Mini Chat Quotas"]);
    let ok = op.responses.iter().find(|r| r.status == 200).expect("200");
    assert!(format!("{:?}", ok.schema).contains("QuotaStatusResponse"));
}

#[tokio::test]
async fn status_requires_user_quota_read() {
    let app = TestApp::builder().build().await;
    let (user, tenant) = ids();
    app.as_user(user, tenant).get(STATUS).await;
    let req = app.pdp.last_request();
    assert_eq!(
        req.resource.resource_type,
        "gts.cf.core.ai_chat.user_quota.v1~cf.core.mini_chat.user_quota.v1~"
    );
    assert_eq!(req.action.name, "read");

    app.pdp.set_mode(PdpMode::Deny);
    let resp = app.as_user(user, tenant).get(STATUS).await;
    assert_eq!(resp.status, StatusCode::FORBIDDEN, "{}", resp.text());
}

#[tokio::test]
async fn limit_le_zero_period_omitted() {
    let (std_l, prem_l) = limits(0, 5_000_000, 0, -1);
    let app = TestApp::builder().limits(std_l, prem_l).build().await;
    let (user, tenant) = ids();

    let body = app.as_user(user, tenant).get(STATUS).await.json();
    let tiers = body["tiers"].as_array().unwrap();
    assert_eq!(tiers[0]["tier"], "premium");
    assert_eq!(tiers[0]["periods"], serde_json::json!([]));
    assert_eq!(tiers[1]["tier"], "total");
    let periods = tiers[1]["periods"].as_array().unwrap();
    assert_eq!(periods.len(), 1);
    assert_eq!(periods[0]["period"], "monthly");
    assert_eq!(periods[0]["limit_credits_micro"], 5_000_000);
}

fn user_limits(user: Uuid, (standard, premium): (TierLimits, TierLimits)) -> UserLimits {
    UserLimits {
        user_id: user,
        policy_version: 1,
        standard,
        premium,
    }
}

fn now(app: &TestApp) -> OffsetDateTime {
    use mini_chat::domain::clock::Clock;
    app.clock.now()
}

async fn preflight(
    app: &TestApp,
    tenant: Uuid,
    user: Uuid,
    model: &str,
    limits: &UserLimits,
    inputs: &ReserveInputs,
) -> Result<PreflightDecision, DomainError> {
    let snap = app.policy.snapshot();
    app.services
        .quota
        .preflight(tenant, user, model, &snap, limits, inputs, now(app))
        .await
}

async fn reserve(
    app: &TestApp,
    tenant: Uuid,
    user: Uuid,
    d: &PreflightDecision,
    limits: &UserLimits,
) -> Result<(), DomainError> {
    let quota = Arc::clone(&app.services.quota);
    let d = d.clone();
    let limits = *limits;
    app.db
        .transaction(move |tx| {
            Box::pin(async move { quota.reserve(tx, tenant, user, &d, &limits).await })
        })
        .await
}

async fn rows(app: &TestApp, tenant: Uuid, user: Uuid) -> Vec<quota_usage::Model> {
    let conn = app.db.conn().unwrap();
    let mut rows = QuotaUsageRepo
        .list_period_rows(
            &conn,
            &tenant_scope(tenant, user),
            &[("daily", TODAY), ("monthly", MONTH)],
        )
        .await
        .unwrap();
    rows.sort_by(|a, b| (&a.bucket, &a.period_type).cmp(&(&b.bucket, &b.period_type)));
    rows
}

fn summary(rows: &[quota_usage::Model]) -> Vec<(String, String, i64, i64)> {
    rows.iter()
        .map(|r| {
            (
                r.bucket.clone(),
                r.period_type.clone(),
                r.spent_credits_micro,
                r.reserved_credits_micro,
            )
        })
        .collect()
}

#[tokio::test]
async fn reserve_recheck_rejects_concurrent_overbooking() {
    let app = TestApp::builder().build().await;
    let (user, tenant) = ids();
    let inputs = ReserveInputs {
        message_bytes: 1_000,
        ..ReserveInputs::default()
    };
    let r = candidate_reserve(
        &standard_model("s1"),
        &inputs,
        &KillSwitches::default(),
        app.config.streaming.max_output_tokens,
        app.config.estimation_budgets.minimal_generation_floor,
    )
    .reserved_credits_micro;
    // Room for one reserve, not two.
    let limits = user_limits(user, limits(2 * r - 1, 100 * r, r, 100 * r));

    // Both requests pass preflight before either books its reserve.
    let first = preflight(&app, tenant, user, "s1", &limits, &inputs)
        .await
        .unwrap();
    let second = preflight(&app, tenant, user, "s1", &limits, &inputs)
        .await
        .unwrap();
    assert_eq!(first.effective.id, "s1");
    assert_eq!(first.plan.reserved_credits_micro, r);
    assert_eq!(first.periods, period_starts(now(&app)));
    assert_eq!(first.policy_version, 1);

    reserve(&app, tenant, user, &first, &limits).await.unwrap();
    let booked = rows(&app, tenant, user).await;
    assert_eq!(
        summary(&booked),
        [
            ("total".to_owned(), "daily".to_owned(), 0, r),
            ("total".to_owned(), "monthly".to_owned(), 0, r),
        ]
    );

    assert_eq!(
        reserve(&app, tenant, user, &second, &limits).await,
        Err(DomainError::QuotaExceeded(QuotaScope::Tokens))
    );
    // Rolled back: the rows are exactly as after the first reserve.
    assert_eq!(rows(&app, tenant, user).await, booked);

    // A later preflight sees the booked reserve.
    assert_eq!(
        preflight(&app, tenant, user, "s1", &limits, &inputs)
            .await
            .unwrap_err(),
        DomainError::QuotaExceeded(QuotaScope::Tokens)
    );
}

#[tokio::test]
async fn preflight_reads_rows_for_downgrade_and_tool_quota() {
    let app = TestApp::builder().build().await;
    let (user, tenant) = ids();
    let limits = user_limits(user, limits(1_000_000_000, 10_000_000_000, 100, 1_000));
    seed(
        &app,
        vec![{
            let mut r = row(tenant, user, "daily", TODAY, "total", 0, 0);
            r.web_search_calls = 75;
            r
        }],
    )
    .await;

    // Premium limit 100 cannot hold any reserve -> standard default.
    let d = preflight(&app, tenant, user, "p1", &limits, &ReserveInputs::default())
        .await
        .unwrap();
    assert_eq!(d.effective.id, "s1");
    assert_eq!(d.effective_tier, ModelTier::Standard);
    assert_eq!(d.downgrade_reason, Some("premium_quota_exhausted"));

    let ws = ReserveInputs {
        web_search_requested: true,
        ..ReserveInputs::default()
    };
    assert_eq!(
        preflight(&app, tenant, user, "s1", &limits, &ws)
            .await
            .unwrap_err(),
        DomainError::QuotaExceeded(QuotaScope::WebSearch)
    );
}

async fn settle(app: &TestApp, s: SettleInput) -> u64 {
    let quota = Arc::clone(&app.services.quota);
    app.db
        .transaction(move |tx| Box::pin(async move { quota.settle(tx, &s).await }))
        .await
        .unwrap()
        .rows_updated
}

#[tokio::test]
async fn settle_moves_reserve_to_spent() {
    let app = TestApp::builder()
        .catalog(vec![premium_model("p1"), standard_model("s1")])
        .build()
        .await;
    let (user, tenant) = ids();
    let limits = user_limits(
        user,
        limits(1_000_000_000, 10_000_000_000, 500_000_000, 5_000_000_000),
    );

    let d = preflight(&app, tenant, user, "p1", &limits, &ReserveInputs::default())
        .await
        .unwrap();
    assert_eq!(d.effective_tier, ModelTier::Premium);
    let r = d.plan.reserved_credits_micro;
    reserve(&app, tenant, user, &d, &limits).await.unwrap();
    reserve(&app, tenant, user, &d, &limits).await.unwrap();
    assert_eq!(rows(&app, tenant, user).await.len(), 4);

    let turn = TurnReserve {
        reserve_tokens: d.plan.reserve_tokens,
        max_output_tokens_applied: d.plan.max_output_tokens_applied,
        reserved_credits_micro: r,
        minimal_generation_floor_applied: d.plan.minimal_generation_floor_applied,
    };
    // Actual settlement of turn one.
    let n = settle(
        &app,
        SettleInput {
            tenant,
            user,
            effective_tier: ModelTier::Premium,
            periods: d.periods,
            turn,
            method: SettlementMethod::Actual,
            settlement: Settlement {
                committed_credits_micro: 700,
                actual_tokens_for_telemetry: Some((100, 200)),
                overshoot: false,
            },
            web_search_calls: 1,
            code_interpreter_calls: 2,
        },
    )
    .await;
    assert_eq!(n, 4);
    // Estimated settlement of turn two (calls counted, tokens not).
    settle(
        &app,
        SettleInput {
            tenant,
            user,
            effective_tier: ModelTier::Premium,
            periods: d.periods,
            turn,
            method: SettlementMethod::Estimated,
            settlement: Settlement {
                committed_credits_micro: 300,
                actual_tokens_for_telemetry: None,
                overshoot: false,
            },
            web_search_calls: 1,
            code_interpreter_calls: 0,
        },
    )
    .await;

    for r in rows(&app, tenant, user).await {
        let what = format!("{} {}", r.bucket, r.period_type);
        assert_eq!(r.reserved_credits_micro, 0, "{what}");
        assert_eq!(r.spent_credits_micro, 1_000, "{what}");
        assert_eq!(r.calls, 2, "{what}");
        if r.bucket == "total" {
            assert_eq!((r.input_tokens, r.output_tokens), (100, 200), "{what}");
            assert_eq!(r.web_search_calls, 2, "{what}");
            assert_eq!(r.code_interpreter_calls, 2, "{what}");
        } else {
            assert_eq!((r.input_tokens, r.output_tokens), (0, 0), "{what}");
            assert_eq!(
                (r.web_search_calls, r.code_interpreter_calls),
                (0, 0),
                "{what}"
            );
        }
    }

    // A released settlement of a standard turn: total rows only, no tool calls.
    let s = preflight(&app, tenant, user, "s1", &limits, &ReserveInputs::default())
        .await
        .unwrap();
    reserve(&app, tenant, user, &s, &limits).await.unwrap();
    let n = settle(
        &app,
        SettleInput {
            tenant,
            user,
            effective_tier: ModelTier::Standard,
            periods: s.periods,
            turn: TurnReserve {
                reserved_credits_micro: s.plan.reserved_credits_micro,
                ..turn
            },
            method: SettlementMethod::Released,
            settlement: Settlement {
                committed_credits_micro: 0,
                actual_tokens_for_telemetry: None,
                overshoot: false,
            },
            web_search_calls: 5,
            code_interpreter_calls: 5,
        },
    )
    .await;
    assert_eq!(n, 2);
    for r in rows(&app, tenant, user).await {
        let what = format!("{} {}", r.bucket, r.period_type);
        assert_eq!(r.reserved_credits_micro, 0, "{what}");
        let (calls, ws) = if r.bucket == "total" { (3, 2) } else { (2, 0) };
        assert_eq!(r.calls, calls, "{what}");
        assert_eq!(r.web_search_calls, ws, "{what}");
    }
}

#[tokio::test]
async fn warnings_follow_status_math() {
    let app = TestApp::builder().build().await;
    let (user, tenant) = ids();
    let limits = user_limits(user, limits(1_000, 0, 1_000, 100_000));
    seed(
        &app,
        vec![
            row(tenant, user, "daily", TODAY, "tier:premium", 850, 0),
            row(tenant, user, "daily", TODAY, "total", 1_000, 0),
        ],
    )
    .await;

    let conn = app.db.conn().unwrap();
    let w = app
        .services
        .quota
        .warnings(&conn, tenant, user, &limits, now(&app))
        .await
        .unwrap();
    // total monthly (limit 0) is skipped; order premium then total, daily then monthly.
    let got: Vec<_> = w
        .iter()
        .map(|w| {
            (
                w.tier,
                w.period,
                w.remaining_percentage,
                w.warning,
                w.exhausted,
                w.next_reset.map(|t| t.to_string()),
            )
        })
        .collect();
    let tomorrow = Some("2025-10-10 0:00:00.0 +00:00:00".to_owned());
    assert_eq!(
        got,
        [
            (
                Bucket::Premium,
                Period::Daily,
                15,
                true,
                false,
                tomorrow.clone()
            ),
            (Bucket::Premium, Period::Monthly, 100, false, false, None),
            (Bucket::Total, Period::Daily, 0, true, true, tomorrow),
        ]
    );
}
