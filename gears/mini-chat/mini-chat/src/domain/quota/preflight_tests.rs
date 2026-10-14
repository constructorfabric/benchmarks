//! Preflight reserve tests over in-memory `SQLite`: the policy binding (no cache, version
//! binding, plugin failures), the web search kill switch, the decision values of the DESIGN 5.10
//! example, the single preflight instant, the daily tool quotas, the prior context read, the
//! metrics recorded before any reserve, and that preflight writes nothing.
//!
//! Slice S1-all: flow `cpt-cf-mini-chat-flow-quota-and-billing-turn-admission`, flow
//! `cpt-cf-mini-chat-flow-quota-and-billing-tool-quota-request`, algorithms
//! `cpt-cf-mini-chat-algo-quota-and-billing-preflight-reserve`,
//! `cpt-cf-mini-chat-algo-quota-and-billing-policy-binding` and
//! `cpt-cf-mini-chat-algo-quota-and-billing-token-estimation` (prior context), definitions of
//! done `cpt-cf-mini-chat-dod-quota-and-billing-preflight-downgrade`,
//! `cpt-cf-mini-chat-dod-quota-and-billing-policy-version`,
//! `cpt-cf-mini-chat-dod-quota-and-billing-tool-quota` and
//! `cpt-cf-mini-chat-dod-quota-and-billing-rejection-contract`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;
use std::sync::atomic::Ordering;

use mini_chat_sdk::{KillSwitches, ModelTier, QuotaDecision};
use uuid::Uuid;

use crate::domain::error::{DomainError, QuotaScope};
use crate::domain::quota::{
    Bucket, BucketLimit, DowngradeReason, PeriodStarts, PeriodType, RequestFacts,
};
use crate::test_support::USER_A1;
use crate::test_support::chats::{at, outbox_body_count};
use crate::test_support::quota::{
    FixedClock, MetricCall, PREMIUM, QuotaGateway, QuotaRigOptions, STANDARD, TickingClock,
    all_quota_rows, catalog_v, date, default_now, default_settings, design_catalog, plain_facts,
    premium_model, quota_rig, quota_rig_with, row, seed_message_with_tokens, seed_rows,
    standard_model, utc, utc_ms,
};

const DAY: &str = "2026-02-28";
const MONTH: &str = "2026-02-01";

/// The DESIGN 5.10 consumption of user 1 of tenant A at the default instant.
async fn seed_design_consumption(db: &crate::test_support::TestDb) {
    seed_rows(
        db,
        &[
            row("daily", DAY, "tier:premium").spent(20_000_000),
            row("monthly", MONTH, "tier:premium").spent(200_000_000),
            row("daily", DAY, "total").spent(25_000_000),
            row("monthly", MONTH, "total").spent(240_000_000),
        ],
    )
    .await;
}

fn assert_policy_unavailable<T: std::fmt::Debug>(result: Result<T, DomainError>) {
    match result {
        Err(DomainError::PolicyUnavailable { .. }) => {}
        other => panic!("expected PolicyUnavailable, got {other:?}"),
    }
}

// ── policy binding ───────────────────────────────────────────────────────────

#[tokio::test]
async fn the_policy_is_asked_on_every_preflight_and_never_cached() {
    let rig = quota_rig().await;
    let chat = rig.chat_a1(PREMIUM).await;
    rig.preflight(&chat, PREMIUM, plain_facts()).await.unwrap();
    rig.preflight(&chat, PREMIUM, plain_facts()).await.unwrap();
    assert_eq!(
        *rig.gateway.current_calls.lock().unwrap(),
        vec![USER_A1, USER_A1]
    );
    assert_eq!(
        *rig.gateway.limits_calls.lock().unwrap(),
        vec![(USER_A1, 1), (USER_A1, 1)]
    );
}

