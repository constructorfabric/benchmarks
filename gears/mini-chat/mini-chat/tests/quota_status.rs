#![allow(clippy::unwrap_used, clippy::expect_used)]

//! Quota status API `GET {url_prefix}/v1/quota/status` (DESIGN "Quota Status Endpoint (P1)", "Quota
//! Warning Thresholds (P1)", "User Quota Resource Type", the PEP table of section 3.8).
//!
//! The usage the status reports is `spent + reserved` of the `quota_usage` rows of the caller: the
//! tests drive it either through real turns or by seeding rows with SQL, and compare the status with
//! the rows.

mod common;
mod live;

use common::{
    QUOTA_STATUS_PATH, STANDARD_MODEL, Seed, USER_QUOTA_RESOURCE_TYPE, assert_problem, chat_on,
    complete_turn, default_usage_credits, env_with_authz, env_with_user_limits, expect_quota_row,
    expect_status, expect_status_period, get, merge, new_chat, parse_ts, policy, quota_rows,
    quota_status, quota_status_ok, seed_quota, seed_quota_at, status_period, status_tier_names,
    two_model_catalog, used_of,
};
use http::StatusCode;
use mini_chat::test_support::{AuthzMode, TestEnv, TestEnvBuilder, TestUser};
use serde_json::{Value, json};
use time::{Date, Duration, OffsetDateTime, Time};
use uuid::Uuid;

const PERIOD_MEMBERS: [&str; 8] = [
    "period",
    "limit_credits_micro",
    "used_credits_micro",
    "remaining_credits_micro",
    "remaining_percentage",
    "next_reset",
    "warning",
    "exhausted",
];

/// Total (overall) and premium limits that differ per bucket and period.
const TOTAL_LIMITS: (i64, i64) = (4_000_000, 40_000_000);
const PREMIUM_LIMITS: (i64, i64) = (2_000_000, 20_000_000);

async fn env_with_default_limits() -> TestEnv {
    env_with_user_limits(TOTAL_LIMITS, PREMIUM_LIMITS).await
}

fn first_of_next_month(date: Date) -> Date {
    let (year, month) = (date.year(), date.month());
    if month == time::Month::December {
        Date::from_calendar_date(year + 1, time::Month::January, 1).unwrap()
    } else {
        Date::from_calendar_date(year, month.next(), 1).unwrap()
    }
}

// ===========================================================================
// Response shape
// ===========================================================================

#[tokio::test]
async fn the_status_is_a_json_object_with_tiers_and_the_warning_threshold() {
    let env = TestEnv::new().await;

    let resp = quota_status(&env, &TestUser::new()).await;

    expect_status(&resp, StatusCode::OK);
    assert!(
        common::content_type(&resp).starts_with("application/json"),
        "{}",
        common::content_type(&resp)
    );
    let status = resp.json();
    let mut keys: Vec<&str> = status
        .as_object()
        .expect("an object")
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    assert_eq!(keys, ["tiers", "warning_threshold_pct"], "{status}");
    assert_eq!(
        status["warning_threshold_pct"],
        json!(80),
        "the configured default of quota.warning_threshold_pct"
    );
}

#[tokio::test]
async fn the_status_lists_the_premium_and_total_tiers_with_daily_and_monthly_periods() {
    let env = TestEnv::new().await;

    let status = quota_status_ok(&env, &TestUser::new()).await;

    let mut tiers = status_tier_names(&status);
    tiers.sort();
    assert_eq!(tiers, ["premium", "total"], "{status}");
    for tier in ["premium", "total"] {
        for period in ["daily", "monthly"] {
            expect_status_period(&status, tier, period);
        }
        let periods = status["tiers"]
            .as_array()
            .unwrap()
            .iter()
            .find(|entry| entry["tier"] == json!(tier))
            .unwrap()["periods"]
            .as_array()
            .unwrap()
            .len();
        assert_eq!(periods, 2, "{tier}: exactly daily and monthly");
    }
    let tier_keys: Vec<&str> = status["tiers"][0]
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert!(
        tier_keys.len() == 2 && tier_keys.contains(&"tier") && tier_keys.contains(&"periods"),
        "a tier entry is {{tier, periods}}: {}",
        status["tiers"][0]
    );
}

