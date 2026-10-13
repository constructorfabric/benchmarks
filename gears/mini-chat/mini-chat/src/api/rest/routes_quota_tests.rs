#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Slice 6: `GET /v1/quota/status` through the REST layer
//! (`cpt-cf-mini-chat-flow-quota-billing-get-quota-status`,
//! `cpt-cf-mini-chat-dod-quota-billing-quota-status`): shape, arithmetic,
//! resets, omissions, owner and tenant isolation, errors, registration and the
//! `quota_warnings` DTO.

use std::collections::BTreeSet;

use axum::http::StatusCode;
use mini_chat_sdk::UserLimits;
use serde_json::{Value, json};

use super::BASE_LICENSE_FEATURE;
use crate::api::rest::dto::QuotaWarning;
use crate::chat_test_env::Reply;
use crate::config::QuotaConfig;
use crate::domain::service::quota::service::QuotaServiceConfig;
use crate::domain::service::quota::status::QuotaWarningView;
use crate::domain::service::quota::test_support::{
    PolicyCall, PolicyStep, default_config, limits, premium_decision, standard_decision, utc,
};
use crate::domain::service::quota::types::{QuotaPeriod, QuotaTier};
use crate::quota_test_env::{BucketSeed, Options, QuotaEnv, StreamReq, TurnSeed};
use crate::test_doubles::{
    OTHER_TENANT, OTHER_USER, PdpReply, USER, USER_TENANT, other_tenant_ctx, other_user_ctx,
    user_ctx,
};

const URI: &str = "/mini-chat/v1/quota/status";

/// Limits of the examples: total `100_000_000` daily and `1_000_000_000` monthly,
/// premium `40_000_000` and `400_000_000`.
fn example_limits() -> UserLimits {
    limits(7, 100_000_000, 1_000_000_000, 40_000_000, 400_000_000)
}

async fn env_with_limits(limits: UserLimits) -> QuotaEnv {
    QuotaEnv::with(Options {
        limits,
        ..Options::default()
    })
    .await
}

fn tier<'a>(reply: &'a Reply, name: &str) -> &'a Value {
    reply.json["tiers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["tier"] == name)
        .unwrap_or_else(|| panic!("no tier {name}: {}", reply.text))
}

fn period<'a>(reply: &'a Reply, tier_name: &str, period_name: &str) -> &'a Value {
    tier(reply, tier_name)["periods"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["period"] == period_name)
        .unwrap_or_else(|| panic!("no period {period_name} in {tier_name}: {}", reply.text))
}

fn keys(value: &Value) -> BTreeSet<&str> {
    value
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect()
}

// ================================================================================================
// Shape and arithmetic
// ================================================================================================