#[tokio::test]
async fn the_version_of_the_current_snapshot_is_bound_to_the_decision_and_used_for_the_limits() {
    let gateway = QuotaGateway::serving(
        catalog_v(7, vec![premium_model(), standard_model()]),
        (60_000_000, 600_000_000),
        (22_000_000, 300_000_000),
    );
    let rig = quota_rig_with(QuotaRigOptions {
        gateway,
        ..QuotaRigOptions::new()
    })
    .await;
    let chat = rig.chat_a1(PREMIUM).await;
    let decision = rig.preflight(&chat, PREMIUM, plain_facts()).await.unwrap();
    assert_eq!(decision.policy_version_applied, 7);
    assert_eq!(
        *rig.gateway.limits_calls.lock().unwrap(),
        vec![(USER_A1, 7)]
    );
}

#[tokio::test]
async fn the_limits_map_total_to_the_standard_limits_and_the_premium_bucket_to_the_premium_limits()
{
    let rig = quota_rig().await;
    let chat = rig.chat_a1(PREMIUM).await;
    let decision = rig.preflight(&chat, PREMIUM, plain_facts()).await.unwrap();
    for (bucket, period, limit) in [
        (Bucket::Total, PeriodType::Daily, 60_000_000),
        (Bucket::Total, PeriodType::Monthly, 600_000_000),
        (Bucket::TierPremium, PeriodType::Daily, 22_000_000),
        (Bucket::TierPremium, PeriodType::Monthly, 300_000_000),
    ] {
        assert!(
            decision.limits.contains(&BucketLimit {
                period,
                bucket,
                limit_credits_micro: limit
            }),
            "{bucket:?} {period:?}: {:?}",
            decision.limits
        );
    }
}

#[tokio::test]
async fn a_snapshot_failure_is_policy_unavailable_and_nothing_is_recorded_or_written() {
    let rig = quota_rig().await;
    rig.gateway.fail_current.store(true, Ordering::SeqCst);
    let chat = rig.chat_a1(PREMIUM).await;
    assert_policy_unavailable(rig.preflight(&chat, PREMIUM, plain_facts()).await);
    assert!(rig.metrics.calls().is_empty());
    assert!(all_quota_rows(&rig.db).await.is_empty());
}

#[tokio::test]
async fn a_user_limits_failure_is_policy_unavailable() {
    let rig = quota_rig().await;
    rig.gateway.fail_limits.store(true, Ordering::SeqCst);
    let chat = rig.chat_a1(PREMIUM).await;
    assert_policy_unavailable(rig.preflight(&chat, PREMIUM, plain_facts()).await);
    assert!(rig.metrics.calls().is_empty());
}

#[tokio::test]
async fn the_license_check_is_never_part_of_the_preflight() {
    // the gateway port has no license operation: the preflight asks for the snapshot and the
    // limits only
    let rig = quota_rig().await;
    let chat = rig.chat_a1(PREMIUM).await;
    rig.preflight(&chat, PREMIUM, plain_facts()).await.unwrap();
    assert_eq!(rig.gateway.version_call_count(), 0);
    assert_eq!(rig.gateway.total_calls(), 2);
}

// ── the web search kill switch ───────────────────────────────────────────────

fn web_killed_gateway() -> Arc<QuotaGateway> {
    let mut catalog = design_catalog();
    catalog.kill_switches = KillSwitches {
        disable_web_search: true,
        ..KillSwitches::default()
    };
    QuotaGateway::serving(
        catalog,
        (60_000_000, 600_000_000),
        (22_000_000, 300_000_000),
    )
}

#[tokio::test]
async fn web_search_under_the_kill_switch_is_rejected_as_feature_disabled_before_estimation() {
    let rig = quota_rig_with(QuotaRigOptions {
        gateway: web_killed_gateway(),
        ..QuotaRigOptions::new()
    })
    .await;
    let chat = rig.chat_a1(PREMIUM).await;
    let facts = RequestFacts {
        web_search_enabled: true,
        ..plain_facts()
    };
    let error = rig.preflight(&chat, PREMIUM, facts).await.unwrap_err();
    assert!(
        matches!(
            error,
            DomainError::FeatureDisabled {
                subject: "web_search"
            }
        ),
        "{error:?}"
    );
    assert!(
        !rig.metrics
            .calls()
            .iter()
            .any(|call| matches!(call, MetricCall::EstimatedTokens(_))),
        "no estimation ran: {:?}",
        rig.metrics.calls()
    );
    assert!(all_quota_rows(&rig.db).await.is_empty());
}

