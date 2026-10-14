//! In-process tests of `GET /v1/quota/status` over `SQLite` with the real routes, a recording
//! PDP and a fake model-policy gateway: the 200 body for premium and total, daily and monthly,
//! empty and partly used rows, the omitted zero limit, the configured threshold, the owner and
//! tenant isolation through a tenant-only PDP, consistency with the reserve write and the
//! settlement, the PEP request, the 403, 503 and 500 Problems and the declared operation.
//!
//! Slice S1-all: flow `cpt-cf-mini-chat-flow-quota-and-billing-quota-status`, algorithm
//! `cpt-cf-mini-chat-algo-quota-and-billing-status-computation`, definitions of done
//! `cpt-cf-mini-chat-dod-quota-and-billing-status-api` and
//! `cpt-cf-mini-chat-dod-quota-and-billing-warning-thresholds`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeSet;
use std::sync::atomic::Ordering;

use axum::http::StatusCode;
use serde_json::{Value, json};
use toolkit::api::OpenApiInfo;
use toolkit_security::SecurityContext;

use crate::domain::quota::{
    FinalizeRequest, Finalizer, TerminalState, ToolCallCounts, TurnTerminalUpdate,
};
use crate::test_support::chats::{a1, a2, assert_problem, b1};
use crate::test_support::quota::{
    QuotaGateway, QuotaRigOptions, STANDARD, STATUS_URI, StatusRig, default_settings,
    design_catalog, plain_facts, row, seed_rows, status_rig, status_rig_with, usage, utc,
};
use crate::test_support::{Pdp, TENANT_A, TENANT_B, USER_A1, USER_A2, raw_exec};

const DAY: &str = "2026-02-28";
const MONTH: &str = "2026-02-01";
const BASE_LICENSE: &str = "gts.cf.core.lic.feat.v1~cf.core.global.base.v1";
const QUOTA_TYPE: &str = "gts.cf.core.ai_chat.user_quota.v1~cf.core.mini_chat.user_quota.v1~";

fn keys(value: &Value) -> BTreeSet<&str> {
    value
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect()
}

fn period<'a>(body: &'a Value, tier: &str, period: &str) -> &'a Value {
    body["tiers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["tier"] == tier)
        .unwrap_or_else(|| panic!("tier {tier} missing in {body}"))["periods"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["period"] == period)
        .unwrap_or_else(|| panic!("period {period} missing in {body}"))
}

async fn ok_body(rig: &StatusRig, who: &SecurityContext) -> Value {
    let reply = rig.get_status(who).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text);
    reply.json()
}

// ── the 200 body ─────────────────────────────────────────────────────────────

#[tokio::test]
async fn a_user_without_usage_rows_sees_zero_used_and_one_hundred_percent_everywhere() {
    let rig = status_rig(Pdp::TenantOnly).await;
    let reply = rig.get_status(&a1()).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text);
    assert_eq!(reply.content_type, "application/json");
    let body = reply.json();
    assert_eq!(
        keys(&body),
        BTreeSet::from(["tiers", "warning_threshold_pct"])
    );
    assert_eq!(body["warning_threshold_pct"], 80);
    let tiers = body["tiers"].as_array().unwrap();
    assert_eq!(
        tiers
            .iter()
            .map(|t| t["tier"].as_str().unwrap())
            .collect::<Vec<_>>(),
        vec!["premium", "total"]
    );
    for tier in tiers {
        let periods = tier["periods"].as_array().unwrap();
        assert_eq!(
            periods
                .iter()
                .map(|p| p["period"].as_str().unwrap())
                .collect::<Vec<_>>(),
            vec!["daily", "monthly"]
        );
        for entry in periods {
            assert_eq!(
                keys(entry),
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
            assert_eq!(entry["used_credits_micro"], 0);
            assert_eq!(
                entry["remaining_credits_micro"],
                entry["limit_credits_micro"]
            );
            assert_eq!(entry["remaining_percentage"], 100);
            assert_eq!(entry["warning"], false);
            assert_eq!(entry["exhausted"], false);
        }
    }
    assert_eq!(
        period(&body, "premium", "daily")["limit_credits_micro"],
        22_000_000
    );
    assert_eq!(
        period(&body, "premium", "monthly")["limit_credits_micro"],
        300_000_000
    );
    assert_eq!(
        period(&body, "total", "daily")["limit_credits_micro"],
        60_000_000
    );
    assert_eq!(
        period(&body, "total", "monthly")["limit_credits_micro"],
        600_000_000
    );
}

