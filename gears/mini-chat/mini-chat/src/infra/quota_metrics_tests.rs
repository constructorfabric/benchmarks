//! Cost metrics tests over an in-memory OTLP pipeline: the series names under the configured
//! prefix, their labels (decision, model, tier, period only), the recording rules and the
//! declared-but-unrecorded series that stay unrecorded through a whole preflight, reserve and
//! settlement.
//!
//! Slice S1-all: algorithm `cpt-cf-mini-chat-algo-quota-and-billing-cost-metrics`, flow
//! `cpt-cf-mini-chat-flow-quota-and-billing-admin-cost-observability`, definition of done
//! `cpt-cf-mini-chat-dod-quota-and-billing-cost-metrics`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::{BTreeMap, BTreeSet};

use opentelemetry::metrics::MeterProvider;
use opentelemetry_sdk::metrics::data::{AggregatedMetrics, MetricData};

use crate::domain::quota::{PeriodType, QuotaMetrics};
use crate::infra::metrics::MiniChatMetrics;
use crate::test_support::s2::{MetricsHarness, metrics_harness};

type HistogramPoint = (BTreeMap<String, String>, u64, f64);

const ALLOWED_LABELS: [&str; 4] = ["decision", "model", "tier", "period"];

const UNRECORDED: [&str; 7] = [
    "quota_preflight_v2",
    "quota_tier_downgrade",
    "quota_negative",
    "quota_overshoot_tokens",
    "quota_image_commit",
    "credits_overflow",
    "unknown_error_code",
];

fn points(harness: &MetricsHarness, name: &str) -> Vec<(BTreeMap<String, String>, u64)> {
    harness.counter_points(name)
}

fn labels(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect()
}

/// The data points of a histogram: `(attributes, count, sum)`.
fn histogram_points(harness: &MetricsHarness, name: &str) -> Vec<HistogramPoint> {
    harness.exporter.reset();
    harness.provider.force_flush().unwrap();
    let mut found = Vec::new();
    for resource_metrics in harness.exporter.get_finished_metrics().unwrap() {
        for scope in resource_metrics.scope_metrics() {
            for metric in scope.metrics() {
                if metric.name() != name {
                    continue;
                }
                if let AggregatedMetrics::F64(MetricData::Histogram(histogram)) = metric.data() {
                    for point in histogram.data_points() {
                        let attributes = point
                            .attributes()
                            .map(|kv| (kv.key.as_str().to_owned(), kv.value.to_string()))
                            .collect();
                        found.push((attributes, point.count(), point.sum()));
                    }
                }
            }
        }
    }
    found
}

#[test]
fn preflight_counts_one_decision_with_its_model_and_tier() {
    let harness = metrics_harness("mini_chat");
    harness
        .metrics
        .preflight("downgrade", "standard-1", "standard");
    harness
        .metrics
        .preflight("downgrade", "standard-1", "standard");
    harness.metrics.preflight("allow", "premium-1", "premium");
    harness.metrics.preflight("reject", "premium-1", "premium");
    let mut found = points(&harness, "mini_chat_quota_preflight");
    found.sort();
    assert_eq!(
        found,
        vec![
            (
                labels(&[
                    ("decision", "allow"),
                    ("model", "premium-1"),
                    ("tier", "premium")
                ]),
                1
            ),
            (
                labels(&[
                    ("decision", "downgrade"),
                    ("model", "standard-1"),
                    ("tier", "standard")
                ]),
                2
            ),
            (
                labels(&[
                    ("decision", "reject"),
                    ("model", "premium-1"),
                    ("tier", "premium")
                ]),
                1
            ),
        ]
    );
}

#[test]
fn the_estimated_tokens_are_a_histogram_of_the_reserve_tokens_without_labels() {
    let harness = metrics_harness("mini_chat");
    harness.metrics.estimated_tokens(1500);
    harness.metrics.estimated_tokens(2500);
    let found = histogram_points(&harness, "mini_chat_quota_estimated_tokens");
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].1, 2);
    assert!((found[0].2 - 4000.0).abs() < f64::EPSILON);
    assert!(found[0].0.is_empty(), "{:?}", found[0].0);
}

