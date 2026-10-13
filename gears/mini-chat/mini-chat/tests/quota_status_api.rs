//! `GET /v1/quota/status` and the `quota_warnings` of the SSE `done` event.
//!
//! Source of truth: DESIGN 3.2 "Quota Warning Thresholds (P1)", "Quota Status Endpoint (P1)",
//! 3.3 `event: done` (`quota_warnings`), 3.8 (USER_QUOTA resource, action `read`,
//! `require_constraints`, tenant + owner predicates), 5.4.2 (bucket rows), the OpenAPI schemas
//! `QuotaStatusResponse`, `QuotaTierStatus`, `QuotaPeriodStatus`, `QuotaWarning`, `QuotaTier`,
//! `QuotaPeriod` and the operation `mini_chat.get_quota_status` (tag "Mini Chat Quotas").

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
    clippy::integer_division
)]

mod chat_support;
mod common;
mod quota_support;
mod stream_support;

use std::collections::BTreeSet;

use mini_chat::domain::authz::USER_QUOTA_RESOURCE_TYPE;
use quota_support::{
    FakePdp, LimitsSpec, Opts, PREMIUM_IN, PREMIUM_OUT, ProviderReply, QuotaHarness, QuotaOptions,
    STANDARD, STANDARD_IN, STANDARD_OUT, TENANT_B, USER_B, UsageSeed, answer_with_usage,
    assert_problem, ctx_a, parse_ts, period_of, ref_credits, status_period, stream_model_json,
    tier_of, warning_of,
};
use serde_json::{Value, json};
use stream_support::{second_user, tenant_b_user};
use time::OffsetDateTime;
use uuid::Uuid;

const MSG: &str = "hello";

fn keys(value: &Value) -> BTreeSet<String> {
    value.as_object().expect("object").keys().cloned().collect()
}

fn key_set(names: &[&str]) -> BTreeSet<String> {
    names.iter().map(|s| (*s).to_owned()).collect()
}

/// 100_000 / 1_000_000 for the shared cap, 50_000 / 500_000 for the premium sub-cap (the premium
/// reserve of a short message is 61_779, so no premium turn fits these limits).
fn small_limits() -> LimitsSpec {
    LimitsSpec::new(100_000, 1_000_000, 50_000, 500_000)
}

/// Room for premium turns.
fn roomy() -> LimitsSpec {
    LimitsSpec::new(1_000_000, 10_000_000, 500_000, 5_000_000)
}

// ═════════════════════════════════════════════════════════════════════════════
// the endpoint contract
// ═════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn the_status_operation_is_registered_with_the_openapi_identity() {
    let h = QuotaHarness::new().await;
    let spec = h
        .chat
        .openapi
        .operation_specs
        .get("GET:/mini-chat/v1/quota/status")
        .expect("GET /mini-chat/v1/quota/status is registered");
    assert_eq!(
        spec.operation_id.as_deref(),
        Some("mini_chat.get_quota_status")
    );
    assert!(spec.authenticated);
    assert!(
        spec.tags.iter().any(|t| t == "Mini Chat Quotas"),
        "{:?}",
        spec.tags
    );
    let license = spec.license_requirement.as_ref().expect("license feature");
    assert_eq!(
        license.license_names,
        vec!["gts.cf.core.lic.feat.v1~cf.core.global.base.v1".to_owned()]
    );
    let statuses: Vec<u16> = spec.responses.iter().map(|r| r.status).collect();
    for status in [200, 401, 403, 500, 503] {
        assert!(
            statuses.contains(&status),
            "declares {status}: {statuses:?}"
        );
    }
}

