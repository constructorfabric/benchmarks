//! The turn mutation metrics: the `result` classes of `mini_chat_turn_mutation`, the series
//! recorded through an in-memory OTLP pipeline (`mini_chat_turn_mutation{op,result}` once per
//! request, `mini_chat_turn_mutation_latency_ms{op}`), low-cardinality labels only and no
//! reaction series.
//!
//! Slice S1-all of `cpt-cf-mini-chat-feature-turn-mutations-and-reactions`: algorithm
//! `cpt-cf-mini-chat-algo-turn-mutations-and-reactions-metrics` (`inst-met-counter`,
//! `inst-met-latency`, `inst-met-cardinality`, `inst-met-no-reaction-metric`, `inst-met-return`);
//! flow `cpt-cf-mini-chat-flow-turn-mutations-and-reactions-admin-observability`
//! (`inst-obs-mutation-counter`, `inst-obs-mutation-latency`, `inst-obs-return`); definition of
//! done `cpt-cf-mini-chat-dod-turn-mutations-and-reactions-errors-observability`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeSet;
use std::time::Duration;

use super::{MutationMetrics, MutationOp, MutationResult, classify_rejection};
use crate::domain::error::{ConflictReason, DomainError, EnqueueSite, QuotaScope, ResourceKind};
use crate::test_support::s2::metrics_harness;

const OPS: [MutationOp; 3] = [MutationOp::Retry, MutationOp::Edit, MutationOp::Delete];

// ── the result classes ───────────────────────────────────────────────────────

#[test]
fn the_result_label_takes_the_eight_documented_values() {
    // inst-met-counter
    let all = [
        (MutationResult::Ok, "ok"),
        (MutationResult::NotFound, "not_found"),
        (MutationResult::Forbidden, "forbidden"),
        (MutationResult::NotLatestTurn, "not_latest_turn"),
        (MutationResult::TurnRunning, "turn_running"),
        (
            MutationResult::GenerationInProgress,
            "generation_in_progress",
        ),
        (MutationResult::PreflightRejected, "preflight_rejected"),
        (MutationResult::Error, "error"),
    ];
    for (result, wire) in all {
        assert_eq!(result.as_str(), wire);
    }
    let distinct: BTreeSet<&str> = all.iter().map(|(_, wire)| *wire).collect();
    assert_eq!(distinct.len(), 8);
}

#[test]
fn the_operation_label_values_are_retry_edit_and_delete() {
    assert_eq!(OPS.map(MutationOp::as_str), ["retry", "edit", "delete"]);
}

#[test]
fn a_missing_chat_turn_or_message_is_not_found() {
    for resource in [
        ResourceKind::Chat,
        ResourceKind::Turn,
        ResourceKind::Message,
    ] {
        let error = DomainError::NotFound { resource, id: None };
        assert_eq!(classify_rejection(&error, false), MutationResult::NotFound);
    }
}

#[test]
fn a_requester_mismatch_or_a_deny_is_forbidden() {
    assert_eq!(
        classify_rejection(&DomainError::AuthzDenied, false),
        MutationResult::Forbidden
    );
}

#[test]
fn the_mutation_conflicts_have_their_own_classes() {
    let conflict = |reason| DomainError::Conflict { reason };
    assert_eq!(
        classify_rejection(&conflict(ConflictReason::NotLatestTurn), false),
        MutationResult::NotLatestTurn
    );
    assert_eq!(
        classify_rejection(&conflict(ConflictReason::GenerationInProgress), false),
        MutationResult::GenerationInProgress
    );
    assert_eq!(
        classify_rejection(&DomainError::TurnNotTerminal, false),
        MutationResult::TurnRunning
    );
}

#[test]
fn a_preflight_rejection_is_preflight_rejected() {
    let rejections = [
        DomainError::QuotaExceeded {
            scope: QuotaScope::Tokens,
        },
        DomainError::QuotaExceeded {
            scope: QuotaScope::WebSearch,
        },
        DomainError::FeatureDisabled { subject: "images" },
        DomainError::TooManyImages,
        DomainError::VisionNotSupported,
        DomainError::InputTooLong,
        DomainError::InvalidModel {
            model: "ghost".to_owned(),
        },
    ];
    for error in rejections {
        assert_eq!(
            classify_rejection(&error, true),
            MutationResult::PreflightRejected,
            "{error:?}"
        );
    }
}