#[tokio::test]
async fn the_kill_switch_does_not_reject_a_request_without_web_search() {
    let rig = quota_rig_with(QuotaRigOptions {
        gateway: web_killed_gateway(),
        ..QuotaRigOptions::new()
    })
    .await;
    let chat = rig.chat_a1(PREMIUM).await;
    let decision = rig.preflight(&chat, PREMIUM, plain_facts()).await.unwrap();
    assert_eq!(decision.effective_model, PREMIUM);
}

// ── the decision ─────────────────────────────────────────────────────────────

#[tokio::test]
async fn the_design_example_is_downgraded_to_standard_with_the_standard_reserve() {
    let rig = quota_rig().await;
    seed_design_consumption(&rig.db).await;
    let chat = rig.chat_a1(PREMIUM).await;
    let decision = rig.preflight(&chat, PREMIUM, plain_facts()).await.unwrap();

    assert_eq!(decision.selected_model, PREMIUM);
    assert_eq!(decision.effective_model, STANDARD);
    assert_eq!(decision.tier, ModelTier::Standard);
    assert_eq!(decision.quota_decision, QuotaDecision::Downgrade);
    assert_eq!(decision.downgrade_from.as_deref(), Some(PREMIUM));
    assert_eq!(
        decision.downgrade_reason,
        Some(DowngradeReason::PremiumQuotaExhausted)
    );
    assert_eq!(decision.max_output_tokens_applied, 500);
    assert_eq!(decision.reserve_tokens, 1500);
    assert_eq!(decision.reserved_credits_micro, 1_500_000);
    assert_eq!(decision.minimal_generation_floor_applied, 50);
    assert_eq!(decision.policy_version_applied, 1);
    assert!(
        !decision.vision_input,
        "the standard model has no image input"
    );
    // a standard turn re-checks the total bucket only
    assert_eq!(decision.limits.len(), 2);
    assert!(decision.limits.iter().all(|l| l.bucket == Bucket::Total));
}

#[tokio::test]
async fn the_metrics_of_a_downgrade_are_recorded_at_preflight_before_any_reserve() {
    let rig = quota_rig().await;
    seed_design_consumption(&rig.db).await;
    let chat = rig.chat_a1(PREMIUM).await;
    rig.preflight(&chat, PREMIUM, plain_facts()).await.unwrap();
    assert_eq!(
        rig.metrics.calls(),
        vec![
            MetricCall::Preflight {
                decision: "downgrade".to_owned(),
                model: STANDARD.to_owned(),
                tier: "standard".to_owned()
            },
            MetricCall::EstimatedTokens(1500),
        ]
    );
}

#[tokio::test]
async fn an_allowed_premium_decision_checks_both_buckets_and_records_an_allow() {
    let rig = quota_rig().await;
    let chat = rig.chat_a1(PREMIUM).await;
    let decision = rig.preflight(&chat, PREMIUM, plain_facts()).await.unwrap();
    assert_eq!(decision.effective_model, PREMIUM);
    assert_eq!(decision.tier, ModelTier::Premium);
    assert_eq!(decision.quota_decision, QuotaDecision::Allow);
    assert_eq!(decision.downgrade_from, None);
    assert_eq!(decision.downgrade_reason, None);
    assert_eq!(decision.reserved_credits_micro, 3_750_000);
    assert!(decision.vision_input);
    assert_eq!(decision.provider_id, "azure_openai");
    assert_eq!(decision.limits.len(), 4);
    assert_eq!(
        rig.metrics.calls(),
        vec![
            MetricCall::Preflight {
                decision: "allow".to_owned(),
                model: PREMIUM.to_owned(),
                tier: "premium".to_owned()
            },
            MetricCall::EstimatedTokens(1500),
        ]
    );
}

