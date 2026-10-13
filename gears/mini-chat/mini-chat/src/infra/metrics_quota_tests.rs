#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Slice 6: the billing group of the cost metrics
//! (`cpt-cf-mini-chat-algo-quota-billing-record-metrics`,
//! `cpt-cf-mini-chat-dod-quota-billing-cost-metrics`) recorded by
//! `MiniChatMetrics` through the `QuotaMetrics` port, read back with the
//! in-memory exporter of the existing metrics harness.

use std::collections::{BTreeMap, BTreeSet};

use mini_chat_sdk::ModelTier;
use opentelemetry::metrics::MeterProvider;
use opentelemetry_sdk::metrics::data::{AggregatedMetrics, MetricData};
use opentelemetry_sdk::metrics::{InMemoryMetricExporter, PeriodicReader, SdkMeterProvider};

use super::MiniChatMetrics;
use crate::domain::service::quota::ports::{PreflightLabel, QuotaMetrics};
use crate::domain::service::quota::types::QuotaPeriod;

struct Harness {
    provider: SdkMeterProvider,
    exporter: InMemoryMetricExporter,
    metrics: MiniChatMetrics,
}

fn harness(prefix: &str) -> Harness {
    let exporter = InMemoryMetricExporter::default();
    let provider = SdkMeterProvider::builder()
        .with_reader(PeriodicReader::builder(exporter.clone()).build())
        .build();
    let metrics = MiniChatMetrics::new(&provider.meter("mini_chat"), prefix);
    Harness {
        provider,
        exporter,
        metrics,
    }
}

type Labels = BTreeMap<String, String>;

impl Harness {
    /// The latest collection only (the exporter keeps every cumulative one).
    fn latest(&self) -> Vec<opentelemetry_sdk::metrics::data::ResourceMetrics> {
        self.provider.force_flush().expect("flush");
        self.exporter
            .get_finished_metrics()
            .unwrap()
            .into_iter()
            .last()
            .into_iter()
            .collect()
    }

    /// `(labels, value)` of every data point of the counter `name`.
    fn counter(&self, name: &str) -> Vec<(Labels, u64)> {
        let mut out = Vec::new();
        for rm in self.latest() {
            for sm in rm.scope_metrics() {
                for metric in sm.metrics() {
                    if metric.name() != name {
                        continue;
                    }
                    let AggregatedMetrics::U64(MetricData::Sum(sum)) = metric.data() else {
                        panic!("`{name}` is not a u64 sum");
                    };
                    for point in sum.data_points() {
                        out.push((labels(point.attributes()), point.value()));
                    }
                }
            }
        }
        out.sort();
        out
    }

    /// `(labels, count, sum)` of every data point of the histogram `name`.
    fn histogram(&self, name: &str) -> Vec<(Labels, u64, f64)> {
        let mut out = Vec::new();
        for rm in self.latest() {
            for sm in rm.scope_metrics() {
                for metric in sm.metrics() {
                    if metric.name() != name {
                        continue;
                    }
                    let AggregatedMetrics::F64(MetricData::Histogram(histogram)) = metric.data()
                    else {
                        panic!("`{name}` is not an f64 histogram");
                    };
                    for point in histogram.data_points() {
                        out.push((labels(point.attributes()), point.count(), point.sum()));
                    }
                }
            }
        }
        out
    }

    /// Names of every metric that exported at least one data point.
    fn exported_names(&self) -> BTreeSet<String> {
        self.latest()
            .iter()
            .flat_map(opentelemetry_sdk::metrics::data::ResourceMetrics::scope_metrics)
            .flat_map(opentelemetry_sdk::metrics::data::ScopeMetrics::metrics)
            .map(|metric| metric.name().to_owned())
            .collect()
    }