#[tokio::test]
async fn the_status_has_exactly_the_documented_shape_and_order() {
    let env = env_with_limits(example_limits()).await;

    let reply = env.get(URI).await;

    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text);
    assert_eq!(
        keys(&reply.json),
        BTreeSet::from(["tiers", "warning_threshold_pct"])
    );
    assert_eq!(reply.json["warning_threshold_pct"], 80);
    let tiers = reply.json["tiers"].as_array().unwrap();
    assert_eq!(
        tiers
            .iter()
            .map(|t| t["tier"].as_str().unwrap())
            .collect::<Vec<_>>(),
        vec!["premium", "total"]
    );
    for tier in tiers {
        assert_eq!(keys(tier), BTreeSet::from(["tier", "periods"]));
        let periods = tier["periods"].as_array().unwrap();
        assert_eq!(
            periods
                .iter()
                .map(|p| p["period"].as_str().unwrap())
                .collect::<Vec<_>>(),
            vec!["daily", "monthly"]
        );
        for period in periods {
            assert_eq!(
                keys(period),
                BTreeSet::from([
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
        }
    }
}

#[tokio::test]
async fn the_limits_come_from_the_premium_and_standard_user_limits() {
    let env = env_with_limits(example_limits()).await;

    let reply = env.get(URI).await;

    assert_eq!(
        period(&reply, "premium", "daily")["limit_credits_micro"],
        40_000_000
    );
    assert_eq!(
        period(&reply, "premium", "monthly")["limit_credits_micro"],
        400_000_000
    );
    assert_eq!(
        period(&reply, "total", "daily")["limit_credits_micro"],
        100_000_000
    );
    assert_eq!(
        period(&reply, "total", "monthly")["limit_credits_micro"],
        1_000_000_000
    );
}

#[tokio::test]
async fn a_user_without_usage_has_nothing_used_and_a_full_percentage() {
    let env = env_with_limits(example_limits()).await;

    let reply = env.get(URI).await;

    for tier_name in ["premium", "total"] {
        for period_name in ["daily", "monthly"] {
            let p = period(&reply, tier_name, period_name);
            assert_eq!(p["used_credits_micro"], 0, "{tier_name} {period_name}");
            assert_eq!(p["remaining_credits_micro"], p["limit_credits_micro"]);
            assert_eq!(p["remaining_percentage"], 100);
            assert_eq!(p["warning"], false);
            assert_eq!(p["exhausted"], false);
        }
    }
}

#[tokio::test]
async fn used_is_spent_plus_reserved_and_the_percentage_is_floored() {
    let env = env_with_limits(example_limits()).await;
    env.seed_bucket(&BucketSeed::total("daily", 50_000_000, 30_000_001))
        .await;

    let reply = env.get(URI).await;

    let daily = period(&reply, "total", "daily");
    assert_eq!(daily["used_credits_micro"], 80_000_001);
    assert_eq!(daily["remaining_credits_micro"], 19_999_999);
    assert_eq!(daily["remaining_percentage"], 19);
    assert_eq!(daily["warning"], true);
    assert_eq!(daily["exhausted"], false);
}

#[tokio::test]
async fn less_than_one_percent_remaining_is_exhausted_and_over_the_limit_clamps_remaining_only() {
    let env = env_with_limits(example_limits()).await;
    env.seed_bucket(&BucketSeed::total("daily", 99_000_001, 0))
        .await;
    env.seed_bucket(&BucketSeed::total("monthly", 900_000_000, 200_000_000))
        .await;

    let reply = env.get(URI).await;

    let daily = period(&reply, "total", "daily");
    assert_eq!(daily["remaining_credits_micro"], 999_999);
    assert_eq!(daily["remaining_percentage"], 0);
    assert_eq!(daily["exhausted"], true);
    assert_eq!(daily["warning"], true);
    let monthly = period(&reply, "total", "monthly");
    assert_eq!(monthly["used_credits_micro"], 1_100_000_000, "not clamped");
    assert_eq!(monthly["remaining_credits_micro"], 0);
    assert_eq!(monthly["remaining_percentage"], 0);
    assert_eq!(monthly["exhausted"], true);
}

#[tokio::test]
async fn the_premium_tier_reads_the_premium_bucket_and_total_the_total_bucket() {
    let env = env_with_limits(example_limits()).await;
    env.seed_bucket(&BucketSeed::premium("daily", 10_000_000, 0))
        .await;
    env.seed_bucket(&BucketSeed::total("daily", 20_000_000, 5_000_000))
        .await;

    let reply = env.get(URI).await;

    assert_eq!(
        period(&reply, "premium", "daily")["used_credits_micro"],
        10_000_000
    );
    assert_eq!(
        period(&reply, "premium", "daily")["remaining_percentage"],
        75
    );
    assert_eq!(
        period(&reply, "total", "daily")["used_credits_micro"],
        25_000_000
    );
}

#[tokio::test]
async fn the_threshold_of_the_configuration_decides_the_warning_and_is_reported() {
    let env = QuotaEnv::with(Options {
        config: QuotaServiceConfig {
            quota: QuotaConfig {
                warning_threshold_pct: 60,
                ..QuotaConfig::default()
            },
            ..default_config()
        },
        limits: example_limits(),
        ..Options::default()
    })
    .await;
    // 40% remaining: a warning at threshold 60 fires at 40 remaining or less
    env.seed_bucket(&BucketSeed::total("daily", 60_000_000, 0))
        .await;
    env.seed_bucket(&BucketSeed::total("monthly", 600_000_000, 0))
        .await;

    let reply = env.get(URI).await;

    assert_eq!(reply.json["warning_threshold_pct"], 60);
    assert_eq!(period(&reply, "total", "daily")["remaining_percentage"], 40);
    assert_eq!(
        period(&reply, "total", "daily")["warning"],
        true,
        "40 <= 100 - 60"
    );
    assert_eq!(period(&reply, "total", "monthly")["warning"], true);
    assert_eq!(period(&reply, "premium", "daily")["warning"], false);
}

// ================================================================================================
// Resets
// ================================================================================================

#[tokio::test]
async fn next_reset_is_the_next_utc_midnight_and_the_first_of_the_next_month() {
    let env = QuotaEnv::with(Options {
        now: utc(2026, 2, 28, 23, 59, 59),
        limits: example_limits(),
        ..Options::default()
    })
    .await;

    let reply = env.get(URI).await;

    for tier_name in ["premium", "total"] {
        assert_eq!(
            period(&reply, tier_name, "daily")["next_reset"],
            "2026-03-01T00:00:00Z"
        );
        assert_eq!(
            period(&reply, tier_name, "monthly")["next_reset"],
            "2026-03-01T00:00:00Z"
        );
    }
}

#[tokio::test]
async fn the_reset_after_the_last_day_of_the_year_is_in_the_next_year() {
    let env = QuotaEnv::with(Options {
        now: utc(2026, 12, 31, 12, 0, 0),
        limits: example_limits(),
        ..Options::default()
    })
    .await;

    let reply = env.get(URI).await;

    assert_eq!(
        period(&reply, "total", "daily")["next_reset"],
        "2027-01-01T00:00:00Z"
    );
    assert_eq!(
        period(&reply, "total", "monthly")["next_reset"],
        "2027-01-01T00:00:00Z"
    );
}

#[tokio::test]
async fn only_the_rows_of_the_current_utc_periods_are_counted() {
    let env = env_with_limits(example_limits()).await;
    env.seed_bucket(&BucketSeed::total("daily", 11_000_000, 0).at("2026-03-09"))
        .await;
    env.seed_bucket(&BucketSeed::total("monthly", 22_000_000, 0).at("2026-02-01"))
        .await;
    env.seed_bucket(&BucketSeed::total("daily", 1_000_000, 0))
        .await;
    env.seed_bucket(&BucketSeed::total("monthly", 2_000_000, 0))
        .await;

    let reply = env.get(URI).await;

    assert_eq!(
        period(&reply, "total", "daily")["used_credits_micro"],
        1_000_000
    );
    assert_eq!(
        period(&reply, "total", "monthly")["used_credits_micro"],
        2_000_000
    );
}

// ================================================================================================
// Omissions
// ================================================================================================

#[tokio::test]
async fn a_period_with_a_zero_limit_is_omitted_and_an_empty_tier_is_still_listed() {
    let env = env_with_limits(limits(7, 100_000_000, 0, 0, 0)).await;

    let reply = env.get(URI).await;

    let total = tier(&reply, "total")["periods"].as_array().unwrap();
    assert_eq!(total.len(), 1);
    assert_eq!(total[0]["period"], "daily");
    assert_eq!(tier(&reply, "premium")["periods"], json!([]));
}

// ================================================================================================
// Isolation
// ================================================================================================

#[tokio::test]
async fn another_users_or_tenants_buckets_never_appear() {
    let env = env_with_limits(example_limits()).await;
    env.seed_bucket(&BucketSeed::total("daily", 5_000_000, 0))
        .await;
    env.seed_bucket(&BucketSeed::total("daily", 90_000_000, 0).of(USER_TENANT, OTHER_USER))
        .await;
    env.seed_bucket(&BucketSeed::total("daily", 91_000_000, 0).of(OTHER_TENANT, USER))
        .await;
    env.seed_bucket(&BucketSeed::total("daily", 92_000_000, 0).of(OTHER_TENANT, OTHER_USER))
        .await;

    let mine = env.get_as(&user_ctx(), URI).await;
    let theirs = env.get_as(&other_user_ctx(), URI).await;
    let foreign = env.get_as(&other_tenant_ctx(), URI).await;

    assert_eq!(
        period(&mine, "total", "daily")["used_credits_micro"],
        5_000_000
    );
    assert_eq!(
        period(&theirs, "total", "daily")["used_credits_micro"],
        90_000_000
    );
    assert_eq!(
        period(&foreign, "total", "daily")["used_credits_micro"],
        92_000_000
    );
}

#[tokio::test]
async fn any_query_string_is_ignored() {
    let env = env_with_limits(example_limits()).await;
    env.seed_bucket(&BucketSeed::total("daily", 5_000_000, 0))
        .await;

    let plain = env.get(URI).await;
    let with_query = env
        .get(&format!("{URI}?limit=5&tier=premium&$filter=x"))
        .await;

    assert_eq!(with_query.status, StatusCode::OK, "{}", with_query.text);
    assert_eq!(with_query.json, plain.json);
}

// ================================================================================================
// After a turn
// ================================================================================================

#[tokio::test]
async fn used_grows_by_the_reserve_and_after_the_settlement_by_exactly_the_committed_credits() {
    let env = env_with_limits(example_limits()).await;
    let chat = env.seed_chat(1).await;
    env.seed_bucket(&BucketSeed::total("daily", 25_000_000, 0))
        .await;
    env.seed_bucket(&BucketSeed::total("monthly", 240_000_000, 0))
        .await;
    let used = |reply: &Reply, p: &str| {
        period(reply, "total", p)["used_credits_micro"]
            .as_i64()
            .unwrap()
    };
    let baseline = env.get(URI).await;
    assert_eq!(used(&baseline, "daily"), 25_000_000);

    // the reserve of a standard turn is in flight: it counts at once
    let turn = env.seed_turn(&TurnSeed::standard(1, chat)).await;
    env.repo_reserve_standard().await;
    let reserved = env.get(URI).await;
    assert_eq!(used(&reserved, "daily"), 26_500_000);
    assert_eq!(used(&reserved, "monthly"), 241_500_000);

    // the turn settles 900 input and 300 output tokens
    env.repo_finalize(&StreamReq::completed(turn).usage(900, 300))
        .await;
    let settled = env.get(URI).await;
    assert_eq!(used(&settled, "daily"), 25_000_000 + 1_200_000);
    assert_eq!(used(&settled, "monthly"), 240_000_000 + 1_200_000);
    // the premium tier did not move: the turn was standard
    assert_eq!(
        period(&settled, "premium", "daily")["used_credits_micro"],
        0
    );
}

#[tokio::test]
async fn a_premium_turn_moves_both_tiers_of_the_status() {
    let env = env_with_limits(example_limits()).await;
    let chat = env.seed_chat(1).await;
    let turn = env.seed_turn(&TurnSeed::premium(1, chat)).await;
    env.repo_reserve_premium().await;

    let reserved = env.get(URI).await;
    assert_eq!(
        period(&reserved, "premium", "daily")["used_credits_micro"],
        3_750_000
    );
    assert_eq!(
        period(&reserved, "total", "daily")["used_credits_micro"],
        3_750_000
    );

    env.repo_finalize(&StreamReq::completed(turn).usage(900, 300))
        .await;
    let settled = env.get(URI).await;
    assert_eq!(
        period(&settled, "premium", "daily")["used_credits_micro"],
        3_000_000
    );
    assert_eq!(
        period(&settled, "total", "daily")["used_credits_micro"],
        3_000_000
    );
}

// ================================================================================================
// Policy and authorization
// ================================================================================================

#[tokio::test]
async fn the_status_reads_the_current_version_and_its_limits_but_never_the_snapshot() {
    let env = env_with_limits(example_limits()).await;

    env.get(URI).await;

    assert_eq!(
        env.policy.calls(),
        vec![PolicyCall::Version(USER), PolicyCall::Limits(USER, 7)]
    );
}

#[tokio::test]
async fn a_failing_policy_plugin_is_a_500_internal_without_leaking_the_cause() {
    for step in [PolicyStep::Version, PolicyStep::Limits] {
        let env = env_with_limits(example_limits()).await;
        assert_eq!(
            env.get(URI).await.status,
            StatusCode::OK,
            "control: the status is served while the plugin works"
        );
        env.policy.fail(step);

        let reply = env.get(URI).await;

        assert_eq!(
            reply.status,
            StatusCode::INTERNAL_SERVER_ERROR,
            "{step:?}: {}",
            reply.text
        );
        assert!(reply.json["type"].as_str().unwrap().contains("internal"));
        assert!(!reply.text.contains("failed"), "{}", reply.text);
    }
}

#[tokio::test]
async fn a_database_error_is_a_500_internal() {
    use sea_orm::ConnectionTrait;
    let env = env_with_limits(example_limits()).await;
    assert_eq!(
        env.get(URI).await.status,
        StatusCode::OK,
        "control: the status is served while the table exists"
    );
    env.db
        .raw
        .execute_unprepared("DROP TABLE quota_usage")
        .await
        .unwrap();

    let reply = env.get(URI).await;

    assert_eq!(
        reply.status,
        StatusCode::INTERNAL_SERVER_ERROR,
        "{}",
        reply.text
    );
    assert!(reply.json["type"].as_str().unwrap().contains("internal"));
}

#[tokio::test]
async fn a_pdp_deny_is_403_authz_denied_and_nothing_is_read() {
    let env = QuotaEnv::with(Options {
        pdp: PdpReply::Deny,
        limits: example_limits(),
        ..Options::default()
    })
    .await;

    let reply = env.get(URI).await;

    assert_eq!(reply.status, StatusCode::FORBIDDEN, "{}", reply.text);
    assert_eq!(reply.json["context"]["reason"], "AUTHZ_DENIED");
    assert!(env.policy.calls().is_empty());
}

#[tokio::test]
async fn a_pdp_failure_is_503_with_retry_after_five() {
    let env = QuotaEnv::with(Options {
        pdp: PdpReply::Failure,
        limits: example_limits(),
        ..Options::default()
    })
    .await;

    let reply = env.get(URI).await;

    assert_eq!(
        reply.status,
        StatusCode::SERVICE_UNAVAILABLE,
        "{}",
        reply.text
    );
    assert_eq!(reply.header("retry-after"), Some("5"));
    assert_eq!(reply.json["context"]["retry_after_seconds"], 5);
    assert!(env.policy.calls().is_empty());
}

#[tokio::test]
async fn the_pdp_is_asked_for_read_on_the_user_quota_resource_without_a_resource_id() {
    let env = env_with_limits(example_limits()).await;

    env.get(URI).await;

    let requests = env.pdp.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].action.name, "read");
    assert_eq!(
        requests[0].resource.resource_type,
        "gts.cf.core.ai_chat.user_quota.v1~cf.core.mini_chat.user_quota.v1~"
    );
    assert_eq!(requests[0].resource.id, None);
    assert!(requests[0].context.require_constraints);
}