#[test]
fn an_internal_failure_is_an_error_class_at_every_stage() {
    let failures = [
        DomainError::internal("database down"),
        DomainError::PolicyUnavailable {
            cause: "plugin down".to_owned(),
        },
        DomainError::OutboxPayloadTooLarge {
            site: EnqueueSite::TurnRetry,
        },
    ];
    for error in failures {
        for at_preflight in [false, true] {
            assert_eq!(
                classify_rejection(&error, at_preflight),
                MutationResult::Error,
                "{error:?} at_preflight={at_preflight}"
            );
        }
    }
}

// ── the recorded series ──────────────────────────────────────────────────────

#[test]
fn the_counter_is_recorded_with_op_and_result_only() {
    // inst-met-counter, inst-met-cardinality, inst-obs-mutation-counter
    let harness = metrics_harness("mini_chat");
    harness
        .metrics
        .mutation(MutationOp::Retry, MutationResult::Ok);
    harness
        .metrics
        .mutation(MutationOp::Retry, MutationResult::Ok);
    harness
        .metrics
        .mutation(MutationOp::Edit, MutationResult::NotLatestTurn);
    harness
        .metrics
        .mutation(MutationOp::Delete, MutationResult::GenerationInProgress);

    let points = harness.series("turn_mutation");
    assert_eq!(points.len(), 3, "{points:?}");
    for (attributes, _) in &points {
        let keys: BTreeSet<&str> = attributes.keys().map(String::as_str).collect();
        assert_eq!(keys, BTreeSet::from(["op", "result"]), "{attributes:?}");
    }
    let count = |op: &str, result: &str| {
        points
            .iter()
            .filter(|(a, _)| a["op"] == op && a["result"] == result)
            .map(|(_, v)| *v)
            .sum::<i64>()
    };
    assert_eq!(count("retry", "ok"), 2);
    assert_eq!(count("edit", "not_latest_turn"), 1);
    assert_eq!(count("delete", "generation_in_progress"), 1);
}

#[test]
fn the_latency_is_recorded_with_the_op_label_only() {
    // inst-met-latency, inst-obs-mutation-latency
    let harness = metrics_harness("mini_chat");
    for op in OPS {
        harness
            .metrics
            .mutation_latency(op, Duration::from_millis(12));
    }
    harness
        .metrics
        .mutation_latency(MutationOp::Retry, Duration::from_millis(40));
    let points = harness.series("turn_mutation_latency_ms");
    assert_eq!(points.len(), 3, "{points:?}");
    for (attributes, _) in &points {
        let keys: BTreeSet<&str> = attributes.keys().map(String::as_str).collect();
        assert_eq!(keys, BTreeSet::from(["op"]), "{attributes:?}");
    }
    let retry: i64 = points
        .iter()
        .filter(|(a, _)| a["op"] == "retry")
        .map(|(_, v)| *v)
        .sum();
    assert_eq!(retry, 2, "two samples for retry");
}

#[test]
fn no_identifier_is_ever_a_label_value_or_key() {
    // inst-met-cardinality
    let harness = metrics_harness("mini_chat");
    for op in OPS {
        harness.metrics.mutation(op, MutationResult::Ok);
        harness
            .metrics
            .mutation_latency(op, Duration::from_millis(1));
    }
    for name in ["turn_mutation", "turn_mutation_latency_ms"] {
        for (attributes, _) in harness.series(name) {
            for key in attributes.keys() {
                for forbidden in ["tenant_id", "user_id", "chat_id", "request_id"] {
                    assert_ne!(key, forbidden);
                }
            }
        }
    }
}

#[test]
fn nothing_is_recorded_until_a_mutation_is_reported() {
    let harness = metrics_harness("mini_chat");
    assert_eq!(harness.total("turn_mutation"), 0);
    assert_eq!(harness.total("turn_mutation_latency_ms"), 0);
}

#[test]
fn the_series_follow_the_configured_prefix() {
    let harness = metrics_harness("acme_chat");
    harness
        .metrics
        .mutation(MutationOp::Delete, MutationResult::Ok);
    assert_eq!(harness.total("turn_mutation"), 1);
    assert!(harness.recorded().contains("turn_mutation"));
}

#[test]
fn there_is_no_reaction_series() {
    // inst-met-no-reaction-metric: the DESIGN defines none
    let harness = metrics_harness("mini_chat");
    let names = harness.metrics.instrument_names();
    assert!(
        names.iter().all(|name| !name.contains("reaction")),
        "{names:?}"
    );
}