    /// Every label key of every exported data point.
    fn all_label_keys(&self) -> BTreeSet<String> {
        let mut keys = BTreeSet::new();
        for rm in self.latest() {
            for sm in rm.scope_metrics() {
                for metric in sm.metrics() {
                    match metric.data() {
                        AggregatedMetrics::U64(MetricData::Sum(sum)) => {
                            for point in sum.data_points() {
                                keys.extend(labels(point.attributes()).into_keys());
                            }
                        }
                        AggregatedMetrics::F64(MetricData::Histogram(histogram)) => {
                            for point in histogram.data_points() {
                                keys.extend(labels(point.attributes()).into_keys());
                            }
                        }
                        other => panic!("unexpected aggregation: {other:?}"),
                    }
                }
            }
        }
        keys
    }
}

fn labels<'a>(attributes: impl Iterator<Item = &'a opentelemetry::KeyValue>) -> Labels {
    attributes
        .map(|kv| (kv.key.to_string(), kv.value.as_str().into_owned()))
        .collect()
}

fn label_set(pairs: &[(&str, &str)]) -> Labels {
    pairs
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect()
}

// ---- preflight ------------------------------------------------------------------------------------------

#[test]
fn the_preflight_counter_carries_decision_model_and_tier_only() {
    let harness = harness("");

    harness
        .metrics
        .record_preflight(PreflightLabel::Allow, "gpt-5.2", ModelTier::Premium);
    harness
        .metrics
        .record_preflight(PreflightLabel::Allow, "gpt-5.2", ModelTier::Premium);
    harness
        .metrics
        .record_preflight(PreflightLabel::Downgrade, "gpt-4.1", ModelTier::Standard);
    harness
        .metrics
        .record_preflight(PreflightLabel::Reject, "gpt-5.2", ModelTier::Premium);

    assert_eq!(
        harness.counter("mini_chat_quota_preflight"),
        vec![
            (
                label_set(&[
                    ("decision", "allow"),
                    ("model", "gpt-5.2"),
                    ("tier", "premium")
                ]),
                2
            ),
            (
                label_set(&[
                    ("decision", "downgrade"),
                    ("model", "gpt-4.1"),
                    ("tier", "standard")
                ]),
                1
            ),
            (
                label_set(&[
                    ("decision", "reject"),
                    ("model", "gpt-5.2"),
                    ("tier", "premium")
                ]),
                1
            ),
        ]
    );
}

#[test]
fn the_preflight_decision_labels_are_the_documented_values() {
    assert_eq!(PreflightLabel::Allow.as_str(), "allow");
    assert_eq!(PreflightLabel::Downgrade.as_str(), "downgrade");
    assert_eq!(PreflightLabel::Reject.as_str(), "reject");
}

#[test]
fn the_estimated_tokens_are_a_histogram_without_labels() {
    let harness = harness("");

    harness.metrics.record_estimated_tokens(1500);
    harness.metrics.record_estimated_tokens(500);

    let points = harness.histogram("mini_chat_quota_estimated_tokens");
    assert_eq!(points.len(), 1);
    assert_eq!(points[0].0, Labels::new());
    assert_eq!(points[0].1, 2);
    assert!((points[0].2 - 2000.0).abs() < f64::EPSILON);
}

// ---- reserve and settlement --------------------------------------------------------------------------

#[test]
fn reserve_commit_and_overshoot_are_counted_per_period() {
    let harness = harness("");

    for period in QuotaPeriod::ALL {
        harness.metrics.record_reserve(period);
        harness.metrics.record_commit(period);
        harness.metrics.record_overshoot(period);
    }
    harness.metrics.record_reserve(QuotaPeriod::Daily);

    let per_period = |name: &str| harness.counter(name);
    assert_eq!(
        per_period("mini_chat_quota_reserve"),
        vec![
            (label_set(&[("period", "daily")]), 2),
            (label_set(&[("period", "monthly")]), 1),
        ]
    );
    for name in ["mini_chat_quota_commit", "mini_chat_quota_overshoot"] {
        assert_eq!(
            per_period(name),
            vec![
                (label_set(&[("period", "daily")]), 1),
                (label_set(&[("period", "monthly")]), 1),
            ],
            "{name}"
        );
    }
}