#[tokio::test]
async fn a_kill_switch_downgrade_names_its_reason() {
    let mut catalog = design_catalog();
    catalog.kill_switches = KillSwitches {
        force_standard_tier: true,
        ..KillSwitches::default()
    };
    let rig = quota_rig_with(QuotaRigOptions {
        gateway: QuotaGateway::serving(
            catalog,
            (60_000_000, 600_000_000),
            (22_000_000, 300_000_000),
        ),
        ..QuotaRigOptions::new()
    })
    .await;
    let chat = rig.chat_a1(PREMIUM).await;
    let decision = rig.preflight(&chat, PREMIUM, plain_facts()).await.unwrap();
    assert_eq!(
        decision.downgrade_reason,
        Some(DowngradeReason::ForceStandardTier)
    );
    assert_eq!(decision.effective_model, STANDARD);
}

#[tokio::test]
async fn an_exhausted_quota_is_a_tokens_rejection_that_records_a_reject() {
    let rig = quota_rig().await;
    seed_rows(&rig.db, &[row("daily", DAY, "total").spent(59_500_000)]).await;
    let chat = rig.chat_a1(PREMIUM).await;
    let error = rig
        .preflight(&chat, PREMIUM, plain_facts())
        .await
        .unwrap_err();
    assert!(
        matches!(
            error,
            DomainError::QuotaExceeded {
                scope: QuotaScope::Tokens
            }
        ),
        "{error:?}"
    );
    let calls = rig.metrics.calls();
    assert!(
        calls.iter().any(|call| matches!(
            call,
            MetricCall::Preflight { decision, model, .. } if decision == "reject" && model == PREMIUM
        )),
        "{calls:?}"
    );
    assert!(
        !calls
            .iter()
            .any(|call| matches!(call, MetricCall::EstimatedTokens(_))),
        "estimated tokens are recorded after an allow or downgrade only: {calls:?}"
    );
}

#[tokio::test]
async fn a_catalog_without_an_enabled_model_is_a_tokens_rejection() {
    let mut premium = premium_model();
    premium.enabled = false;
    let mut standard = standard_model();
    standard.enabled = false;
    let rig = quota_rig_with(QuotaRigOptions {
        gateway: QuotaGateway::serving(
            catalog_v(1, vec![premium, standard]),
            (60_000_000, 600_000_000),
            (22_000_000, 300_000_000),
        ),
        ..QuotaRigOptions::new()
    })
    .await;
    let chat = rig.chat_a1(PREMIUM).await;
    let error = rig
        .preflight(&chat, PREMIUM, plain_facts())
        .await
        .unwrap_err();
    assert!(
        matches!(
            error,
            DomainError::QuotaExceeded {
                scope: QuotaScope::Tokens
            }
        ),
        "{error:?}"
    );
}

#[tokio::test]
async fn an_uncomputable_reserve_on_every_candidate_is_a_tokens_rejection_not_a_failure() {
    let mut premium = premium_model();
    premium.input_tokens_credit_multiplier_micro = 0;
    let mut standard = standard_model();
    standard.output_tokens_credit_multiplier_micro = 10_000_000_001;
    let rig = quota_rig_with(QuotaRigOptions {
        gateway: QuotaGateway::serving(
            catalog_v(1, vec![premium, standard]),
            (60_000_000, 600_000_000),
            (22_000_000, 300_000_000),
        ),
        ..QuotaRigOptions::new()
    })
    .await;
    let chat = rig.chat_a1(PREMIUM).await;
    let error = rig
        .preflight(&chat, PREMIUM, plain_facts())
        .await
        .unwrap_err();
    assert!(
        matches!(
            error,
            DomainError::QuotaExceeded {
                scope: QuotaScope::Tokens
            }
        ),
        "{error:?}"
    );
}