#[tokio::test]
async fn every_period_entry_has_exactly_the_documented_members_and_types() {
    let env = TestEnv::new().await;

    let status = quota_status_ok(&env, &TestUser::new()).await;

    for tier in status["tiers"].as_array().unwrap() {
        for period in tier["periods"].as_array().unwrap() {
            let mut keys: Vec<&str> = period
                .as_object()
                .unwrap()
                .keys()
                .map(String::as_str)
                .collect();
            keys.sort_unstable();
            let mut expected = PERIOD_MEMBERS.to_vec();
            expected.sort_unstable();
            assert_eq!(keys, expected, "{period}");
            assert!(period["period"].is_string(), "{period}");
            for number in [
                "limit_credits_micro",
                "used_credits_micro",
                "remaining_credits_micro",
                "remaining_percentage",
            ] {
                assert!(
                    period[number].is_i64(),
                    "`{number}` is an integer: {period}"
                );
            }
            assert!(period["warning"].is_boolean(), "{period}");
            assert!(period["exhausted"].is_boolean(), "{period}");
            let _reset = parse_ts(&period["next_reset"]);
        }
    }
}

#[tokio::test]
async fn the_limits_are_the_policy_limits_of_each_tier_and_period() {
    let env = env_with_default_limits().await;

    let status = quota_status_ok(&env, &TestUser::new()).await;

    let limit = |tier: &str, period: &str| {
        expect_status_period(&status, tier, period)["limit_credits_micro"].clone()
    };
    assert_eq!(limit("total", "daily"), json!(TOTAL_LIMITS.0));
    assert_eq!(limit("total", "monthly"), json!(TOTAL_LIMITS.1));
    assert_eq!(limit("premium", "daily"), json!(PREMIUM_LIMITS.0));
    assert_eq!(limit("premium", "monthly"), json!(PREMIUM_LIMITS.1));
}

#[tokio::test]
async fn a_user_without_usage_has_everything_remaining() {
    let env = env_with_default_limits().await;

    let status = quota_status_ok(&env, &TestUser::new()).await;

    for tier in ["premium", "total"] {
        for period in ["daily", "monthly"] {
            let entry = expect_status_period(&status, tier, period);
            assert_eq!(entry["used_credits_micro"], json!(0), "{tier}/{period}");
            assert_eq!(
                entry["remaining_credits_micro"], entry["limit_credits_micro"],
                "{tier}/{period}"
            );
            assert_eq!(entry["remaining_percentage"], json!(100), "{tier}/{period}");
            assert_eq!(entry["warning"], json!(false), "{tier}/{period}");
            assert_eq!(entry["exhausted"], json!(false), "{tier}/{period}");
        }
    }
}

#[tokio::test]
async fn reading_the_status_writes_nothing() {
    let env = TestEnv::new().await;
    let user = TestUser::new();

    quota_status_ok(&env, &user).await;
    quota_status_ok(&env, &user).await;

    assert!(
        quota_rows(&env, &user).await.is_empty(),
        "the status is a read"
    );
}

#[tokio::test]
async fn next_reset_is_midnight_utc_tomorrow_for_daily_and_the_first_of_next_month_for_monthly() {
    let env = TestEnv::new().await;
    let user = TestUser::new();
    let before = OffsetDateTime::now_utc();

    let status = quota_status_ok(&env, &user).await;

    let after = OffsetDateTime::now_utc();
    let expected_daily = |at: OffsetDateTime| {
        (at.date() + Duration::days(1))
            .with_time(Time::MIDNIGHT)
            .assume_utc()
    };
    let expected_monthly = |at: OffsetDateTime| {
        first_of_next_month(at.date())
            .with_time(Time::MIDNIGHT)
            .assume_utc()
    };
    for tier in ["premium", "total"] {
        let daily = parse_ts(&expect_status_period(&status, tier, "daily")["next_reset"]);
        assert!(
            daily == expected_daily(before) || daily == expected_daily(after),
            "{tier} daily next_reset {daily}"
        );
        let monthly = parse_ts(&expect_status_period(&status, tier, "monthly")["next_reset"]);
        assert!(
            monthly == expected_monthly(before) || monthly == expected_monthly(after),
            "{tier} monthly next_reset {monthly}"
        );
        assert_eq!(daily.offset(), time::UtcOffset::UTC);
    }
}

