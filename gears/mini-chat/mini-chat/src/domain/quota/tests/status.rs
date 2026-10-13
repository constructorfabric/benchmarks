//! Quota warnings and `GET /quota/status`.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use http::StatusCode;
use mini_chat_sdk::TierLimits;
use time::format_description::well_known::Rfc3339;

use super::put_row;
use crate::domain::quota::{
    BUCKET_PREMIUM, BUCKET_TOTAL, PERIOD_DAILY, PERIOD_MONTHLY, QuotaWarning, next_daily_reset, next_monthly_reset,
    period_starts, quota_warnings,
};
use crate::testing::{TENANT_A, TestApp, USER_A1, ctx_a1};

const URI: &str = "/mini-chat/v1/quota/status";

async fn warnings(t: &TestApp) -> Vec<QuotaWarning> {
    let app = Arc::clone(&t.app);
    let limits = t.app.policy.user_limits(USER_A1, 1).await.unwrap();
    t.app
        .db
        .transaction(move |tx| {
            Box::pin(async move { quota_warnings(&app, tx, TENANT_A, USER_A1, &limits, crate::clock::now()).await })
        })
        .await
        .unwrap()
}

#[tokio::test]
async fn status_without_usage() {
    let t = TestApp::new().await;
    let (st, _, body) = t.call(&ctx_a1(), "GET", URI, None).await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert_eq!(body["warning_threshold_pct"], 80);
    let tiers = body["tiers"].as_array().unwrap();
    assert_eq!(tiers.len(), 2);
    assert_eq!(tiers[0]["tier"], "premium");
    assert_eq!(tiers[1]["tier"], "total");
    let total_daily = &tiers[1]["periods"][0];
    assert_eq!(total_daily["period"], "daily");
    assert_eq!(total_daily["limit_credits_micro"], 100_000_000);
    assert_eq!(total_daily["used_credits_micro"], 0);
    assert_eq!(total_daily["remaining_credits_micro"], 100_000_000);
    assert_eq!(total_daily["remaining_percentage"], 100);
    assert_eq!(total_daily["warning"], false);
    assert_eq!(total_daily["exhausted"], false);
    let now = crate::clock::now();
    assert_eq!(total_daily["next_reset"], next_daily_reset(now).format(&Rfc3339).unwrap());
    let total_monthly = &tiers[1]["periods"][1];
    assert_eq!(total_monthly["period"], "monthly");
    assert_eq!(total_monthly["limit_credits_micro"], 1_000_000_000);
    assert_eq!(total_monthly["next_reset"], next_monthly_reset(now).format(&Rfc3339).unwrap());
    assert_eq!(tiers[0]["periods"][0]["limit_credits_micro"], 50_000_000);
    // authz was consulted with the quota resource
    assert!(t.authz.calls.lock().unwrap().iter().any(|(a, _)| a == "quota:read"));
}

#[tokio::test]
async fn status_percentages_and_flags() {
    let t = TestApp::new().await;
    let p = period_starts(crate::clock::now());
    put_row(&t.app, PERIOD_DAILY, p.daily, BUCKET_TOTAL, |r| {
        r.spent_credits_micro = 85_000_000;
        r.reserved_credits_micro = 5_000_000;
    })
    .await;
    // 0.8 % left -> floored to 0 -> exhausted
    put_row(&t.app, PERIOD_DAILY, p.daily, BUCKET_PREMIUM, |r| r.spent_credits_micro = 49_600_000).await;
    // 25 % left -> no warning at 80 % threshold (warning at <= 20 %)
    put_row(&t.app, PERIOD_MONTHLY, p.monthly, BUCKET_TOTAL, |r| r.spent_credits_micro = 750_000_000).await;
    // over the limit: remaining clamps at zero
    put_row(&t.app, PERIOD_MONTHLY, p.monthly, BUCKET_PREMIUM, |r| r.spent_credits_micro = 600_000_000).await;
    let (st, _, body) = t.call(&ctx_a1(), "GET", URI, None).await;
    assert_eq!(st, StatusCode::OK, "{body}");
    let premium = &body["tiers"][0]["periods"];
    let total = &body["tiers"][1]["periods"];
    assert_eq!(total[0]["used_credits_micro"], 90_000_000);
    assert_eq!(total[0]["remaining_credits_micro"], 10_000_000);
    assert_eq!(total[0]["remaining_percentage"], 10);
    assert_eq!(total[0]["warning"], true);
    assert_eq!(total[0]["exhausted"], false);
    assert_eq!(premium[0]["remaining_credits_micro"], 400_000);
    assert_eq!(premium[0]["remaining_percentage"], 0);
    assert_eq!(premium[0]["warning"], true);
    assert_eq!(premium[0]["exhausted"], true);
    assert_eq!(total[1]["remaining_percentage"], 25);
    assert_eq!(total[1]["warning"], false);
    assert_eq!(premium[1]["remaining_credits_micro"], 0);
    assert_eq!(premium[1]["used_credits_micro"], 600_000_000);
    assert_eq!(premium[1]["exhausted"], true);

    let w = warnings(&t).await;
    assert_eq!(w.len(), 4);
    let find = |tier: &str, period: &str| w.iter().find(|x| x.tier == tier && x.period == period).unwrap().clone();
    let td = find("total", "daily");
    assert_eq!((td.remaining_percentage, td.warning, td.exhausted), (10, true, false));
    assert!(td.next_reset.is_some());
    let tm = find("total", "monthly");
    assert_eq!((tm.remaining_percentage, tm.warning, tm.exhausted), (25, false, false));
    assert!(tm.next_reset.is_none(), "next_reset only on warning / exhausted");
    let pd = find("premium", "daily");
    assert!(pd.exhausted);
    let json = serde_json::to_value(&tm).unwrap();
    assert!(json.get("next_reset").is_none());
    let json = serde_json::to_value(&td).unwrap();
    assert!(json["next_reset"].is_string());
}