#[tokio::test]
async fn a_fresh_user_sees_every_limit_untouched() {
    let h = QuotaHarness::with_limits(small_limits()).await;
    let resp = h.status_as(&ctx_a()).await;
    assert_eq!(resp.status.as_u16(), 200, "{}", resp.raw);
    assert!(
        resp.content_type().starts_with("application/json"),
        "{}",
        resp.content_type()
    );
    let body = resp.json();
    assert_eq!(keys(&body), key_set(&["tiers", "warning_threshold_pct"]));
    assert_eq!(
        body["warning_threshold_pct"],
        json!(80),
        "the config default"
    );

    for (tier, daily, monthly) in [("total", 100_000, 1_000_000), ("premium", 50_000, 500_000)] {
        let entry = tier_of(&body, tier).unwrap_or_else(|| panic!("tier {tier}: {body}"));
        assert_eq!(keys(&entry), key_set(&["tier", "periods"]));
        for (period, limit) in [("daily", daily), ("monthly", monthly)] {
            let p = period_of(&entry, period).unwrap_or_else(|| panic!("{tier}/{period}"));
            assert_eq!(
                keys(&p),
                key_set(&[
                    "period",
                    "limit_credits_micro",
                    "used_credits_micro",
                    "remaining_credits_micro",
                    "remaining_percentage",
                    "next_reset",
                    "warning",
                    "exhausted"
                ])
            );
            assert_eq!(p["limit_credits_micro"], json!(limit), "{tier}/{period}");
            assert_eq!(p["used_credits_micro"], json!(0));
            assert_eq!(p["remaining_credits_micro"], json!(limit));
            assert_eq!(p["remaining_percentage"], json!(100));
            assert_eq!(p["warning"], json!(false));
            assert_eq!(p["exhausted"], json!(false));
        }
    }
    let tiers: BTreeSet<String> = body["tiers"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["tier"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(tiers, key_set(&["premium", "total"]));
}

#[tokio::test]
async fn next_reset_is_the_next_utc_midnight_and_the_first_of_next_month() {
    let h = QuotaHarness::with_limits(small_limits()).await;
    let before = OffsetDateTime::now_utc();
    let body = h.status().await;
    let after = OffsetDateTime::now_utc();

    for tier in ["total", "premium"] {
        let daily = parse_ts(&status_period(&body, tier, "daily")["next_reset"]);
        let monthly = parse_ts(&status_period(&body, tier, "monthly")["next_reset"]);
        for reset in [daily, monthly] {
            assert_eq!(reset.offset(), time::UtcOffset::UTC, "UTC");
            assert_eq!(
                (
                    reset.hour(),
                    reset.minute(),
                    reset.second(),
                    reset.nanosecond()
                ),
                (0, 0, 0, 0),
                "midnight"
            );
        }
        let expected_daily =
            |now: OffsetDateTime| now.replace_time(time::Time::MIDNIGHT) + time::Duration::days(1);
        assert!(
            daily == expected_daily(before) || daily == expected_daily(after),
            "daily reset {daily}"
        );
        assert_eq!(monthly.day(), 1, "monthly reset is the 1st: {monthly}");
        assert!(monthly > after && monthly <= after + time::Duration::days(32));
    }
}

#[tokio::test]
async fn the_warning_threshold_is_the_configured_value() {
    let h = QuotaHarness::with(QuotaOptions {
        config: json!({"quota": {"warning_threshold_pct": 65}}),
        limits: small_limits(),
        ..Default::default()
    })
    .await;
    assert_eq!(h.status().await["warning_threshold_pct"], json!(65));
}

#[tokio::test]
async fn used_is_spent_plus_reserved_and_remaining_is_what_is_left() {
    let h = QuotaHarness::with_limits(small_limits()).await;
    h.seed_usage(UsageSeed::total("daily").spent(10_000).reserved(5_000))
        .await;
    h.seed_usage(UsageSeed::premium("monthly").spent(125_000))
        .await;
    let body = h.status().await;

    let p = status_period(&body, "total", "daily");
    assert_eq!(p["used_credits_micro"], json!(15_000));
    assert_eq!(p["remaining_credits_micro"], json!(85_000));
    assert_eq!(p["remaining_percentage"], json!(85));
    let p = status_period(&body, "premium", "monthly");
    assert_eq!(p["used_credits_micro"], json!(125_000));
    assert_eq!(p["remaining_credits_micro"], json!(375_000));
    assert_eq!(p["remaining_percentage"], json!(75));
    // a bucket without a row is untouched
    let p = status_period(&body, "total", "monthly");
    assert_eq!(p["used_credits_micro"], json!(0));
}

#[tokio::test]
async fn the_status_is_consistent_with_the_turns_that_ran() {
    let h = QuotaHarness::with_limits(roomy()).await;
    let premium_chat = h.seed_chat().await;
    let standard_chat = h.seed_chat_on(STANDARD).await;
    h.oagw.push(answer_with_usage(800, 200)); // premium turn
    h.oagw.push(answer_with_usage(100, 50)); // standard turn
    h.say(premium_chat, MSG).await;
    h.say(standard_chat, MSG).await;

    let premium_credits = ref_credits(800, 200, PREMIUM_IN, PREMIUM_OUT);
    let standard_credits = ref_credits(100, 50, STANDARD_IN, STANDARD_OUT);
    let body = h.status().await;
    for period in ["daily", "monthly"] {
        let total = status_period(&body, "total", period);
        assert_eq!(
            total["used_credits_micro"],
            json!(premium_credits + standard_credits),
            "the shared cap counts every tier ({period})"
        );
        let premium = status_period(&body, "premium", period);
        assert_eq!(
            premium["used_credits_micro"],
            json!(premium_credits),
            "the premium sub-cap counts only premium turns ({period})"
        );
        let limit = total["limit_credits_micro"].as_i64().unwrap();
        let used = total["used_credits_micro"].as_i64().unwrap();
        assert_eq!(total["remaining_credits_micro"], json!(limit - used));
        assert_eq!(
            total["remaining_percentage"],
            json!((limit - used) * 100 / limit)
        );
    }
}

#[tokio::test]
async fn an_in_flight_reserve_counts_as_used_until_the_turn_settles() {
    let h = QuotaHarness::with_limits(LimitsSpec::new(
        1_000_000, 10_000_000, 1_000_000, 10_000_000,
    ))
    .await;
    let chat = h.seed_chat().await;
    h.oagw.push(ProviderReply::text_with(
        &["a", "b"],
        &Opts::default().usage(800, 200).pause_after(1),
    ));
    let mut live = h
        .open(chat, &ctx_a(), &json!({"content": MSG}))
        .await
        .live();
    live.expect("stream_started").await;
    h.oagw.wait_paused(1).await;

    let reserve = 61_779;
    let during = h.status().await;
    assert_eq!(
        status_period(&during, "total", "daily")["used_credits_micro"],
        json!(reserve)
    );
    assert_eq!(
        status_period(&during, "premium", "monthly")["used_credits_micro"],
        json!(reserve)
    );

    h.oagw.release();
    live.drain().await;
    let after = h.status().await;
    assert_eq!(
        status_period(&after, "total", "daily")["used_credits_micro"],
        json!(ref_credits(800, 200, PREMIUM_IN, PREMIUM_OUT)),
        "the unused part of the reserve is returned"
    );
}

#[tokio::test]
async fn the_warning_and_exhausted_flags_follow_the_threshold() {
    let h = QuotaHarness::with_limits(small_limits()).await;
    // total daily: 85 % used -> remaining 15 % <= 100 - 80 -> warning; premium daily: 100 % used
    h.seed_usage(UsageSeed::total("daily").spent(85_000)).await;
    h.seed_usage(UsageSeed::premium("daily").spent(50_000))
        .await;
    h.seed_usage(UsageSeed::total("monthly").spent(790_000))
        .await; // exactly 21 % left
    let body = h.status().await;

    let p = status_period(&body, "total", "daily");
    assert_eq!(p["remaining_percentage"], json!(15));
    assert_eq!(
        (p["warning"].clone(), p["exhausted"].clone()),
        (json!(true), json!(false))
    );
    let p = status_period(&body, "premium", "daily");
    assert_eq!(p["remaining_percentage"], json!(0));
    assert_eq!(
        (p["warning"].clone(), p["exhausted"].clone()),
        (json!(true), json!(true))
    );
    let p = status_period(&body, "total", "monthly");
    assert_eq!(p["remaining_percentage"], json!(21));
    assert_eq!(p["warning"], json!(false), "21 > 100 - 80");
}

#[tokio::test]
async fn periods_with_a_non_positive_limit_are_not_listed() {
    let h = QuotaHarness::with_limits(LimitsSpec::new(100_000, 0, 0, 500_000)).await;
    let body = h.status().await;
    let periods_of = |tier: &str| -> Vec<String> {
        tier_of(&body, tier)
            .map(|t| {
                t["periods"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|p| p["period"].as_str().unwrap().to_owned())
                    .collect()
            })
            .unwrap_or_default()
    };
    assert_eq!(periods_of("total"), vec!["daily".to_owned()]);
    assert_eq!(periods_of("premium"), vec!["monthly".to_owned()]);
}

#[tokio::test]
async fn only_the_current_period_rows_are_read() {
    let h = QuotaHarness::with_limits(small_limits()).await;
    h.seed_usage(UsageSeed::total("daily").spent(99_000).stale())
        .await;
    h.seed_usage(UsageSeed::total("monthly").spent(999_000).stale())
        .await;
    let body = h.status().await;
    assert_eq!(
        status_period(&body, "total", "daily")["used_credits_micro"],
        json!(0)
    );
    assert_eq!(
        status_period(&body, "total", "monthly")["used_credits_micro"],
        json!(0)
    );

    // a turn today also ignores them
    let chat = h.seed_chat_on(STANDARD).await;
    let sent = h.say(chat, MSG).await;
    assert_eq!(sent.names_no_ping().last().unwrap(), "done");
}

#[tokio::test]
async fn the_status_is_scoped_to_the_caller() {
    let h = QuotaHarness::with_limits(small_limits()).await;
    h.seed_usage(UsageSeed::total("daily").spent(40_000)).await;
    h.seed_usage(
        UsageSeed::total("daily")
            .spent(90_000)
            .user(chat_support::USER_A2, quota_support::TENANT_A),
    )
    .await;
    h.seed_usage(
        UsageSeed::total("daily")
            .spent(70_000)
            .user(USER_B, TENANT_B),
    )
    .await;

    let a = h.status().await;
    assert_eq!(
        status_period(&a, "total", "daily")["used_credits_micro"],
        json!(40_000)
    );
    let a2 = h.status_as(&second_user()).await;
    assert_eq!(a2.status.as_u16(), 200);
    assert_eq!(
        status_period(&a2.json(), "total", "daily")["used_credits_micro"],
        json!(90_000)
    );
    let b = h.status_as(&tenant_b_user()).await;
    assert_eq!(
        status_period(&b.json(), "total", "daily")["used_credits_micro"],
        json!(70_000)
    );
}

#[tokio::test]
async fn limits_are_read_under_the_current_policy_version() {
    let h = QuotaHarness::with(QuotaOptions {
        limits: small_limits(),
        policy_version: 7,
        ..Default::default()
    })
    .await;
    h.status().await;
    let asked = h.policy.limit_requests.lock().unwrap().clone();
    assert!(!asked.is_empty(), "the limits come from the policy plugin");
    assert!(
        asked
            .iter()
            .all(|(user, version)| *user == quota_support::USER_A && *version == 7),
        "{asked:?}"
    );
}

#[tokio::test]
async fn reading_the_status_changes_nothing() {
    let h = QuotaHarness::with_limits(small_limits()).await;
    h.seed_usage(UsageSeed::total("daily").spent(1_000)).await;
    let before = h.row_counts().await;
    let fingerprint = h.usage_fingerprint().await;
    for _ in 0..3 {
        h.status().await;
    }
    assert_eq!(h.row_counts().await, before);
    assert_eq!(h.usage_fingerprint().await, fingerprint);
    h.oagw.assert_no_provider_call();
    h.policy.assert_published_stays(0).await;
}

// ── authorization (DESIGN 3.8) ───────────────────────────────────────────────

#[tokio::test]
async fn the_status_is_authorized_as_read_on_the_user_quota_resource() {
    let h = QuotaHarness::new().await;
    h.status().await;
    let request = h.pdp.last();
    assert_eq!(request.resource.resource_type, USER_QUOTA_RESOURCE_TYPE);
    assert_eq!(request.action.name, "read");
    assert!(request.context.require_constraints);
    assert!(request.resource.id.is_none());
    let supported: BTreeSet<String> = request
        .context
        .supported_properties
        .iter()
        .cloned()
        .collect();
    assert_eq!(supported, key_set(&["owner_tenant_id", "owner_id"]));
}

#[tokio::test]
async fn a_denied_or_unreachable_pdp_fails_closed() {
    let denied = QuotaHarness::with(QuotaOptions {
        pdp: FakePdp::deny("no grant"),
        ..Default::default()
    })
    .await;
    let resp = denied.status_as(&ctx_a()).await;
    assert_problem(&resp, 403, "permission_denied");
    assert!(
        !resp.raw.contains("no grant"),
        "the PDP reason is not exposed"
    );

    let down = QuotaHarness::with(QuotaOptions {
        pdp: FakePdp::unreachable("pdp down"),
        ..Default::default()
    })
    .await;
    let resp = down.status_as(&ctx_a()).await;
    assert_problem(&resp, 503, "service_unavailable");
    assert!(!resp.raw.contains("pdp down"));
}

// ═════════════════════════════════════════════════════════════════════════════
// done.quota_warnings
// ═════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn a_completed_turn_reports_a_warning_entry_per_tier_and_period() {
    let h = QuotaHarness::with_limits(small_limits()).await;
    let chat = h.seed_chat_on(STANDARD).await;
    h.oagw.push(answer_with_usage(1_000, 200));
    let sent = h.say(chat, MSG).await;
    let done = sent.done();

    let warnings = done["quota_warnings"]
        .as_array()
        .expect("quota_warnings on a completed turn");
    for tier in ["total", "premium"] {
        for period in ["daily", "monthly"] {
            let w = warning_of(done, tier, period)
                .unwrap_or_else(|| panic!("no {tier}/{period} in {warnings:?}"));
            assert_eq!(
                keys(&w)
                    .difference(&key_set(&["next_reset"]))
                    .cloned()
                    .collect::<BTreeSet<_>>(),
                key_set(&[
                    "tier",
                    "period",
                    "remaining_percentage",
                    "warning",
                    "exhausted"
                ]),
                "{w}"
            );
        }
    }
    // the figures are taken after this turn's settlement: 1_000 + 600 credits of 100_000
    let w = warning_of(done, "total", "daily").unwrap();
    assert_eq!(w["remaining_percentage"], json!(98));
    assert_eq!(w["warning"], json!(false));
    assert_eq!(w["exhausted"], json!(false));
    assert!(
        w.get("next_reset").is_none_or(Value::is_null),
        "no next_reset without a warning"
    );
    let w = warning_of(done, "premium", "daily").unwrap();
    assert_eq!(
        w["remaining_percentage"],
        json!(100),
        "a standard turn left the sub-cap alone"
    );
}

#[tokio::test]
async fn warnings_carry_next_reset_once_the_threshold_is_crossed() {
    let h = QuotaHarness::with_limits(small_limits()).await;
    // 82_000 + this turn (1_600) = 83_600 of 100_000 -> 16 % left -> warning
    h.seed_usage(UsageSeed::total("daily").spent(82_000)).await;
    let chat = h.seed_chat_on(STANDARD).await;
    h.oagw.push(answer_with_usage(1_000, 200));
    let sent = h.say(chat, MSG).await;
    let done = sent.done();

    let w = warning_of(done, "total", "daily").unwrap();
    assert_eq!(w["remaining_percentage"], json!(16));
    assert_eq!(w["warning"], json!(true));
    assert_eq!(w["exhausted"], json!(false));
    let status = h.status().await;
    assert_eq!(
        w["next_reset"],
        status_period(&status, "total", "daily")["next_reset"],
        "the same reset instant as the status endpoint"
    );
    // the monthly period is still fine
    let m = warning_of(done, "total", "monthly").unwrap();
    assert_eq!(m["warning"], json!(false));
    assert!(m.get("next_reset").is_none_or(Value::is_null));
}

#[tokio::test]
async fn less_than_one_percent_left_after_a_turn_is_reported_exhausted() {
    // a model with a 100 token output cap reserves 413 credits; the actual usage is capped at the
    // reserve, leaving 87 of 10_000 (0.87 % -> 0 %)
    let mut standard = stream_model_json(STANDARD, "Standard");
    standard["max_output_tokens"] = json!(100);
    standard["general_config"]["tool_support"]["web_search"] = json!(false);
    let h = QuotaHarness::with(QuotaOptions {
        models: vec![standard],
        limits: LimitsSpec::new(10_000, 1_000_000, 10_000, 1_000_000),
        ..Default::default()
    })
    .await;
    h.seed_usage(UsageSeed::total("daily").spent(9_500)).await;
    let chat = h.seed_chat_on(STANDARD).await;
    h.oagw.push(answer_with_usage(400, 100));
    let sent = h.say(chat, MSG).await;
    assert_eq!(
        sent.names_no_ping().last().unwrap(),
        "done",
        "{:?}",
        sent.events
    );

    let w = warning_of(sent.done(), "total", "daily").unwrap();
    assert_eq!(w["remaining_percentage"], json!(0));
    assert_eq!(w["exhausted"], json!(true));
    assert_eq!(w["warning"], json!(true));
    assert!(w["next_reset"].is_string());
    let status = h.status().await;
    let p = status_period(&status, "total", "daily");
    assert_eq!(p["exhausted"], json!(true));
    assert_eq!(p["remaining_credits_micro"], json!(87));
}

#[tokio::test]
async fn the_threshold_of_the_warnings_is_the_configured_one() {
    let h = QuotaHarness::with(QuotaOptions {
        config: json!({"quota": {"warning_threshold_pct": 50}}),
        limits: small_limits(),
        ..Default::default()
    })
    .await;
    h.seed_usage(UsageSeed::total("daily").spent(55_000)).await;
    let chat = h.seed_chat_on(STANDARD).await;
    h.oagw.push(answer_with_usage(1_000, 200));
    let sent = h.say(chat, MSG).await;
    let w = warning_of(sent.done(), "total", "daily").unwrap();
    assert_eq!(w["remaining_percentage"], json!(43));
    assert_eq!(w["warning"], json!(true), "43 <= 100 - 50");
}

#[tokio::test]
async fn there_are_no_quota_warnings_on_errors_replays_or_cancellations() {
    let h = QuotaHarness::with_limits(roomy()).await;
    let chat = h.seed_chat().await;

    // error: no done event at all
    h.oagw.push(ProviderReply::http_error(500, "boom"));
    let failed = h.say(chat, MSG).await;
    assert!(failed.of("done").is_empty());
    assert!(!failed.blob().contains("quota_warnings"));

    // completed, then replayed
    let rid = Uuid::new_v4();
    let first = h.say_with(chat, "again", rid).await;
    assert!(first.done().get("quota_warnings").is_some());
    let replay = h.say_with(chat, "again", rid).await;
    assert_eq!(replay.started()["is_new_turn"], json!(false));
    assert!(
        replay
            .done()
            .get("quota_warnings")
            .is_none_or(Value::is_null),
        "absent on replay (DESIGN 3.3)"
    );

    // a cancelled turn has no terminal event to carry them
    let cancelled_rid = Uuid::new_v4();
    h.oagw.push(ProviderReply::text_with(
        &["x"],
        &Opts::default().pause_after(0),
    ));
    let mut live = h
        .open(
            chat,
            &ctx_a(),
            &json!({"content": "third", "request_id": cancelled_rid}),
        )
        .await
        .live();
    live.expect("stream_started").await;
    h.oagw.wait_paused(1).await;
    drop(live);
    h.wait_turn_state(cancelled_rid, "cancelled").await;
}

#[tokio::test]
async fn the_warnings_agree_with_the_status_endpoint() {
    let h = QuotaHarness::with_limits(roomy()).await;
    h.seed_usage(UsageSeed::total("daily").spent(900_000)).await;
    h.seed_usage(UsageSeed::premium("monthly").spent(4_000_000))
        .await;
    let chat = h.seed_chat().await;
    h.oagw.push(answer_with_usage(500, 100));
    let sent = h.say(chat, MSG).await;
    let done = sent.done();
    let status = h.status().await;

    for tier in ["total", "premium"] {
        for period in ["daily", "monthly"] {
            let w = warning_of(done, tier, period).unwrap();
            let p = status_period(&status, tier, period);
            for key in ["remaining_percentage", "warning", "exhausted"] {
                assert_eq!(w[key], p[key], "{tier}/{period}/{key}");
            }
        }
    }
}