#[tokio::test]
async fn a_failed_premium_candidate_on_buckets_with_spent_credits_downgrades_without_wrapping() {
    let mut premium = premium_model();
    premium.input_tokens_credit_multiplier_micro = 0;
    let rig = quota_rig_with(QuotaRigOptions {
        gateway: QuotaGateway::serving(
            catalog_v(1, vec![premium, standard_model()]),
            (60_000_000, 600_000_000),
            (22_000_000, 300_000_000),
        ),
        ..QuotaRigOptions::new()
    })
    .await;
    seed_design_consumption(&rig.db).await;
    let chat = rig.chat_a1(PREMIUM).await;
    let decision = rig.preflight(&chat, PREMIUM, plain_facts()).await.unwrap();
    assert_eq!(decision.effective_model, STANDARD);
    assert_eq!(decision.reserved_credits_micro, 1_500_000);
}

#[tokio::test]
async fn the_preflight_of_another_users_rows_is_isolated() {
    // the rows of user 2 and of tenant B do not count against user 1
    let rig = quota_rig().await;
    seed_rows(
        &rig.db,
        &[
            row("daily", DAY, "total")
                .spent(59_999_999)
                .of(crate::test_support::TENANT_A, crate::test_support::USER_A2),
            row("daily", DAY, "total")
                .spent(59_999_999)
                .of(crate::test_support::TENANT_B, USER_A1),
        ],
    )
    .await;
    let chat = rig.chat_a1(PREMIUM).await;
    let decision = rig.preflight(&chat, PREMIUM, plain_facts()).await.unwrap();
    assert_eq!(decision.effective_model, PREMIUM);
}

// ── the single preflight instant ─────────────────────────────────────────────

#[tokio::test]
async fn the_clock_is_read_exactly_once_per_preflight() {
    let clock = FixedClock::at(default_now());
    let rig = quota_rig_with(QuotaRigOptions {
        clock: clock.clone(),
        ..QuotaRigOptions::new()
    })
    .await;
    let chat = rig.chat_a1(PREMIUM).await;
    rig.preflight(&chat, PREMIUM, plain_facts()).await.unwrap();
    assert_eq!(clock.reads(), 1);
    rig.preflight(&chat, PREMIUM, plain_facts()).await.unwrap();
    assert_eq!(clock.reads(), 2);
}

#[tokio::test]
async fn the_period_starts_and_started_at_come_from_one_instant() {
    // a clock that would give another instant on a second read
    let clock = TickingClock::from(utc(2026, 2, 28, 23, 59, 59));
    let rig = quota_rig_with(QuotaRigOptions {
        clock,
        ..QuotaRigOptions::new()
    })
    .await;
    let chat = rig.chat_a1(PREMIUM).await;
    let decision = rig.preflight(&chat, PREMIUM, plain_facts()).await.unwrap();
    assert_eq!(decision.started_at, utc(2026, 2, 28, 23, 59, 59));
    assert_eq!(
        decision.period_starts,
        PeriodStarts::of(decision.started_at)
    );
    assert_eq!(decision.period_starts.daily, date(2026, 2, 28));
    assert_eq!(decision.period_starts.monthly, date(2026, 2, 1));
}

#[tokio::test]
async fn the_rows_are_read_for_the_periods_of_the_preflight_instant() {
    // consumption on day 1 does not exhaust the quota of day 2
    let clock = FixedClock::at(utc(2026, 3, 1, 0, 0, 1));
    let rig = quota_rig_with(QuotaRigOptions {
        clock,
        ..QuotaRigOptions::new()
    })
    .await;
    seed_rows(&rig.db, &[row("daily", DAY, "total").spent(59_999_999)]).await;
    let chat = rig.chat_a1(PREMIUM).await;
    let decision = rig.preflight(&chat, PREMIUM, plain_facts()).await.unwrap();
    assert_eq!(decision.effective_model, PREMIUM);
    assert_eq!(decision.period_starts.daily, date(2026, 3, 1));
}

// ── preflight writes nothing ─────────────────────────────────────────────────