// ================================================================================================
// Registration
// ================================================================================================

fn spec(env: &QuotaEnv, prefix: &str) -> toolkit::api::operation_builder::OperationSpec {
    let (_router, openapi) = env.router(prefix);
    let key = format!("GET:{}/v1/quota/status", prefix.trim_end_matches('/'));
    openapi
        .operation_specs
        .get(&key)
        .unwrap_or_else(|| panic!("{key} is not registered"))
        .value()
        .clone()
}

#[tokio::test]
async fn the_route_is_registered_authenticated_license_gated_and_documented() {
    let env = env_with_limits(example_limits()).await;

    let spec = spec(&env, "/mini-chat");

    assert!(spec.authenticated);
    assert_eq!(
        spec.license_requirement.unwrap().license_names,
        vec![BASE_LICENSE_FEATURE.to_owned()]
    );
    assert_eq!(
        spec.operation_id.as_deref(),
        Some("mini_chat.get_quota_status")
    );
    assert_eq!(spec.tags, vec!["Mini Chat Quotas".to_owned()]);
    assert!(spec.summary.as_deref().is_some_and(|s| !s.is_empty()));
    assert!(spec.request_body.is_none());
    let statuses: BTreeSet<u16> = spec.responses.iter().map(|r| r.status).collect();
    assert_eq!(statuses, BTreeSet::from([200, 401, 403, 500, 503]));
    let ok = spec.responses.iter().find(|r| r.status == 200).unwrap();
    assert_eq!(
        ok.schema.as_ref().unwrap().schema_name(),
        "QuotaStatusResponse"
    );
    let unavailable = spec.responses.iter().find(|r| r.status == 503).unwrap();
    assert!(
        unavailable
            .headers
            .iter()
            .any(|h| h.name.eq_ignore_ascii_case("retry-after")),
        "{:?}",
        unavailable.headers
    );
    assert!(spec.params.is_empty(), "no query parameter is defined");
}

