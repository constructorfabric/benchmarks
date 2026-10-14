//! DTO tests of the quota status endpoint: the wire shape of the response, the tier and period
//! enumerations, the integer widths and the projection from the domain status.
//!
//! Slice S1-all: algorithm `cpt-cf-mini-chat-algo-quota-and-billing-status-computation`
//! (projection), definition of done `cpt-cf-mini-chat-dod-quota-and-billing-status-api`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeSet;

use serde_json::{Value, json};

use super::dto::{
    QuotaPeriodDto, QuotaPeriodStatusDto, QuotaStatusResponseDto, QuotaTierDto, QuotaTierStatusDto,
};
use crate::domain::quota::{PeriodStatus, PeriodType, QuotaStatus, QuotaTier, TierStatus};
use crate::test_support::quota::utc;

fn keys(value: &Value) -> BTreeSet<&str> {
    value
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect()
}

fn status() -> QuotaStatus {
    QuotaStatus {
        tiers: vec![
            TierStatus {
                tier: QuotaTier::Premium,
                periods: vec![PeriodStatus {
                    period: PeriodType::Daily,
                    limit_credits_micro: 22_000_000,
                    used_credits_micro: 20_000_000,
                    remaining_credits_micro: 2_000_000,
                    remaining_percentage: 9,
                    next_reset: utc(2026, 3, 1, 0, 0, 0),
                    warning: true,
                    exhausted: false,
                }],
            },
            TierStatus {
                tier: QuotaTier::Total,
                periods: vec![PeriodStatus {
                    period: PeriodType::Monthly,
                    limit_credits_micro: 600_000_000,
                    used_credits_micro: 241_200_000,
                    remaining_credits_micro: 358_800_000,
                    remaining_percentage: 59,
                    next_reset: utc(2026, 3, 1, 0, 0, 0),
                    warning: false,
                    exhausted: false,
                }],
            },
        ],
        warning_threshold_pct: 80,
    }
}

#[test]
fn the_response_projects_the_domain_status_field_by_field() {
    let dto = QuotaStatusResponseDto::from(status());
    let value = serde_json::to_value(&dto).unwrap();
    assert_eq!(
        keys(&value),
        BTreeSet::from(["tiers", "warning_threshold_pct"])
    );
    assert_eq!(value["warning_threshold_pct"], 80);
    assert_eq!(value["tiers"][0]["tier"], "premium");
    assert_eq!(value["tiers"][1]["tier"], "total");
    let period = &value["tiers"][0]["periods"][0];
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
    assert_eq!(period["period"], "daily");
    assert_eq!(period["limit_credits_micro"], 22_000_000);
    assert_eq!(period["used_credits_micro"], 20_000_000);
    assert_eq!(period["remaining_credits_micro"], 2_000_000);
    assert_eq!(period["remaining_percentage"], 9);
    assert_eq!(period["next_reset"], "2026-03-01T00:00:00Z");
    assert_eq!(period["warning"], true);
    assert_eq!(period["exhausted"], false);
    assert_eq!(value["tiers"][1]["periods"][0]["period"], "monthly");
}

#[test]
fn an_empty_status_serializes_an_empty_tiers_array_and_the_threshold() {
    let dto = QuotaStatusResponseDto::from(QuotaStatus {
        tiers: vec![],
        warning_threshold_pct: 90,
    });
    assert_eq!(
        serde_json::to_value(&dto).unwrap(),
        json!({ "tiers": [], "warning_threshold_pct": 90 })
    );
}

#[test]
fn the_enumerations_use_the_lowercase_wire_values() {
    assert_eq!(
        serde_json::to_value(QuotaTierDto::Premium).unwrap(),
        "premium"
    );
    assert_eq!(serde_json::to_value(QuotaTierDto::Total).unwrap(), "total");
    assert_eq!(
        serde_json::to_value(QuotaPeriodDto::Daily).unwrap(),
        "daily"
    );
    assert_eq!(
        serde_json::to_value(QuotaPeriodDto::Monthly).unwrap(),
        "monthly"
    );
}

#[test]
fn the_dtos_carry_no_billing_or_tool_field() {
    let dto = QuotaStatusResponseDto::from(status());
    let text = serde_json::to_string(&dto).unwrap();
    for banned in [
        "web_search",
        "code_interpreter",
        "billing",
        "settlement",
        "reserved",
        "spent",
    ] {
        assert!(!text.contains(banned), "{banned} in {text}");
    }
    let _ = (
        std::mem::size_of::<QuotaPeriodStatusDto>(),
        std::mem::size_of::<QuotaTierStatusDto>(),
    );
}