#[tokio::test]
async fn preflight_writes_no_row_for_an_allow_a_downgrade_or_a_reject() {
    let rig = quota_rig().await;
    seed_design_consumption(&rig.db).await;
    let chat = rig.chat_a1(PREMIUM).await;
    let before = all_quota_rows(&rig.db).await;
    assert_eq!(before.len(), 4);

    rig.preflight(&chat, STANDARD, plain_facts()).await.unwrap(); // allow
    rig.preflight(&chat, PREMIUM, plain_facts()).await.unwrap(); // downgrade
    assert_eq!(all_quota_rows(&rig.db).await, before);

    rig.gateway.set_limits((1, 1), (1, 1));
    assert!(rig.preflight(&chat, PREMIUM, plain_facts()).await.is_err()); // reject
    assert_eq!(all_quota_rows(&rig.db).await, before);

    assert_eq!(outbox_body_count(&rig.db).await, 0);
    assert_eq!(
        crate::test_support::raw_i64(&rig.db.raw, "SELECT COUNT(*) FROM chat_turns").await,
        0
    );
}

// ── daily tool quotas ────────────────────────────────────────────────────────

fn web_facts() -> RequestFacts {
    RequestFacts {
        web_search_enabled: true,
        ..plain_facts()
    }
}

fn xlsx_facts() -> RequestFacts {
    RequestFacts {
        ready_xlsx_attachments: 1,
        ..plain_facts()
    }
}

#[tokio::test]
async fn a_web_search_request_at_the_daily_quota_is_a_web_search_rejection() {
    let rig = quota_rig().await;
    seed_rows(&rig.db, &[row("daily", DAY, "total").web(75)]).await;
    let chat = rig.chat_a1(PREMIUM).await;
    let error = rig
        .preflight(&chat, PREMIUM, web_facts())
        .await
        .unwrap_err();
    assert!(
        matches!(
            error,
            DomainError::QuotaExceeded {
                scope: QuotaScope::WebSearch
            }
        ),
        "{error:?}"
    );
}

#[tokio::test]
async fn a_request_without_web_search_is_admitted_at_the_same_count() {
    let rig = quota_rig().await;
    seed_rows(&rig.db, &[row("daily", DAY, "total").web(75).code(50)]).await;
    let chat = rig.chat_a1(PREMIUM).await;
    assert!(rig.preflight(&chat, PREMIUM, plain_facts()).await.is_ok());
}

#[tokio::test]
async fn a_code_interpreter_request_at_the_daily_quota_is_a_code_interpreter_rejection() {
    let rig = quota_rig().await;
    seed_rows(&rig.db, &[row("daily", DAY, "total").code(50)]).await;
    let chat = rig.chat_a1(PREMIUM).await;
    let error = rig
        .preflight(&chat, PREMIUM, xlsx_facts())
        .await
        .unwrap_err();
    assert!(
        matches!(
            error,
            DomainError::QuotaExceeded {
                scope: QuotaScope::CodeInterpreter
            }
        ),
        "{error:?}"
    );
}

#[tokio::test]
async fn the_counters_are_read_from_the_daily_total_row_only() {
    let rig = quota_rig().await;
    seed_rows(
        &rig.db,
        &[
            row("monthly", MONTH, "total").web(500).code(500),
            row("daily", DAY, "tier:premium").web(500).code(500),
            row("daily", DAY, "total").web(74).code(49),
        ],
    )
    .await;
    let chat = rig.chat_a1(PREMIUM).await;
    let facts = RequestFacts {
        ready_xlsx_attachments: 1,
        web_search_enabled: true,
        ..plain_facts()
    };
    assert!(rig.preflight(&chat, PREMIUM, facts).await.is_ok());
}

#[tokio::test]
async fn a_tool_quota_rejection_still_records_the_preflight_and_the_estimate() {
    let rig = quota_rig().await;
    seed_rows(&rig.db, &[row("daily", DAY, "total").web(75)]).await;
    let chat = rig.chat_a1(PREMIUM).await;
    assert!(rig.preflight(&chat, PREMIUM, web_facts()).await.is_err());
    let calls = rig.metrics.calls();
    assert!(
        calls.iter().any(|call| matches!(
            call,
            MetricCall::Preflight { decision, .. } if decision == "allow"
        )),
        "{calls:?}"
    );
    assert!(
        calls
            .iter()
            .any(|call| matches!(call, MetricCall::EstimatedTokens(_))),
        "{calls:?}"
    );
    assert!(
        !calls
            .iter()
            .any(|call| matches!(call, MetricCall::Reserve(_)))
    );
}