// ===========================================================================
// Accuracy against the quota_usage rows
// ===========================================================================

#[allow(
    clippy::integer_division,
    reason = "the remaining percentage is a floored integer percentage by definition"
)]
fn floored_percentage(remaining: i64, limit: i64) -> i64 {
    remaining * 100 / limit
}

#[tokio::test]
async fn after_a_premium_turn_both_tiers_report_the_spent_credits() {
    let env = env_with_default_limits().await;
    let user = TestUser::new();
    let chat = new_chat(&env, &user).await;
    complete_turn(&env, &user, &chat, "hello", "answer").await;
    let spent = default_usage_credits();

    let status = quota_status_ok(&env, &user).await;

    for (tier, limits) in [("total", TOTAL_LIMITS), ("premium", PREMIUM_LIMITS)] {
        for (period, limit) in [("daily", limits.0), ("monthly", limits.1)] {
            let entry = expect_status_period(&status, tier, period);
            assert_eq!(entry["used_credits_micro"], json!(spent), "{tier}/{period}");
            assert_eq!(
                entry["remaining_credits_micro"],
                json!(limit - spent),
                "{tier}/{period}"
            );
            assert_eq!(
                entry["remaining_percentage"],
                json!(floored_percentage(limit - spent, limit)),
                "{tier}/{period}: floored integer percentage"
            );
        }
    }
}

#[tokio::test]
async fn the_status_equals_spent_plus_reserved_of_every_quota_row() {
    let env = env_with_default_limits().await;
    let user = TestUser::new();
    let chat = new_chat(&env, &user).await;
    complete_turn(&env, &user, &chat, "one", "A").await;
    complete_turn(&env, &user, &chat, "two", "B").await;

    let status = quota_status_ok(&env, &user).await;

    let rows = quota_rows(&env, &user).await;
    for (tier, bucket) in [("total", "total"), ("premium", "tier:premium")] {
        for period in ["daily", "monthly"] {
            let row = expect_quota_row(&rows, period, bucket);
            assert_eq!(
                expect_status_period(&status, tier, period)["used_credits_micro"],
                json!(row.spent + row.reserved),
                "{tier}/{period}"
            );
        }
    }
    assert_eq!(
        used_of(&env, &user, "daily", "total").await,
        2 * default_usage_credits()
    );
}

#[tokio::test]
async fn a_standard_turn_is_counted_in_the_total_tier_only() {
    let env = env_with_default_limits().await;
    let user = TestUser::new();
    let chat = chat_on(&env, &user, STANDARD_MODEL).await;
    complete_turn(&env, &user, &chat, "hello", "answer").await;

    let status = quota_status_ok(&env, &user).await;

    for period in ["daily", "monthly"] {
        assert_eq!(
            expect_status_period(&status, "total", period)["used_credits_micro"],
            json!(default_usage_credits()),
            "{period}"
        );
        assert_eq!(
            expect_status_period(&status, "premium", period)["used_credits_micro"],
            json!(0),
            "{period}: a standard turn leaves the premium subcap alone"
        );
    }
}

#[tokio::test]
async fn an_in_flight_reserve_counts_as_used_and_is_replaced_by_the_charge_when_the_turn_ends() {
    let env = env_with_default_limits().await;
    let user = TestUser::new();
    let chat = new_chat(&env, &user).await;
    let request_id = Uuid::new_v4();
    let stream = live::start_hung_turn(&env, &user, &chat, request_id, "hello", &["par"]).await;
    let turn = common::turn_row(&env, &chat, request_id).await.unwrap();
    let reserve = turn.reserved_credits_micro.unwrap();

    let running = quota_status_ok(&env, &user).await;

    for tier in ["premium", "total"] {
        for period in ["daily", "monthly"] {
            let entry = expect_status_period(&running, tier, period);
            assert_eq!(
                entry["used_credits_micro"],
                json!(reserve),
                "{tier}/{period}"
            );
        }
    }
    live::disconnect_and_wait_cancelled(&env, &chat, request_id, stream).await;
    let settled = quota_status_ok(&env, &user).await;
    let charged = common::estimated_credits(
        &common::turn_row(&env, &chat, request_id).await.unwrap(),
        1_000_000,
        3_000_000,
    );
    assert!(charged < reserve);
    assert_eq!(
        expect_status_period(&settled, "total", "daily")["used_credits_micro"],
        json!(charged),
        "the reserve is released and the estimated charge is spent"
    );
}