#[tokio::test]
async fn next_reset_is_midnight_utc_tomorrow_for_daily_and_the_first_of_next_month_for_monthly() {
    let rig = status_rig(Pdp::TenantOnly).await;
    let body = ok_body(&rig, &a1()).await;
    for tier in ["premium", "total"] {
        assert_eq!(
            period(&body, tier, "daily")["next_reset"],
            "2026-03-01T00:00:00Z"
        );
        assert_eq!(
            period(&body, tier, "monthly")["next_reset"],
            "2026-03-01T00:00:00Z"
        );
    }
}

#[tokio::test]
async fn used_is_spent_plus_reserved_with_floored_percentages_and_the_warning_flag() {
    let rig = status_rig(Pdp::TenantOnly).await;
    seed_rows(
        &rig.rig.db,
        &[
            row("daily", DAY, "tier:premium").spent(20_000_000),
            row("monthly", MONTH, "tier:premium").spent(200_000_000),
            row("daily", DAY, "total")
                .spent(25_000_000)
                .reserved(3_000_000),
            row("monthly", MONTH, "total").spent(240_000_000),
        ],
    )
    .await;
    let body = ok_body(&rig, &a1()).await;

    let premium_daily = period(&body, "premium", "daily");
    assert_eq!(premium_daily["used_credits_micro"], 20_000_000);
    assert_eq!(premium_daily["remaining_credits_micro"], 2_000_000);
    assert_eq!(premium_daily["remaining_percentage"], 9);
    assert_eq!(premium_daily["warning"], true);
    assert_eq!(premium_daily["exhausted"], false);

    let premium_monthly = period(&body, "premium", "monthly");
    assert_eq!(premium_monthly["remaining_percentage"], 33);
    assert_eq!(premium_monthly["warning"], false);

    let total_daily = period(&body, "total", "daily");
    assert_eq!(total_daily["used_credits_micro"], 28_000_000);
    assert_eq!(total_daily["remaining_credits_micro"], 32_000_000);
    assert_eq!(total_daily["remaining_percentage"], 53);

    let total_monthly = period(&body, "total", "monthly");
    assert_eq!(total_monthly["used_credits_micro"], 240_000_000);
    assert_eq!(total_monthly["remaining_percentage"], 60);
}

#[tokio::test]
async fn an_exhausted_period_is_flagged_and_the_remainder_never_goes_below_zero() {
    let rig = status_rig(Pdp::TenantOnly).await;
    seed_rows(
        &rig.rig.db,
        &[
            row("daily", DAY, "tier:premium").spent(23_000_000),
            row("daily", DAY, "total").spent(59_999_000),
        ],
    )
    .await;
    let body = ok_body(&rig, &a1()).await;
    let premium = period(&body, "premium", "daily");
    assert_eq!(premium["used_credits_micro"], 23_000_000);
    assert_eq!(premium["remaining_credits_micro"], 0);
    assert_eq!(premium["remaining_percentage"], 0);
    assert_eq!(premium["exhausted"], true);
    assert_eq!(premium["warning"], true);
    // less than one percent remaining floors to 0 and counts as exhausted
    let total = period(&body, "total", "daily");
    assert_eq!(total["remaining_credits_micro"], 1_000);
    assert_eq!(total["remaining_percentage"], 0);
    assert_eq!(total["exhausted"], true);
}