#[test]
fn the_actual_tokens_are_a_histogram_of_the_provider_tokens() {
    let harness = metrics_harness("mini_chat");
    harness.metrics.actual_tokens(1200);
    let found = histogram_points(&harness, "mini_chat_quota_actual_tokens");
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].1, 1);
    assert!((found[0].2 - 1200.0).abs() < f64::EPSILON);
}

#[test]
fn reserve_commit_and_overshoot_are_counted_per_period() {
    let harness = metrics_harness("mini_chat");
    harness.metrics.reserve(PeriodType::Daily);
    harness.metrics.reserve(PeriodType::Monthly);
    harness.metrics.commit(PeriodType::Daily);
    harness.metrics.commit(PeriodType::Monthly);
    harness.metrics.commit(PeriodType::Monthly);
    harness.metrics.overshoot(PeriodType::Daily);
    for (name, daily, monthly) in [
        ("mini_chat_quota_reserve", 1, 1),
        ("mini_chat_quota_commit", 1, 2),
        ("mini_chat_quota_overshoot", 1, 0),
    ] {
        let mut found = points(&harness, name);
        found.sort();
        let mut expected = vec![(labels(&[("period", "daily")]), daily)];
        if monthly > 0 {
            expected.push((labels(&[("period", "monthly")]), monthly));
        }
        expected.sort();
        assert_eq!(found, expected, "{name}");
    }
}

#[test]
fn code_interpreter_calls_are_added_by_the_number_of_calls_per_model() {
    let harness = metrics_harness("mini_chat");
    harness.metrics.code_interpreter_calls("standard-1", 3);
    harness.metrics.code_interpreter_calls("standard-1", 2);
    harness.metrics.code_interpreter_calls("premium-1", 1);
    let mut found = points(&harness, "mini_chat_code_interpreter_calls");
    found.sort();
    assert_eq!(
        found,
        vec![
            (labels(&[("model", "premium-1")]), 1),
            (labels(&[("model", "standard-1")]), 5),
        ]
    );
}

#[test]
fn the_series_use_the_configured_prefix() {
    let harness = metrics_harness("acme");
    harness.metrics.preflight("allow", "m", "standard");
    harness.metrics.reserve(PeriodType::Daily);
    assert!(!points(&harness, "acme_quota_preflight").is_empty());
    assert!(!points(&harness, "acme_quota_reserve").is_empty());
    assert!(points(&harness, "mini_chat_quota_preflight").is_empty());
}

#[test]
fn no_series_carries_a_label_outside_the_low_cardinality_set() {
    let harness = metrics_harness("mini_chat");
    harness.metrics.preflight("allow", "m", "standard");
    harness.metrics.reserve(PeriodType::Daily);
    harness.metrics.commit(PeriodType::Monthly);
    harness.metrics.overshoot(PeriodType::Daily);
    harness.metrics.code_interpreter_calls("m", 1);
    for name in [
        "mini_chat_quota_preflight",
        "mini_chat_quota_reserve",
        "mini_chat_quota_commit",
        "mini_chat_quota_overshoot",
        "mini_chat_code_interpreter_calls",
    ] {
        for (attributes, _) in points(&harness, name) {
            for key in attributes.keys() {
                assert!(
                    ALLOWED_LABELS.contains(&key.as_str()),
                    "{name} has the label {key}"
                );
            }
            let forbidden: BTreeSet<&str> = [
                "tenant_id",
                "user_id",
                "chat_id",
                "request_id",
                "error",
                "turn_id",
            ]
            .into();
            assert!(
                attributes
                    .keys()
                    .all(|key| !forbidden.contains(key.as_str())),
                "{name}"
            );
        }
    }
}

#[test]
fn the_registered_but_unrecorded_series_stay_unrecorded() {
    let harness = metrics_harness("mini_chat");
    harness.metrics.preflight("allow", "m", "standard");
    harness.metrics.estimated_tokens(10);
    harness.metrics.reserve(PeriodType::Daily);
    harness.metrics.commit(PeriodType::Daily);
    harness.metrics.overshoot(PeriodType::Daily);
    harness.metrics.actual_tokens(10);
    harness.metrics.code_interpreter_calls("m", 1);
    let names = harness.metrics.instrument_names();
    for name in UNRECORDED {
        let full = format!("mini_chat_{name}");
        assert!(names.contains(&full), "{full} stays registered");
        assert!(
            points(&harness, &full).is_empty(),
            "{full} must not be recorded"
        );
    }
}

