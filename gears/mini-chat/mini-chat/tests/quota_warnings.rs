#![allow(clippy::unwrap_used, clippy::expect_used)]

//! `done.quota_warnings` (DESIGN "Quota Warning Thresholds (P1)", section 3.7 `done` table): one entry
//! per tier and period with `{tier, period, remaining_percentage, warning, exhausted, next_reset?}`,
//! computed after the settlement, `next_reset` only when `warning` or `exhausted`, absent on error and
//! replay.
//!
//! A turn on the default catalog uses a `10 / 5` token answer, which is `25` micro-credits at the
//! `catalog_entry` multipliers; the limits and the seeded usage below are chosen so the reserve of a
//! `hello` turn (`12401` micro-credits) still fits before the turn.

mod common;

use common::{
    STANDARD_MODEL, Seed, chat_on, default_usage_credits, enqueue_chat, env_with_user_limits,
    expect_status_period, expect_stream, failed_script, named, new_chat, parse_ts, quota_status_ok,
    seed_quota, send_text, send_with_id, terminal,
};
use mini_chat::test_support::{TestEnv, TestUser};
use serde_json::{Value, json};
use time::{Duration, OffsetDateTime, Time};
use uuid::Uuid;

const BIG: i64 = 1_000_000_000;

fn warnings_of(events: &[mini_chat::test_support::SseEvent]) -> Vec<Value> {
    terminal(events).data["quota_warnings"]
        .as_array()
        .unwrap_or_else(|| panic!("quota_warnings must be an array: {events:?}"))
        .clone()
}

fn entry<'a>(warnings: &'a [Value], tier: &str, period: &str) -> &'a Value {
    warnings
        .iter()
        .find(|warning| warning["tier"] == json!(tier) && warning["period"] == json!(period))
        .unwrap_or_else(|| panic!("no `{tier}`/`{period}` entry in {warnings:?}"))
}

fn tomorrow_midnight() -> [OffsetDateTime; 2] {
    let at = |now: OffsetDateTime| {
        (now.date() + Duration::days(1))
            .with_time(Time::MIDNIGHT)
            .assume_utc()
    };
    // The test may straddle midnight: accept the reset computed before or after the request.
    [
        at(OffsetDateTime::now_utc() - Duration::seconds(5)),
        at(OffsetDateTime::now_utc()),
    ]
}

async fn turn_with(env: &TestEnv, user: &TestUser, seeds: &[(&str, &str, i64)]) -> Vec<Value> {
    for (period, bucket, spent) in seeds {
        seed_quota(env, user, period, bucket, Seed::spent(*spent)).await;
    }
    let chat = new_chat(env, user).await;
    let events = expect_stream(&send_text(env, user, &chat, "hello").await);
    assert_eq!(terminal(&events).event, "done", "{events:?}");
    warnings_of(&events)
}

// ===========================================================================
// Shape
// ===========================================================================

#[tokio::test]
async fn a_completed_turn_reports_one_entry_per_tier_and_period() {
    let env = env_with_user_limits((BIG, BIG), (BIG, BIG)).await;
    let user = TestUser::new();

    let warnings = turn_with(&env, &user, &[]).await;

    assert_eq!(warnings.len(), 4, "{warnings:?}");
    for tier in ["premium", "total"] {
        for period in ["daily", "monthly"] {
            entry(&warnings, tier, period);
        }
    }
}

#[tokio::test]
async fn an_entry_without_warning_or_exhaustion_has_no_next_reset() {
    let env = env_with_user_limits((BIG, BIG), (BIG, BIG)).await;
    let user = TestUser::new();

    let warnings = turn_with(&env, &user, &[]).await;

    for warning in &warnings {
        let mut keys: Vec<&str> = warning
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            [
                "exhausted",
                "period",
                "remaining_percentage",
                "tier",
                "warning"
            ],
            "{warning}"
        );
        assert_eq!(warning["warning"], json!(false), "{warning}");
        assert_eq!(warning["exhausted"], json!(false), "{warning}");
        assert!(warning["remaining_percentage"].is_i64(), "{warning}");
    }
}

#[tokio::test]
async fn the_percentage_is_floored_after_the_settlement() {
    // limit 100_000_000: (100_000_000 - 25) * 100 / 100_000_000 = 99.999975 -> 99.
    let env = env_with_user_limits((100_000_000, BIG), (50_000_000, BIG)).await;
    let user = TestUser::new();

    let warnings = turn_with(&env, &user, &[]).await;

    assert_eq!(
        entry(&warnings, "total", "daily")["remaining_percentage"],
        json!(99)
    );
    assert_eq!(
        entry(&warnings, "premium", "daily")["remaining_percentage"],
        json!(99)
    );
    assert_eq!(
        entry(&warnings, "total", "monthly")["remaining_percentage"],
        json!(99),
        "floored, not rounded"
    );
}