#[tokio::test]
async fn the_tool_quota_is_checked_for_the_effective_model_after_the_cascade() {
    // the standard model has no web search: after the downgrade the quota is not checked
    let mut standard = standard_model();
    standard.general_config.tool_support.web_search = false;
    let rig = quota_rig_with(QuotaRigOptions {
        gateway: QuotaGateway::serving(
            catalog_v(1, vec![premium_model(), standard]),
            (60_000_000, 600_000_000),
            (22_000_000, 300_000_000),
        ),
        ..QuotaRigOptions::new()
    })
    .await;
    seed_rows(
        &rig.db,
        &[
            row("daily", DAY, "tier:premium").spent(20_000_000),
            row("monthly", MONTH, "tier:premium").spent(200_000_000),
            row("daily", DAY, "total").spent(25_000_000).web(75),
            row("monthly", MONTH, "total").spent(240_000_000),
        ],
    )
    .await;
    let chat = rig.chat_a1(PREMIUM).await;
    let decision = rig.preflight(&chat, PREMIUM, web_facts()).await.unwrap();
    assert_eq!(decision.effective_model, STANDARD);
}

#[tokio::test]
async fn the_configured_tool_quotas_are_used() {
    let mut settings = default_settings();
    settings.web_search_daily_quota = 5;
    settings.code_interpreter_daily_quota = 3;
    let rig = quota_rig_with(QuotaRigOptions {
        settings,
        ..QuotaRigOptions::new()
    })
    .await;
    seed_rows(&rig.db, &[row("daily", DAY, "total").web(5).code(3)]).await;
    let chat = rig.chat_a1(PREMIUM).await;
    assert!(matches!(
        rig.preflight(&chat, PREMIUM, web_facts()).await,
        Err(DomainError::QuotaExceeded {
            scope: QuotaScope::WebSearch
        })
    ));
    assert!(matches!(
        rig.preflight(&chat, PREMIUM, xlsx_facts()).await,
        Err(DomainError::QuotaExceeded {
            scope: QuotaScope::CodeInterpreter
        })
    ));
}

// ── prior context ────────────────────────────────────────────────────────────

/// The reserve tokens of a standard turn: 1000 estimated text tokens, 500 output, plus the
/// prior context.
async fn reserve_with_prior(
    rig: &crate::test_support::quota::QuotaRig,
    chat: &crate::test_support::quota::ChatRecord,
) -> i64 {
    rig.preflight(chat, STANDARD, plain_facts())
        .await
        .unwrap()
        .reserve_tokens
}

#[tokio::test]
async fn a_chat_without_an_assistant_message_has_no_prior_context() {
    let rig = quota_rig().await;
    let chat = rig.chat_a1(STANDARD).await;
    assert_eq!(reserve_with_prior(&rig, &chat).await, 1500);
    seed_message_with_tokens(&rig.db, &chat, "user", at(1), 900, 900, false).await;
    assert_eq!(
        reserve_with_prior(&rig, &chat).await,
        1500,
        "a user message is not prior context"
    );
}

#[tokio::test]
async fn the_prior_context_is_input_plus_output_tokens_of_the_latest_assistant_message() {
    let rig = quota_rig().await;
    let chat = rig.chat_a1(STANDARD).await;
    seed_message_with_tokens(&rig.db, &chat, "assistant", at(1), 111, 11, false).await;
    seed_message_with_tokens(&rig.db, &chat, "assistant", at(2), 300, 200, false).await;
    assert_eq!(reserve_with_prior(&rig, &chat).await, 2000);
}

#[tokio::test]
async fn an_assistant_message_without_tokens_or_soft_deleted_is_skipped() {
    let rig = quota_rig().await;
    let chat = rig.chat_a1(STANDARD).await;
    seed_message_with_tokens(&rig.db, &chat, "assistant", at(1), 300, 200, false).await;
    seed_message_with_tokens(&rig.db, &chat, "assistant", at(2), 0, 0, false).await;
    seed_message_with_tokens(&rig.db, &chat, "assistant", at(3), 999, 1, true).await;
    assert_eq!(reserve_with_prior(&rig, &chat).await, 2000);
}