// ===========================================================================
// Percentages, warning and exhausted
// ===========================================================================

/// `(spent, remaining_percentage, warning, exhausted)` against a daily limit of 1000 and the default
/// `warning_threshold_pct` of 80: warning when `remaining_percentage <= 20`, exhausted when 0.
const BOUNDARIES: [(i64, i64, bool, bool); 8] = [
    (0, 100, false, false),
    (790, 21, false, false),
    (800, 20, true, false),
    (801, 19, true, false),
    (990, 1, true, false),
    (991, 0, true, true),
    (1000, 0, true, true),
    (1500, 0, true, true),
];

#[tokio::test]
async fn the_percentage_is_floored_and_warning_and_exhausted_follow_their_thresholds() {
    let env = env_with_user_limits((1000, 100_000_000), (100_000_000, 1_000_000_000)).await;
    let user = TestUser::new();

    for (spent, percentage, warning, exhausted) in BOUNDARIES {
        seed_quota(&env, &user, "daily", "total", Seed::spent(spent)).await;
        let status = quota_status_ok(&env, &user).await;

        let entry = expect_status_period(&status, "total", "daily");
        assert_eq!(entry["used_credits_micro"], json!(spent), "spent {spent}");
        assert_eq!(
            entry["remaining_percentage"],
            json!(percentage),
            "spent {spent}"
        );
        assert_eq!(entry["warning"], json!(warning), "spent {spent}");
        assert_eq!(entry["exhausted"], json!(exhausted), "spent {spent}");
        assert!(
            entry["remaining_credits_micro"].as_i64().unwrap() >= 0,
            "remaining credits never go negative (spent {spent}): {entry}"
        );
        if spent <= 1000 {
            assert_eq!(
                entry["remaining_credits_micro"],
                json!(1000 - spent),
                "spent {spent}"
            );
        }
    }
}

#[tokio::test]
async fn reserved_credits_count_as_used_for_the_remaining_and_the_warning() {
    let env = env_with_user_limits((1000, 100_000_000), (100_000_000, 1_000_000_000)).await;
    let user = TestUser::new();
    seed_quota(
        &env,
        &user,
        "daily",
        "total",
        Seed::spent_reserved(500, 300),
    )
    .await;

    let status = quota_status_ok(&env, &user).await;

    let entry = expect_status_period(&status, "total", "daily");
    assert_eq!(entry["used_credits_micro"], json!(800));
    assert_eq!(entry["remaining_credits_micro"], json!(200));
    assert_eq!(entry["remaining_percentage"], json!(20));
    assert_eq!(entry["warning"], json!(true));
    assert_eq!(entry["exhausted"], json!(false));
}

#[tokio::test]
async fn a_custom_warning_threshold_is_reported_and_applied() {
    let env = TestEnvBuilder::new()
        .policy_config(merge(
            policy(two_model_catalog()),
            json!({
                "default_standard_limits": common::limits(1000, 100_000_000),
                "default_premium_limits": common::limits(100_000_000, 1_000_000_000),
            }),
        ))
        .gear_config(json!({ "quota": { "warning_threshold_pct": 50 } }))
        .build()
        .await;
    let user = TestUser::new();

    // warning when remaining_percentage <= 100 - 50.
    for (spent, warning) in [(400, false), (490, false), (500, true), (999, true)] {
        seed_quota(&env, &user, "daily", "total", Seed::spent(spent)).await;
        let status = quota_status_ok(&env, &user).await;
        assert_eq!(status["warning_threshold_pct"], json!(50));
        assert_eq!(
            expect_status_period(&status, "total", "daily")["warning"],
            json!(warning),
            "spent {spent}"
        );
    }
}

