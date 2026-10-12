#![allow(clippy::unwrap_used, clippy::expect_used)]

use mini_chat_sdk::{KillSwitches, ModelCatalogEntry, PolicySnapshot, TierLimits, UserLimits};
use serde_json::json;
use time::macros::datetime;
use uuid::Uuid;

use super::{
    CascadeInputs, DowngradeReason, quota_breakdown, quota_warnings, resolve_effective_model,
};
use crate::config::QuotaConfig;
use crate::domain::error::{DomainError, QuotaScope};
use crate::domain::estimation::{ReserveInputs, ToolFacts};
use crate::domain::quota::model::{Bucket, BucketUsage, PeriodType, QuotaPeriod, QuotaSnapshot};

fn entry(id: &str, tier: &str, enabled: bool, default: bool) -> ModelCatalogEntry {
    serde_json::from_value(json!({
        "id": id, "provider_model_id": format!("p-{id}"), "display_name": id, "provider_id": "p",
        "provider_display_name": "P", "tier": tier, "enabled": enabled,
        "context_window": 128_000, "max_output_tokens": 4096, "max_input_tokens": 0,
        "input_tokens_credit_multiplier_micro": 1_000_000,
        "output_tokens_credit_multiplier_micro": 3_000_000,
        "max_num_results": 5,
        "general_config": {
            "type": "", "available_from": "1970-01-01T00:00:00Z", "max_file_size_mb": 25,
            "api_params": { "stop": [] },
            "features": { "streaming": true, "structured_output": true },
            "tool_support": { "web_search": true, "file_search": true,
                "image_generation": false, "code_interpreter": true, "mcp": false },
            "supported_endpoints": {}
        },
        "preference": { "is_default": default, "sort_order": 0 }
    }))
    .unwrap()
}

fn snapshot(kill: KillSwitches) -> PolicySnapshot {
    PolicySnapshot {
        policy_version: 1,
        model_catalog: vec![
            entry("premium", "premium", true, true),
            entry("standard", "standard", true, false),
        ],
        kill_switches: kill,
    }
}

fn limits(total: i64, premium: i64) -> UserLimits {
    UserLimits {
        user_id: Uuid::nil(),
        policy_version: 1,
        standard: TierLimits {
            limit_daily_credits_micro: total,
            limit_monthly_credits_micro: total,
        },
        premium: TierLimits {
            limit_daily_credits_micro: premium,
            limit_monthly_credits_micro: premium,
        },
    }
}

fn run(
    snap: &PolicySnapshot,
    user_limits: &UserLimits,
    usage: &QuotaSnapshot,
    selected: &str,
    facts: ToolFacts,
) -> Result<super::PreflightDecision, DomainError> {
    let quota = QuotaConfig::default();
    resolve_effective_model(&CascadeInputs {
        snapshot: snap,
        limits: user_limits,
        usage,
        selected_model: selected,
        reserve_inputs: ReserveInputs {
            content_bytes: 5,
            prior_context_tokens: 0,
            image_count: 0,
            streaming_max_output_tokens: 32_768,
        },
        tool_facts: facts,
        periods: QuotaPeriod::containing(datetime!(2026-02-28 15:30 UTC)),
        tool_quotas: &quota,
    })
}

const RESERVE: i64 = 12_401;

#[test]
fn an_available_premium_tier_is_used_without_a_downgrade() {
    let decision = run(
        &snapshot(KillSwitches::default()),
        &limits(1_000_000, 1_000_000),
        &QuotaSnapshot::default(),
        "premium",
        ToolFacts::default(),
    )
    .unwrap();

    assert_eq!(decision.effective.id, "premium");
    assert_eq!(decision.decision(), "allow");
    assert_eq!(decision.reserve.reserved_credits_micro, RESERVE);
    assert_eq!(decision.plan.limits.len(), 4);
    assert_eq!(decision.policy_version, 1);
}