#[tokio::test]
async fn warning_threshold_boundary() {
    let t = TestApp::with_config(|c| c.quota.warning_threshold_pct = 50).await;
    let p = period_starts(crate::clock::now());
    // exactly 50 % left -> warning (<= 100 - 50)
    put_row(&t.app, PERIOD_DAILY, p.daily, BUCKET_TOTAL, |r| r.spent_credits_micro = 50_000_000).await;
    // 51 % left -> no warning
    put_row(&t.app, PERIOD_MONTHLY, p.monthly, BUCKET_TOTAL, |r| r.spent_credits_micro = 490_000_000).await;
    let (_, _, body) = t.call(&ctx_a1(), "GET", URI, None).await;
    assert_eq!(body["warning_threshold_pct"], 50);
    let total = &body["tiers"][1]["periods"];
    assert_eq!(total[0]["remaining_percentage"], 50);
    assert_eq!(total[0]["warning"], true);
    assert_eq!(total[1]["remaining_percentage"], 51);
    assert_eq!(total[1]["warning"], false);
}

#[tokio::test]
async fn periods_with_non_positive_limit_are_skipped() {
    let t = TestApp::new().await;
    t.policy.set_limits(
        TierLimits { limit_daily_credits_micro: 0, limit_monthly_credits_micro: 1_000_000 },
        TierLimits { limit_daily_credits_micro: 0, limit_monthly_credits_micro: -5 },
    );
    let (st, _, body) = t.call(&ctx_a1(), "GET", URI, None).await;
    assert_eq!(st, StatusCode::OK);
    let tiers = body["tiers"].as_array().unwrap();
    assert_eq!(tiers.len(), 1, "{body}");
    assert_eq!(tiers[0]["tier"], "total");
    let periods = tiers[0]["periods"].as_array().unwrap();
    assert_eq!(periods.len(), 1);
    assert_eq!(periods[0]["period"], "monthly");
    let w = warnings(&t).await;
    assert_eq!(w.len(), 1);
    assert_eq!((w[0].tier.as_str(), w[0].period.as_str()), ("total", "monthly"));
}

#[tokio::test]
async fn status_is_per_user() {
    let t = TestApp::new().await;
    let p = period_starts(crate::clock::now());
    put_row(&t.app, PERIOD_DAILY, p.daily, BUCKET_TOTAL, |r| r.spent_credits_micro = 10_000_000).await;
    let other = crate::testing::ctx(crate::testing::USER_A2, TENANT_A);
    let (_, _, body) = t.call(&other, "GET", URI, None).await;
    assert_eq!(body["tiers"][1]["periods"][0]["used_credits_micro"], 0);
    let (_, _, body) = t.call(&ctx_a1(), "GET", URI, None).await;
    assert_eq!(body["tiers"][1]["periods"][0]["used_credits_micro"], 10_000_000);
}

#[tokio::test]
async fn status_denied_and_pdp_down() {
    let t = TestApp::new().await;
    t.authz.deny.store(true, Ordering::SeqCst);
    let (st, _, _) = t.call(&ctx_a1(), "GET", URI, None).await;
    assert_eq!(st, StatusCode::FORBIDDEN);
    t.authz.deny.store(false, Ordering::SeqCst);
    t.authz.unavailable.store(true, Ordering::SeqCst);
    let (st, headers, _) = t.call(&ctx_a1(), "GET", URI, None).await;
    assert_eq!(st, StatusCode::SERVICE_UNAVAILABLE);
    assert!(headers.get("retry-after").is_some());
}