#[tokio::test]
async fn the_buckets_and_periods_are_reported_independently() {
    let env = env_with_user_limits((1000, 1000), (1000, 1000)).await;
    let user = TestUser::new();
    seed_quota(&env, &user, "daily", "tier:premium", Seed::spent(900)).await;
    seed_quota(&env, &user, "monthly", "total", Seed::spent(1000)).await;

    let status = quota_status_ok(&env, &user).await;

    let state = |tier: &str, period: &str| {
        let entry = expect_status_period(&status, tier, period);
        (
            entry["used_credits_micro"].as_i64().unwrap(),
            entry["warning"].as_bool().unwrap(),
            entry["exhausted"].as_bool().unwrap(),
        )
    };
    assert_eq!(state("premium", "daily"), (900, true, false));
    assert_eq!(state("premium", "monthly"), (0, false, false));
    assert_eq!(state("total", "daily"), (0, false, false));
    assert_eq!(state("total", "monthly"), (1000, true, true));
}

#[tokio::test]
async fn rows_of_other_periods_are_not_counted() {
    let env = env_with_user_limits((1000, 1000), (1000, 1000)).await;
    let user = TestUser::new();
    // Yesterday's day and last month's month are over.
    seed_quota_at(
        &env,
        &user,
        "daily",
        "2000-01-01",
        "total",
        Seed::spent(999),
    )
    .await;
    seed_quota_at(
        &env,
        &user,
        "monthly",
        "2000-01-01",
        "total",
        Seed::spent(999),
    )
    .await;
    seed_quota(&env, &user, "daily", "total", Seed::spent(10)).await;

    let status = quota_status_ok(&env, &user).await;

    assert_eq!(
        expect_status_period(&status, "total", "daily")["used_credits_micro"],
        json!(10)
    );
    assert_eq!(
        expect_status_period(&status, "total", "monthly")["used_credits_micro"],
        json!(0),
        "only the current month's row counts"
    );
}

// ===========================================================================
// Periods with a non-positive limit are omitted
// ===========================================================================

#[tokio::test]
async fn a_period_with_a_zero_limit_is_omitted() {
    let env = env_with_user_limits((1000, 1000), (0, 1000)).await;

    let status = quota_status_ok(&env, &TestUser::new()).await;

    assert!(
        status_period(&status, "premium", "daily").is_none(),
        "{status}"
    );
    assert!(status_period(&status, "premium", "monthly").is_some());
    assert!(status_period(&status, "total", "daily").is_some());
    assert!(status_period(&status, "total", "monthly").is_some());
}

#[tokio::test]
async fn a_period_with_a_negative_limit_is_omitted() {
    let env = env_with_user_limits((1000, -5), (1000, 1000)).await;

    let status = quota_status_ok(&env, &TestUser::new()).await;

    assert!(
        status_period(&status, "total", "monthly").is_none(),
        "{status}"
    );
    assert!(status_period(&status, "total", "daily").is_some());
}

#[tokio::test]
async fn a_tier_without_any_positive_limit_reports_no_periods() {
    let env = env_with_user_limits((1000, 1000), (0, 0)).await;

    let status = quota_status_ok(&env, &TestUser::new()).await;

    for period in ["daily", "monthly"] {
        assert!(
            status_period(&status, "premium", period).is_none(),
            "{status}"
        );
    }
    assert!(status_period(&status, "total", "daily").is_some());
}

// ===========================================================================
// Scope: tenant and user
// ===========================================================================

#[tokio::test]
async fn the_status_shows_only_the_callers_own_usage() {
    let env = env_with_default_limits().await;
    let alice = TestUser::new();
    let bob = TestUser::in_tenant(alice.tenant_id);
    let carol = TestUser::new();
    let chat = new_chat(&env, &alice).await;
    complete_turn(&env, &alice, &chat, "hello", "answer").await;

    let used = |status: &Value| {
        expect_status_period(status, "total", "daily")["used_credits_micro"].clone()
    };

    assert_eq!(
        used(&quota_status_ok(&env, &alice).await),
        json!(default_usage_credits())
    );
    assert_eq!(
        used(&quota_status_ok(&env, &bob).await),
        json!(0),
        "a colleague of the same tenant does not see alice's usage"
    );
    assert_eq!(
        used(&quota_status_ok(&env, &carol).await),
        json!(0),
        "another tenant does not see alice's usage"
    );
}