#[test]
fn an_exact_fit_is_available_and_one_micro_credit_less_is_not() {
    let fits = run(
        &snapshot(KillSwitches::default()),
        &limits(RESERVE, RESERVE),
        &QuotaSnapshot::default(),
        "premium",
        ToolFacts::default(),
    );
    let short = run(
        &snapshot(KillSwitches::default()),
        &limits(RESERVE - 1, RESERVE - 1),
        &QuotaSnapshot::default(),
        "premium",
        ToolFacts::default(),
    );

    assert_eq!(fits.unwrap().effective.id, "premium");
    assert!(matches!(
        short,
        Err(DomainError::QuotaExceeded {
            scope: QuotaScope::Tokens
        })
    ));
}

#[test]
fn an_exhausted_premium_bucket_downgrades_to_standard() {
    let decision = run(
        &snapshot(KillSwitches::default()),
        &limits(1_000_000, 1000),
        &QuotaSnapshot::default(),
        "premium",
        ToolFacts::default(),
    )
    .unwrap();

    assert_eq!(decision.effective.id, "standard");
    assert_eq!(
        decision.downgrade_reason,
        Some(DowngradeReason::PremiumQuotaExhausted)
    );
    assert_eq!(
        decision.plan.limits.len(),
        2,
        "standard books the total bucket only"
    );
}

#[test]
fn in_flight_reserves_count_against_the_limit() {
    let usage = QuotaSnapshot::from_rows([(
        (PeriodType::Daily, Bucket::Total),
        BucketUsage {
            reserved_credits_micro: 10_000,
            ..BucketUsage::default()
        },
    )]);

    let result = run(
        &snapshot(KillSwitches::default()),
        &limits(20_000, 20_000),
        &usage,
        "standard",
        ToolFacts::default(),
    );

    assert!(matches!(result, Err(DomainError::QuotaExceeded { .. })));
}

#[test]
fn a_standard_chat_is_never_upgraded() {
    let result = run(
        &snapshot(KillSwitches::default()),
        &limits(1000, 1_000_000_000),
        &QuotaSnapshot::default(),
        "standard",
        ToolFacts::default(),
    );

    assert!(matches!(result, Err(DomainError::QuotaExceeded { .. })));
}

#[test]
fn kill_switches_skip_the_premium_tier_with_their_reason() {
    for (kill, reason) in [
        (
            KillSwitches {
                force_standard_tier: true,
                ..KillSwitches::default()
            },
            DowngradeReason::ForceStandardTier,
        ),
        (
            KillSwitches {
                disable_premium_tier: true,
                ..KillSwitches::default()
            },
            DowngradeReason::DisablePremiumTier,
        ),
    ] {
        let decision = run(
            &snapshot(kill),
            &limits(1_000_000, 1_000_000),
            &QuotaSnapshot::default(),
            "premium",
            ToolFacts::default(),
        )
        .unwrap();

        assert_eq!(decision.effective.id, "standard");
        assert_eq!(decision.downgrade_reason, Some(reason));
    }
}

#[test]
fn a_disabled_selected_model_is_downgraded_with_model_disabled() {
    let mut snap = snapshot(KillSwitches::default());
    snap.model_catalog[0].enabled = false;
    snap.model_catalog
        .push(entry("premium-two", "premium", true, false));

    let decision = run(
        &snap,
        &limits(1_000_000, 1_000_000),
        &QuotaSnapshot::default(),
        "premium",
        ToolFacts::default(),
    )
    .unwrap();

    assert_eq!(decision.effective.id, "premium-two");
    assert_eq!(
        decision.downgrade_reason,
        Some(DowngradeReason::ModelDisabled)
    );
}

#[test]
fn the_daily_web_search_quota_rejects_only_requests_that_use_the_tool() {
    let usage = QuotaSnapshot::from_rows([(
        (PeriodType::Daily, Bucket::Total),
        BucketUsage {
            web_search_calls: 75,
            ..BucketUsage::default()
        },
    )]);
    let snap = snapshot(KillSwitches::default());
    let with_tool = ToolFacts {
        web_search_requested: true,
        ..ToolFacts::default()
    };

    let rejected = run(
        &snap,
        &limits(1_000_000, 1_000_000),
        &usage,
        "premium",
        with_tool,
    );
    let plain = run(
        &snap,
        &limits(1_000_000, 1_000_000),
        &usage,
        "premium",
        ToolFacts::default(),
    );

    assert!(matches!(
        rejected,
        Err(DomainError::QuotaExceeded {
            scope: QuotaScope::WebSearch
        })
    ));
    assert!(plain.is_ok());
}