#[test]
fn the_undefined_series_are_not_registered() {
    let harness = metrics_harness("mini_chat");
    let names = harness.metrics.instrument_names();
    for undefined in [
        "mini_chat_quota_reserved_tokens",
        "mini_chat_quota_overshoot_exceeded",
    ] {
        assert!(!names.contains(&undefined.to_owned()), "{undefined}");
    }
}

#[test]
fn the_recorder_is_the_instrument_set_of_the_gear() {
    // the cost series are recorded on the instruments the foundation registered, not on new ones
    let harness = metrics_harness("mini_chat");
    let before = harness.metrics.instrument_names();
    harness.metrics.preflight("allow", "m", "standard");
    harness.metrics.estimated_tokens(1);
    assert_eq!(harness.metrics.instrument_names(), before);
    let _ = harness.provider.meter("unused");
    let _: Option<&MiniChatMetrics> = None;
}

// ── the whole flow on the real instruments ───────────────────────────────────

#[tokio::test]
async fn a_whole_turn_records_the_documented_series_and_none_of_the_unrecorded_ones() {
    use crate::domain::quota::{
        FinalizeRequest, Finalizer, TerminalState, ToolCallCounts, TurnTerminalUpdate,
    };
    use crate::test_support::quota::{
        QuotaRigOptions, STANDARD, plain_facts, quota_rig_with, usage, utc,
    };

    let harness = metrics_harness("mini_chat");
    let sink: std::sync::Arc<dyn QuotaMetrics> = harness.metrics.clone();
    let rig = quota_rig_with(QuotaRigOptions {
        metrics: Some(sink),
        ..QuotaRigOptions::new()
    })
    .await;
    let chat = rig.chat_a1(STANDARD).await;
    let decision = rig.preflight(&chat, STANDARD, plain_facts()).await.unwrap();
    let identity = crate::test_support::quota::QuotaRig::identity(&chat);
    let (receipt, turn_id) = rig.reserve_new(&identity, &decision).await.unwrap();
    receipt.record_after_commit(harness.metrics.as_ref());
    let finalize = FinalizeRequest {
        tenant_id: identity.tenant_id,
        turn_id,
        update: TurnTerminalUpdate {
            state: TerminalState::Completed,
            error_code: None,
            error_detail: None,
            assistant_message_id: None,
            completed_at: utc(2026, 2, 28, 15, 31, 0),
        },
        finalizer: Finalizer::Stream {
            selected_model: STANDARD.to_owned(),
            period_starts: decision.period_starts,
        },
        usage: Some(usage(900, 300)),
        tool_counts: ToolCallCounts {
            web_search: 0,
            code_interpreter: 2,
            file_search: 0,
        },
    };
    let settled = match rig.finalize(&finalize).await.unwrap() {
        crate::domain::quota::FinalizeOutcome::Settled(settlement) => settlement,
        other => panic!("{other:?}"),
    };
    settled.result.record_after_commit(harness.metrics.as_ref());

    assert_eq!(
        points(&harness, "mini_chat_quota_preflight"),
        vec![(
            labels(&[
                ("decision", "allow"),
                ("model", STANDARD),
                ("tier", "standard")
            ]),
            1
        )]
    );
    let mut reserve = points(&harness, "mini_chat_quota_reserve");
    reserve.sort();
    assert_eq!(
        reserve,
        vec![
            (labels(&[("period", "daily")]), 1),
            (labels(&[("period", "monthly")]), 1)
        ]
    );
    let mut commit = points(&harness, "mini_chat_quota_commit");
    commit.sort();
    assert_eq!(
        commit,
        vec![
            (labels(&[("period", "daily")]), 1),
            (labels(&[("period", "monthly")]), 1)
        ]
    );
    assert_eq!(
        points(&harness, "mini_chat_code_interpreter_calls"),
        vec![(labels(&[("model", STANDARD)]), 2)]
    );
    assert_eq!(
        histogram_points(&harness, "mini_chat_quota_estimated_tokens")[0].1,
        1
    );
    assert_eq!(
        histogram_points(&harness, "mini_chat_quota_actual_tokens")[0].1,
        1
    );
    assert!(points(&harness, "mini_chat_quota_overshoot").is_empty());
    for name in UNRECORDED {
        assert!(
            points(&harness, &format!("mini_chat_{name}")).is_empty(),
            "{name}"
        );
    }
}