#[tokio::test]
async fn the_same_user_id_in_another_tenant_has_separate_usage() {
    let env = env_with_default_limits().await;
    let alice = TestUser::new();
    let twin = TestUser {
        tenant_id: Uuid::new_v4(),
        user_id: alice.user_id,
    };
    seed_quota(&env, &alice, "daily", "total", Seed::spent(777)).await;

    let alice_status = quota_status_ok(&env, &alice).await;
    let twin_status = quota_status_ok(&env, &twin).await;

    assert_eq!(
        expect_status_period(&alice_status, "total", "daily")["used_credits_micro"],
        json!(777)
    );
    assert_eq!(
        expect_status_period(&twin_status, "total", "daily")["used_credits_micro"],
        json!(0)
    );
}

#[tokio::test]
async fn a_tenant_only_pdp_decision_still_scopes_the_status_to_the_caller() {
    let env = TestEnvBuilder::new()
        .policy_config(merge(
            policy(two_model_catalog()),
            json!({ "default_standard_limits": common::limits(1000, 1000) }),
        ))
        .authz(AuthzMode::TenantOnly)
        .build()
        .await;
    let alice = TestUser::new();
    let bob = TestUser::in_tenant(alice.tenant_id);
    seed_quota(&env, &alice, "daily", "total", Seed::spent(500)).await;

    let alice_status = quota_status_ok(&env, &alice).await;
    let bob_status = quota_status_ok(&env, &bob).await;

    assert_eq!(
        expect_status_period(&alice_status, "total", "daily")["used_credits_micro"],
        json!(500)
    );
    assert_eq!(
        expect_status_period(&bob_status, "total", "daily")["used_credits_micro"],
        json!(0),
        "the gear adds the owner predicate itself"
    );
}

// ===========================================================================
// Authorization (PEP resource UserQuota, action read, constraints required)
// ===========================================================================

#[tokio::test]
async fn a_denied_decision_is_403_permission_denied_on_the_user_quota_resource() {
    let env = env_with_authz(AuthzMode::Deny).await;

    let resp = quota_status(&env, &TestUser::new()).await;

    let problem = assert_problem(&resp, 403, Some("permission_denied"));
    assert_eq!(
        problem["context"]["resource_type"],
        json!(USER_QUOTA_RESOURCE_TYPE),
        "{problem}"
    );
    assert_eq!(problem["context"]["reason"], json!("AUTHZ_DENIED"));
}

#[tokio::test]
async fn an_unavailable_pdp_is_503_with_retry_after_5() {
    let env = env_with_authz(AuthzMode::Unavailable).await;

    let resp = quota_status(&env, &TestUser::new()).await;

    let problem = assert_problem(&resp, 503, Some("service_unavailable"));
    assert_eq!(problem["context"]["retry_after_seconds"], json!(5));
    assert_eq!(
        resp.headers
            .get(http::header::RETRY_AFTER)
            .and_then(|value| value.to_str().ok()),
        Some("5")
    );
}

#[tokio::test]
async fn a_decision_without_constraints_is_403_because_the_route_requires_them() {
    let env = env_with_authz(AuthzMode::NoConstraints).await;

    let resp = quota_status(&env, &TestUser::new()).await;

    assert_problem(&resp, 403, Some("permission_denied"));
}

#[tokio::test]
async fn a_denied_status_request_reveals_no_usage() {
    let env = env_with_authz(AuthzMode::Deny).await;
    let user = TestUser::new();
    seed_quota(&env, &user, "daily", "total", Seed::spent(10)).await;

    let resp = quota_status(&env, &user).await;

    assert_eq!(resp.status, StatusCode::FORBIDDEN);
    assert!(
        !common::body_text(&resp).contains("used_credits_micro"),
        "{}",
        common::body_text(&resp)
    );
}

// ===========================================================================
// Routing
// ===========================================================================

#[tokio::test]
async fn the_route_is_mounted_at_the_configured_url_prefix() {
    let env = TestEnvBuilder::new()
        .gear_config(json!({ "url_prefix": "/chat-api" }))
        .build()
        .await;
    let user = TestUser::new();

    let at_prefix = get(&env, &user, "/chat-api/v1/quota/status").await;
    let at_default = get(&env, &user, QUOTA_STATUS_PATH).await;

    expect_status(&at_prefix, StatusCode::OK);
    assert_eq!(at_default.status, StatusCode::NOT_FOUND);
}