#[tokio::test]
async fn the_latest_message_is_chronological_across_sub_second_text_forms() {
    let rig = quota_rig().await;
    let chat = rig.chat_a1(STANDARD).await;
    // 12:00:00.123 is newer than 12:00:00.12 although the stored text sorts the other way
    seed_message_with_tokens(
        &rig.db,
        &chat,
        "assistant",
        utc_ms(2026, 1, 1, 12, 0, 0, 123),
        100,
        0,
        false,
    )
    .await;
    seed_message_with_tokens(
        &rig.db,
        &chat,
        "assistant",
        utc_ms(2026, 1, 1, 12, 0, 0, 120),
        700,
        0,
        false,
    )
    .await;
    assert_eq!(reserve_with_prior(&rig, &chat).await, 1600);
}

#[tokio::test]
async fn a_whole_second_message_is_older_than_a_fractional_one_of_the_same_second() {
    let rig = quota_rig().await;
    let chat = rig.chat_a1(STANDARD).await;
    seed_message_with_tokens(
        &rig.db,
        &chat,
        "assistant",
        utc_ms(2026, 1, 1, 12, 0, 0, 0),
        900,
        0,
        false,
    )
    .await;
    seed_message_with_tokens(
        &rig.db,
        &chat,
        "assistant",
        utc_ms(2026, 1, 1, 12, 0, 0, 500),
        50,
        0,
        false,
    )
    .await;
    assert_eq!(reserve_with_prior(&rig, &chat).await, 1550);
}

#[tokio::test]
async fn equal_timestamps_are_broken_by_the_greater_message_id() {
    let rig = quota_rig().await;
    let chat = rig.chat_a1(STANDARD).await;
    let when = utc_ms(2026, 1, 1, 12, 0, 0, 500);
    let first = seed_message_with_tokens(&rig.db, &chat, "assistant", when, 100, 0, false).await;
    let second = seed_message_with_tokens(&rig.db, &chat, "assistant", when, 400, 0, false).await;
    let winner: Uuid = first.max(second);
    let expected = if winner == first { 100 } else { 400 };
    assert_eq!(reserve_with_prior(&rig, &chat).await, 1500 + expected);
}

#[tokio::test]
async fn the_prior_context_of_another_chat_is_not_read() {
    let rig = quota_rig().await;
    let chat = rig.chat_a1(STANDARD).await;
    let other = rig.chat_a1(STANDARD).await;
    seed_message_with_tokens(&rig.db, &other, "assistant", at(1), 300, 200, false).await;
    assert_eq!(reserve_with_prior(&rig, &chat).await, 1500);
}

// ── the input limit check ────────────────────────────────────────────────────

#[tokio::test]
async fn the_decision_supports_the_input_limit_check_of_the_pipeline() {
    let mut limited = standard_model();
    limited.max_input_tokens = 100;
    let mut unlimited = premium_model();
    unlimited.max_input_tokens = 0;
    let rig = quota_rig_with(QuotaRigOptions {
        gateway: QuotaGateway::serving(
            catalog_v(1, vec![unlimited, limited]),
            (60_000_000, 600_000_000),
            (22_000_000, 300_000_000),
        ),
        ..QuotaRigOptions::new()
    })
    .await;
    let chat = rig.chat_a1(STANDARD).await;
    let decision = rig.preflight(&chat, STANDARD, plain_facts()).await.unwrap();
    assert_eq!(decision.estimated_text_tokens, 1000);
    assert_eq!(decision.max_input_tokens, 100);
    assert!(decision.input_too_long());

    let decision = rig.preflight(&chat, PREMIUM, plain_facts()).await.unwrap();
    assert_eq!(decision.max_input_tokens, 0);
    assert!(!decision.input_too_long(), "0 means no separate limit");
}