// ===========================================================================
// warning: remaining_percentage <= 100 - warning_threshold_pct (80 -> 20)
// ===========================================================================

#[tokio::test]
async fn a_period_at_the_threshold_warns_and_carries_next_reset() {
    // limit 100_000, seeded 79_975 + settled 25 = 80_000 -> remaining 20_000 = 20%.
    let env = env_with_user_limits((100_000, BIG), (BIG, BIG)).await;
    let user = TestUser::new();

    let warnings = turn_with(&env, &user, &[("daily", "total", 79_975)]).await;

    let daily = entry(&warnings, "total", "daily");
    assert_eq!(daily["remaining_percentage"], json!(20));
    assert_eq!(daily["warning"], json!(true));
    assert_eq!(daily["exhausted"], json!(false));
    let reset = parse_ts(&daily["next_reset"]);
    assert!(
        tomorrow_midnight().contains(&reset),
        "a daily next_reset is midnight UTC tomorrow: {reset}"
    );
    let monthly = entry(&warnings, "total", "monthly");
    assert_eq!(monthly["warning"], json!(false));
    assert!(monthly.get("next_reset").is_none(), "{monthly}");
}

#[tokio::test]
async fn a_period_just_above_the_threshold_does_not_warn() {
    // seeded 78_975 + settled 25 = 79_000 -> remaining 21_000 = 21%.
    let env = env_with_user_limits((100_000, BIG), (BIG, BIG)).await;
    let user = TestUser::new();

    let warnings = turn_with(&env, &user, &[("daily", "total", 78_975)]).await;

    let daily = entry(&warnings, "total", "daily");
    assert_eq!(daily["remaining_percentage"], json!(21));
    assert_eq!(daily["warning"], json!(false));
    assert!(daily.get("next_reset").is_none(), "{daily}");
}

#[tokio::test]
async fn a_remaining_share_below_one_percent_is_exhausted_and_warns() {
    // limit 10_000_000, seeded 9_987_000 (+12_401 reserve fits) + settled 25: remaining 12_975 = 0.13%.
    let env = env_with_user_limits((10_000_000, BIG), (BIG, BIG)).await;
    let user = TestUser::new();

    let warnings = turn_with(&env, &user, &[("daily", "total", 9_987_000)]).await;

    let daily = entry(&warnings, "total", "daily");
    assert_eq!(daily["remaining_percentage"], json!(0));
    assert_eq!(daily["exhausted"], json!(true));
    assert_eq!(daily["warning"], json!(true));
    assert!(daily["next_reset"].is_string(), "{daily}");
}

#[tokio::test]
async fn the_monthly_next_reset_is_the_first_of_next_month() {
    let env = env_with_user_limits((BIG, 100_000), (BIG, BIG)).await;
    let user = TestUser::new();

    let warnings = turn_with(&env, &user, &[("monthly", "total", 85_000)]).await;

    let monthly = entry(&warnings, "total", "monthly");
    assert_eq!(monthly["warning"], json!(true));
    let reset = parse_ts(&monthly["next_reset"]);
    assert_eq!(reset.day(), 1, "{reset}");
    assert_eq!(reset.time(), Time::MIDNIGHT);
    let today = OffsetDateTime::now_utc().date();
    assert!(
        reset.date() > today && reset.date() <= today + Duration::days(31),
        "{reset}"
    );
}

#[tokio::test]
async fn the_premium_subcap_warns_independently_of_the_total() {
    // The premium subcap still has room for the reserve (85_000 + 12_401 <= 100_000), so the turn
    // stays premium.
    let env = env_with_user_limits((BIG, BIG), (100_000, BIG)).await;
    let user = TestUser::new();

    let warnings = turn_with(&env, &user, &[("daily", "tier:premium", 85_000)]).await;

    let premium = entry(&warnings, "premium", "daily");
    assert_eq!(premium["remaining_percentage"], json!(14));
    assert_eq!(premium["warning"], json!(true));
    assert_eq!(premium["exhausted"], json!(false));
    assert_eq!(entry(&warnings, "total", "daily")["warning"], json!(false));
}

#[tokio::test]
async fn the_done_of_a_downgraded_turn_still_carries_the_warnings() {
    // Premium is exhausted for the reserve, so the turn runs on the standard model.
    let env = env_with_user_limits((BIG, BIG), (1000, 1000)).await;
    let user = TestUser::new();
    let chat = new_chat(&env, &user).await;

    let events = expect_stream(&send_text(&env, &user, &chat, "hello").await);

    assert_eq!(terminal(&events).data["quota_decision"], json!("downgrade"));
    let warnings = warnings_of(&events);
    assert_eq!(warnings.len(), 4, "{warnings:?}");
    let premium = entry(&warnings, "premium", "daily");
    assert_eq!(premium["exhausted"], json!(false));
    assert_eq!(
        premium["remaining_percentage"],
        json!(100),
        "premium untouched"
    );
}