#[tokio::test]
async fn the_route_follows_a_configured_url_prefix() {
    let env = env_with_limits(example_limits()).await;

    let spec = spec(&env, "/chat-api/");

    assert_eq!(spec.path, "/chat-api/v1/quota/status");
}

#[tokio::test]
async fn exactly_one_operation_is_registered_by_the_quota_routes() {
    let env = env_with_limits(example_limits()).await;
    let (_router, openapi) = env.router("/mini-chat");

    let keys: Vec<String> = openapi
        .operation_specs
        .iter()
        .map(|e| e.key().clone())
        .collect();

    assert_eq!(keys, vec!["GET:/mini-chat/v1/quota/status".to_owned()]);
}

// ================================================================================================
// The `quota_warnings` DTO of the SSE `done` event
// ================================================================================================

#[test]
fn a_warning_entry_serializes_next_reset_only_when_present() {
    let with_reset = QuotaWarning::from(QuotaWarningView {
        tier: QuotaTier::Premium,
        period: QuotaPeriod::Monthly,
        remaining_percentage: 12,
        warning: true,
        exhausted: false,
        next_reset: Some(utc(2026, 4, 1, 0, 0, 0)),
    });
    let without_reset = QuotaWarning::from(QuotaWarningView {
        tier: QuotaTier::Total,
        period: QuotaPeriod::Daily,
        remaining_percentage: 100,
        warning: false,
        exhausted: false,
        next_reset: None,
    });

    let first = serde_json::to_value(&with_reset).unwrap();
    let second = serde_json::to_value(&without_reset).unwrap();

    assert_eq!(
        first,
        json!({
            "tier": "premium",
            "period": "monthly",
            "remaining_percentage": 12,
            "warning": true,
            "exhausted": false,
            "next_reset": "2026-04-01T00:00:00Z"
        })
    );
    assert_eq!(
        second,
        json!({
            "tier": "total",
            "period": "daily",
            "remaining_percentage": 100,
            "warning": false,
            "exhausted": false
        })
    );
}