#[tokio::test]
async fn a_period_with_a_zero_limit_is_omitted_and_a_tier_without_periods_disappears() {
    let rig = status_rig(Pdp::TenantOnly).await;
    rig.rig
        .gateway
        .set_limits((60_000_000, 600_000_000), (0, 300_000_000));
    let body = ok_body(&rig, &a1()).await;
    let premium = body["tiers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["tier"] == "premium")
        .unwrap();
    assert_eq!(premium["periods"].as_array().unwrap().len(), 1);
    assert_eq!(premium["periods"][0]["period"], "monthly");

    rig.rig
        .gateway
        .set_limits((60_000_000, 600_000_000), (0, 0));
    let body = ok_body(&rig, &a1()).await;
    let tiers: Vec<&str> = body["tiers"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["tier"].as_str().unwrap())
        .collect();
    assert_eq!(tiers, vec!["total"]);
}

#[tokio::test]
async fn limits_that_are_all_zero_give_an_empty_tiers_array() {
    let rig = status_rig(Pdp::TenantOnly).await;
    rig.rig.gateway.set_limits((0, 0), (0, 0));
    let body = ok_body(&rig, &a1()).await;
    assert_eq!(body, json!({ "tiers": [], "warning_threshold_pct": 80 }));
}

#[tokio::test]
async fn the_configured_threshold_is_reported_and_applied() {
    let mut settings = default_settings();
    settings.warning_threshold_pct = 99;
    let rig = status_rig_with(QuotaRigOptions {
        settings,
        ..QuotaRigOptions::new()
    })
    .await;
    // 59_000_000 of 60_000_000 used: 1 percent remains
    seed_rows(&rig.rig.db, &[row("daily", DAY, "total").spent(59_000_000)]).await;
    let body = ok_body(&rig, &a1()).await;
    assert_eq!(body["warning_threshold_pct"], 99);
    assert_eq!(period(&body, "total", "daily")["remaining_percentage"], 1);
    assert_eq!(period(&body, "total", "daily")["warning"], true);
    assert_eq!(period(&body, "total", "monthly")["warning"], false);
}

#[tokio::test]
async fn tool_quotas_billing_and_settlement_details_are_never_part_of_the_response() {
    let rig = status_rig(Pdp::TenantOnly).await;
    seed_rows(
        &rig.rig.db,
        &[row("daily", DAY, "total").spent(1).web(70).code(40)],
    )
    .await;
    let reply = rig.get_status(&a1()).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text);
    for banned in [
        "web_search",
        "code_interpreter",
        "billing",
        "settlement",
        "calls",
        "tokens",
    ] {
        assert!(!reply.text.contains(banned), "{banned} in {}", reply.text);
    }
}

// ── isolation ────────────────────────────────────────────────────────────────

#[tokio::test]
async fn the_rows_of_another_user_and_of_another_tenant_never_appear() {
    // a PDP that returns only a tenant predicate still yields only the caller's rows
    let rig = status_rig(Pdp::TenantOnly).await;
    seed_rows(
        &rig.rig.db,
        &[
            row("daily", DAY, "total").spent(10_000_000),
            row("daily", DAY, "total")
                .spent(59_000_000)
                .of(TENANT_A, USER_A2),
            row("daily", DAY, "total")
                .spent(58_000_000)
                .of(TENANT_B, USER_A1),
        ],
    )
    .await;
    let body = ok_body(&rig, &a1()).await;
    assert_eq!(
        period(&body, "total", "daily")["used_credits_micro"],
        10_000_000
    );

    let other = ok_body(&rig, &a2()).await;
    assert_eq!(
        period(&other, "total", "daily")["used_credits_micro"],
        59_000_000
    );

    let foreign = ok_body(&rig, &b1()).await;
    assert_eq!(period(&foreign, "total", "daily")["used_credits_micro"], 0);
}

// ── consistency with the reserve write and the settlement ────────────────────

#[tokio::test]
async fn the_status_follows_the_reserve_write_and_the_settlement() {
    let rig = status_rig(Pdp::TenantOnly).await;
    let chat = rig.rig.chat_a1(STANDARD).await;
    let booked = rig.rig.book(&chat, STANDARD, plain_facts()).await;

    // after the reserve write the used amount rises by the reserved credits
    let body = ok_body(&rig, &a1()).await;
    let daily = period(&body, "total", "daily");
    assert_eq!(daily["used_credits_micro"], 1_500_000);
    assert_eq!(daily["remaining_credits_micro"], 58_500_000);
    assert_eq!(daily["remaining_percentage"], 97);
    assert_eq!(
        period(&body, "total", "monthly")["used_credits_micro"],
        1_500_000
    );
    assert_eq!(
        period(&body, "premium", "daily")["used_credits_micro"],
        0,
        "a standard turn leaves the premium sub-cap alone"
    );

    // after the settlement it equals the committed credits and no reserve remains
    let finalize = FinalizeRequest {
        tenant_id: TENANT_A,
        turn_id: booked.turn_id,
        update: TurnTerminalUpdate {
            state: TerminalState::Completed,
            error_code: None,
            error_detail: None,
            assistant_message_id: None,
            completed_at: utc(2026, 2, 28, 15, 31, 0),
        },
        finalizer: Finalizer::Stream {
            selected_model: STANDARD.to_owned(),
            period_starts: booked.decision.period_starts,
        },
        usage: Some(usage(900, 300)),
        tool_counts: ToolCallCounts::default(),
    };
    rig.rig.finalize(&finalize).await.unwrap();
    let body = ok_body(&rig, &a1()).await;
    let daily = period(&body, "total", "daily");
    assert_eq!(daily["used_credits_micro"], 1_200_000);
    assert_eq!(daily["remaining_credits_micro"], 58_800_000);
    assert_eq!(daily["remaining_percentage"], 98);
    assert_eq!(
        period(&body, "total", "monthly")["used_credits_micro"],
        1_200_000
    );
}