#[tokio::test]
async fn a_standard_turn_leaves_the_premium_entries_at_their_state() {
    let env = env_with_user_limits((BIG, BIG), (100_000, BIG)).await;
    let user = TestUser::new();
    seed_quota(&env, &user, "daily", "tier:premium", Seed::spent(90_000)).await;
    let chat = chat_on(&env, &user, STANDARD_MODEL).await;

    let events = expect_stream(&send_text(&env, &user, &chat, "hello").await);

    let warnings = warnings_of(&events);
    let premium = entry(&warnings, "premium", "daily");
    assert_eq!(premium["remaining_percentage"], json!(10));
    assert_eq!(premium["warning"], json!(true));
}

// ===========================================================================
// Limits <= 0 are skipped
// ===========================================================================

#[tokio::test]
async fn periods_with_a_non_positive_limit_have_no_entry() {
    let env = env_with_user_limits((BIG, BIG), (0, BIG)).await;
    let user = TestUser::new();
    let chat = chat_on(&env, &user, STANDARD_MODEL).await;

    let events = expect_stream(&send_text(&env, &user, &chat, "hello").await);

    let warnings = warnings_of(&events);
    assert!(
        warnings
            .iter()
            .all(|warning| !(warning["tier"] == json!("premium")
                && warning["period"] == json!("daily"))),
        "{warnings:?}"
    );
    entry(&warnings, "premium", "monthly");
    entry(&warnings, "total", "daily");
    entry(&warnings, "total", "monthly");
    assert_eq!(warnings.len(), 3);
}

#[tokio::test]
async fn a_tier_with_no_positive_limit_has_no_entries_at_all() {
    let env = env_with_user_limits((BIG, BIG), (0, 0)).await;
    let user = TestUser::new();
    let chat = chat_on(&env, &user, STANDARD_MODEL).await;

    let events = expect_stream(&send_text(&env, &user, &chat, "hello").await);

    let warnings = warnings_of(&events);
    assert_eq!(warnings.len(), 2, "{warnings:?}");
    assert!(
        warnings
            .iter()
            .all(|warning| warning["tier"] == json!("total"))
    );
}

// ===========================================================================
// Consistency with the status endpoint, absence on error and replay
// ===========================================================================

#[tokio::test]
async fn the_warnings_agree_with_the_quota_status_after_the_turn() {
    let env = env_with_user_limits((100_000, 10_000_000), (50_000, 5_000_000)).await;
    let user = TestUser::new();

    let warnings = turn_with(&env, &user, &[("daily", "total", 79_975)]).await;
    let status = quota_status_ok(&env, &user).await;

    for warning in &warnings {
        let period = expect_status_period(
            &status,
            warning["tier"].as_str().unwrap(),
            warning["period"].as_str().unwrap(),
        );
        for member in ["remaining_percentage", "warning", "exhausted"] {
            assert_eq!(
                warning[member], period[member],
                "{member}: {warning} vs {period}"
            );
        }
        if warning["warning"] == json!(true) || warning["exhausted"] == json!(true) {
            assert_eq!(warning["next_reset"], period["next_reset"], "{warning}");
        }
    }
    assert_eq!(
        expect_status_period(&status, "total", "daily")["used_credits_micro"],
        json!(79_975 + default_usage_credits())
    );
}

#[tokio::test]
async fn a_failed_turn_has_no_warnings() {
    let env = env_with_user_limits((BIG, BIG), (BIG, BIG)).await;
    let user = TestUser::new();
    let chat = new_chat(&env, &user).await;
    enqueue_chat(&env, failed_script("boom", None));

    let events = expect_stream(&send_text(&env, &user, &chat, "hello").await);

    let error = terminal(&events);
    assert_eq!(error.event, "error");
    assert!(error.data.get("quota_warnings").is_none(), "{error:?}");
    assert!(named(&events, "done").is_empty());
}

#[tokio::test]
async fn a_replayed_turn_has_no_warnings() {
    let env = env_with_user_limits((BIG, BIG), (BIG, BIG)).await;
    let user = TestUser::new();
    let chat = new_chat(&env, &user).await;
    let request_id = Uuid::new_v4();
    let original = expect_stream(&send_with_id(&env, &user, &chat, "hello", request_id).await);
    assert!(terminal(&original).data.get("quota_warnings").is_some());

    let replay = expect_stream(&send_with_id(&env, &user, &chat, "hello", request_id).await);

    assert_eq!(terminal(&replay).event, "done");
    assert!(
        terminal(&replay).data.get("quota_warnings").is_none(),
        "{replay:?}"
    );
}