impl QuotaEnv {
    /// Reserves a standard turn (DESIGN 5.10, `1_500_000`) of the default user.
    async fn repo_reserve_standard(&self) {
        use crate::domain::service::quota::ports::QuotaRepository;
        use crate::domain::service::quota::reserve::{ReserveMode, ReserveRequest};
        let decision = standard_decision();
        self.repo
            .reserve(&ReserveRequest {
                tenant_id: USER_TENANT,
                user_id: USER,
                mode: ReserveMode::Send,
                decision: &decision,
                now: self.options.now,
            })
            .await
            .unwrap();
    }

    /// Reserves a premium turn (`3_750_000`) of the default user.
    async fn repo_reserve_premium(&self) {
        use crate::domain::service::quota::ports::QuotaRepository;
        use crate::domain::service::quota::reserve::{ReserveMode, ReserveRequest};
        let decision = premium_decision();
        self.repo
            .reserve(&ReserveRequest {
                tenant_id: USER_TENANT,
                user_id: USER,
                mode: ReserveMode::Send,
                decision: &decision,
                now: self.options.now,
            })
            .await
            .unwrap();
    }

    /// Finalizes a turn through the repository.
    async fn repo_finalize(&self, request: &StreamReq) {
        use crate::domain::service::quota::ports::QuotaRepository;
        self.repo.finalize_turn(&request.build()).await.unwrap();
    }
}