#[test]
fn warnings_report_the_remaining_percentage_per_tier_and_period() {
    let periods = QuotaPeriod::containing(datetime!(2026-02-28 15:30 UTC));
    let usage = QuotaSnapshot::from_rows([(
        (PeriodType::Daily, Bucket::Total),
        BucketUsage {
            spent_credits_micro: 850,
            ..BucketUsage::default()
        },
    )]);

    let warnings = quota_warnings(&limits(1000, 0), &usage, &periods, 80);

    // the premium limit is 0: skipped
    assert_eq!(warnings.len(), 2);
    let daily = warnings.iter().find(|w| w.period == "daily").unwrap();
    assert_eq!(daily.tier, "total");
    assert_eq!(daily.remaining_percentage, 15);
    assert!(daily.warning && !daily.exhausted);
    assert_eq!(daily.next_reset, Some(datetime!(2026-03-01 00:00 UTC)));
    let monthly = warnings.iter().find(|w| w.period == "monthly").unwrap();
    assert_eq!(monthly.remaining_percentage, 100);
    assert!(monthly.next_reset.is_none());
}

#[test]
fn the_next_monthly_reset_rolls_over_the_year() {
    let periods = QuotaPeriod::containing(datetime!(2026-12-15 10:00 UTC));

    assert_eq!(periods[1].next_reset(), datetime!(2027-01-01 00:00 UTC));
}

#[test]
fn the_breakdown_reports_credits_for_every_period_with_a_positive_limit() {
    let periods = QuotaPeriod::containing(datetime!(2026-02-28 15:30 UTC));
    let usage = QuotaSnapshot::from_rows([(
        (PeriodType::Daily, Bucket::Total),
        BucketUsage {
            spent_credits_micro: 700,
            reserved_credits_micro: 400,
            ..BucketUsage::default()
        },
    )]);

    let breakdown = quota_breakdown(&limits(1000, 0), &usage, &periods, 80);

    // the premium limit is 0: no entries; the total bucket has a daily and a monthly one
    assert_eq!(breakdown.len(), 2);
    let daily = &breakdown[0];
    assert_eq!((daily.tier, daily.period), ("total", "daily"));
    assert_eq!(daily.limit_credits_micro, 1000);
    assert_eq!(daily.used_credits_micro, 1100, "spent plus reserved");
    assert_eq!(daily.remaining_credits_micro, 0, "never negative");
    assert_eq!(daily.remaining_percentage, 0);
    assert!(daily.warning && daily.exhausted);
    assert_eq!(daily.next_reset, datetime!(2026-03-01 00:00 UTC));
    let monthly = &breakdown[1];
    assert_eq!((monthly.tier, monthly.period), ("total", "monthly"));
    assert_eq!(monthly.remaining_percentage, 100);
    assert!(!monthly.warning && !monthly.exhausted);
    assert_eq!(
        monthly.next_reset,
        datetime!(2026-03-01 00:00 UTC),
        "the breakdown always carries the reset, the warnings only for warning entries"
    );
}

#[test]
fn the_breakdown_lists_premium_before_total() {
    let periods = QuotaPeriod::containing(datetime!(2026-02-28 15:30 UTC));

    let breakdown = quota_breakdown(&limits(1000, 500), &QuotaSnapshot::default(), &periods, 80);

    let order: Vec<(&str, &str)> = breakdown
        .iter()
        .map(|entry| (entry.tier, entry.period))
        .collect();
    assert_eq!(
        order,
        [
            ("premium", "daily"),
            ("premium", "monthly"),
            ("total", "daily"),
            ("total", "monthly")
        ]
    );
}