#[tokio::test]
async fn a_premium_reserve_shows_in_both_tiers() {
    let rig = status_rig(Pdp::TenantOnly).await;
    let chat = rig.rig.chat_a1("premium-1").await;
    rig.rig.book(&chat, "premium-1", plain_facts()).await;
    let body = ok_body(&rig, &a1()).await;
    assert_eq!(
        period(&body, "premium", "daily")["used_credits_micro"],
        3_750_000
    );
    assert_eq!(
        period(&body, "total", "daily")["used_credits_micro"],
        3_750_000
    );
}

// ── the PEP ──────────────────────────────────────────────────────────────────

#[tokio::test]
async fn the_pep_evaluates_read_on_the_user_quota_resource_with_constraints_required() {
    let rig = status_rig(Pdp::TenantAndOwner).await;
    ok_body(&rig, &a1()).await;
    let request = rig.rig.pdp.only_request();
    assert_eq!(request.action.name, "read");
    assert_eq!(request.resource.resource_type, QUOTA_TYPE);
    assert_eq!(request.resource.id, None);
    assert!(request.context.require_constraints);
}

#[tokio::test]
async fn a_pdp_deny_is_a_403_authz_denied_and_nothing_is_read() {
    let rig = status_rig(Pdp::Deny).await;
    let reply = rig.get_status(&a1()).await;
    let body = assert_problem(&reply, 403, "permission_denied");
    assert_eq!(body["context"]["reason"], "AUTHZ_DENIED");
    assert_eq!(
        rig.rig.gateway.total_calls(),
        0,
        "the policy is not read after a refusal"
    );
}

#[tokio::test]
async fn a_decision_without_usable_constraints_is_a_403() {
    let rig = status_rig(Pdp::NoConstraints).await;
    let reply = rig.get_status(&a1()).await;
    let body = assert_problem(&reply, 403, "permission_denied");
    assert_eq!(body["context"]["reason"], "AUTHZ_DENIED");
}

#[tokio::test]
async fn a_pdp_failure_is_a_503_with_retry_after_5() {
    let rig = status_rig(Pdp::Unreachable).await;
    let reply = rig.get_status(&a1()).await;
    assert_problem(&reply, 503, "service_unavailable");
    assert_eq!(reply.retry_after.as_deref(), Some("5"));
    assert_eq!(rig.rig.gateway.total_calls(), 0);
}

// ── 500 ──────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn a_plugin_failure_is_a_generic_500() {
    for fail_limits in [false, true] {
        let rig = status_rig(Pdp::TenantOnly).await;
        if fail_limits {
            rig.rig.gateway.fail_limits.store(true, Ordering::SeqCst);
        } else {
            rig.rig.gateway.fail_current.store(true, Ordering::SeqCst);
        }
        let reply = rig.get_status(&a1()).await;
        assert_problem(&reply, 500, "internal");
        assert!(
            !reply.text.contains("provider-secret-detail"),
            "{}",
            reply.text
        );
    }
}

#[tokio::test]
async fn a_usage_read_failure_is_a_generic_500() {
    let rig = status_rig(Pdp::TenantOnly).await;
    raw_exec(&rig.rig.db.raw, "DROP TABLE quota_usage")
        .await
        .unwrap();
    let reply = rig.get_status(&a1()).await;
    assert_problem(&reply, 500, "internal");
    assert!(!reply.text.contains("quota_usage"), "{}", reply.text);
}