#[test]
fn the_actual_tokens_are_a_histogram() {
    let harness = harness("");

    harness.metrics.record_actual_tokens(1200);

    let points = harness.histogram("mini_chat_quota_actual_tokens");
    assert_eq!(points.len(), 1);
    assert_eq!(points[0].1, 1);
    assert!((points[0].2 - 1200.0).abs() < f64::EPSILON);
}

#[test]
fn code_interpreter_calls_add_the_call_count_per_effective_model() {
    let harness = harness("");

    harness.metrics.record_code_interpreter_calls("gpt-5.2", 3);
    harness.metrics.record_code_interpreter_calls("gpt-5.2", 2);
    harness.metrics.record_code_interpreter_calls("gpt-4.1", 1);

    assert_eq!(
        harness.counter("mini_chat_code_interpreter_calls"),
        vec![
            (label_set(&[("model", "gpt-4.1")]), 1),
            (label_set(&[("model", "gpt-5.2")]), 5),
        ]
    );
}

// ---- prefix, labels and the unrecorded instruments -----------------------------------------------------

#[test]
fn the_configured_prefix_names_every_billing_instrument() {
    let harness = harness("acme");

    harness.metrics.record_reserve(QuotaPeriod::Daily);
    harness
        .metrics
        .record_preflight(PreflightLabel::Allow, "m", ModelTier::Standard);
    harness.metrics.record_estimated_tokens(10);

    assert_eq!(harness.counter("acme_quota_reserve").len(), 1);
    assert_eq!(harness.counter("acme_quota_preflight").len(), 1);
    assert_eq!(harness.histogram("acme_quota_estimated_tokens").len(), 1);
    assert!(harness.counter("mini_chat_quota_reserve").is_empty());
}

#[test]
fn only_the_seven_billing_instruments_are_recorded_with_the_four_low_cardinality_labels() {
    let harness = harness("");

    harness
        .metrics
        .record_preflight(PreflightLabel::Allow, "m", ModelTier::Premium);
    harness.metrics.record_estimated_tokens(1);
    for period in QuotaPeriod::ALL {
        harness.metrics.record_reserve(period);
        harness.metrics.record_commit(period);
        harness.metrics.record_overshoot(period);
    }
    harness.metrics.record_actual_tokens(1);
    harness.metrics.record_code_interpreter_calls("m", 1);

    let exported = harness.exported_names();
    let allowed: BTreeSet<String> = [
        "mini_chat_quota_preflight",
        "mini_chat_quota_reserve",
        "mini_chat_quota_commit",
        "mini_chat_quota_overshoot",
        "mini_chat_quota_estimated_tokens",
        "mini_chat_quota_actual_tokens",
        "mini_chat_code_interpreter_calls",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect();
    assert_eq!(exported, allowed);
    for never in [
        "mini_chat_quota_preflight_v2",
        "mini_chat_quota_tier_downgrade",
        "mini_chat_quota_negative",
        "mini_chat_quota_overshoot_tokens",
        "mini_chat_quota_image_commit",
        "mini_chat_credits_overflow",
        "mini_chat_unknown_error_code",
    ] {
        assert!(
            !exported.contains(never),
            "{never} is declared but never recorded"
        );
    }
    let keys = harness.all_label_keys();
    let allowed_keys: BTreeSet<String> = ["decision", "model", "tier", "period"]
        .into_iter()
        .map(str::to_owned)
        .collect();
    assert!(keys.is_subset(&allowed_keys), "{keys:?}");
    for forbidden in ["tenant_id", "user_id", "chat_id", "request_id", "turn_id"] {
        assert!(!keys.contains(forbidden), "{forbidden}");
    }
}