#[tokio::test]
async fn every_error_is_a_full_problem_without_a_top_level_code() {
    for (pdp, status) in [(Pdp::Deny, 403), (Pdp::Unreachable, 503)] {
        let rig = status_rig(pdp).await;
        let reply = rig.get_status(&a1()).await;
        let body = assert_problem(&reply, status, "");
        assert!(body.get("code").is_none());
    }
}

// ── the declared operation ───────────────────────────────────────────────────

#[tokio::test]
async fn the_operation_is_registered_under_the_url_prefix_with_its_id_and_tag() {
    let rig = status_rig(Pdp::TenantOnly).await;
    let op = rig
        .openapi
        .op("GET", STATUS_URI)
        .expect("the quota status operation");
    assert_eq!(
        op.operation_id.as_deref(),
        Some("mini_chat.get_quota_status")
    );
    assert_eq!(op.tags, vec!["Mini Chat Quotas".to_owned()]);
    assert!(op.authenticated);
    assert!(op.params.is_empty(), "no query parameters");
    let license = op.license_requirement.as_ref().expect("license feature");
    assert_eq!(license.license_names, vec![BASE_LICENSE.to_owned()]);
}

#[tokio::test]
async fn the_responses_200_401_403_500_and_503_are_declared_with_retry_after_on_503() {
    let rig = status_rig(Pdp::TenantOnly).await;
    let op = rig
        .openapi
        .op("GET", STATUS_URI)
        .expect("the quota status operation");
    let statuses: BTreeSet<u16> = op.responses.iter().map(|r| r.status).collect();
    assert_eq!(statuses, BTreeSet::from([200, 401, 403, 500, 503]));
    let unavailable = op.responses.iter().find(|r| r.status == 503).unwrap();
    assert!(
        unavailable
            .headers
            .iter()
            .any(|h| h.name.eq_ignore_ascii_case("retry-after"))
    );
    for response in op.responses.iter().filter(|r| r.status >= 400) {
        assert_eq!(
            response.content_type, "application/problem+json",
            "{}",
            response.status
        );
    }
}

#[tokio::test]
async fn the_openapi_document_declares_the_status_schemas_and_the_path() {
    let rig = status_rig(Pdp::TenantOnly).await;
    let document = rig
        .openapi
        .inner
        .build_openapi(&OpenApiInfo::default())
        .expect("openapi builds");
    let value = serde_json::to_value(&document).unwrap();
    assert!(value["paths"].as_object().unwrap().contains_key(STATUS_URI));
    let schemas = &value["components"]["schemas"];
    let response_required: BTreeSet<&str> = schemas["QuotaStatusResponseDto"]["required"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert_eq!(
        response_required,
        BTreeSet::from(["tiers", "warning_threshold_pct"])
    );
    let period_required: BTreeSet<&str> = schemas["QuotaPeriodStatusDto"]["required"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert_eq!(
        period_required,
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

#[tokio::test]
async fn the_route_is_registered_exactly_once_next_to_the_foundation_routes() {
    let rig = status_rig(Pdp::TenantOnly).await;
    let ops = rig.openapi.ops.lock().unwrap();
    let count = ops
        .iter()
        .filter(|op| op.method.as_str() == "GET" && op.path == STATUS_URI)
        .count();
    assert_eq!(count, 1);
    assert!(ops.iter().any(|op| op.path.ends_with("/v1/models")));
}

#[tokio::test]
async fn the_policy_is_asked_on_every_status_read() {
    let rig = status_rig_with(QuotaRigOptions {
        gateway: QuotaGateway::serving(
            design_catalog(),
            (60_000_000, 600_000_000),
            (22_000_000, 300_000_000),
        ),
        ..QuotaRigOptions::new()
    })
    .await;
    ok_body(&rig, &a1()).await;
    ok_body(&rig, &a1()).await;
    assert_eq!(rig.rig.gateway.limits_call_count(), 2);
    assert_eq!(
        *rig.rig.gateway.limits_calls.lock().unwrap(),
        vec![(USER_A1, 1), (USER_A1, 1)]
    );
}
