#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! The constants of the feature and the layering of the worker domain logic
//! (`cpt-cf-mini-chat-dod-background-workers-quality-gates`, "Compile-time
//! gates": the domain depends on ports, not on `SeaORM` entities, HTTP types or
//! `kube` types).

use std::time::Duration;

use super::{
    CHAT_CLEANUP_CONCURRENCY, CHAT_CLEANUP_DELIVERY_BUDGET, HandlerOutcome, ORPHAN_SCAN_BATCH,
    SUMMARY_COMMIT_RESERVE, SUMMARY_RESPONSE_MAX_BYTES, SYSTEM_SUBJECT,
};

#[test]
fn the_feature_constants_have_their_designed_values() {
    assert_eq!(ORPHAN_SCAN_BATCH, 100);
    assert_eq!(CHAT_CLEANUP_DELIVERY_BUDGET, Duration::from_secs(25));
    assert_eq!(CHAT_CLEANUP_CONCURRENCY, 4);
    assert_eq!(SUMMARY_COMMIT_RESERVE, Duration::from_secs(10));
    assert_eq!(SUMMARY_RESPONSE_MAX_BYTES, 1024 * 1024);
    assert_eq!(
        SYSTEM_SUBJECT.to_string(),
        "11111111-6a88-4768-9dfc-6bcd5187d9ed"
    );
}

#[test]
fn the_delivery_budget_leaves_headroom_below_the_lease_cancel_point() {
    // the 30 s lease is cancelled at 28 s: the budget keeps 2 s for the last writes
    assert!(CHAT_CLEANUP_DELIVERY_BUDGET + Duration::from_secs(3) <= Duration::from_secs(30));
}

#[test]
fn a_handler_outcome_is_ok_retry_or_reject_with_a_reason() {
    assert_ne!(HandlerOutcome::Ok, HandlerOutcome::Retry);
    assert_eq!(
        HandlerOutcome::Reject("why".to_owned()),
        HandlerOutcome::Reject("why".to_owned())
    );
}

const SOURCES: [(&str, &str); 9] = [
    ("chat_cleanup.rs", include_str!("chat_cleanup.rs")),
    ("clock.rs", include_str!("clock.rs")),
    ("metrics.rs", include_str!("metrics.rs")),
    ("orphan.rs", include_str!("orphan.rs")),
    ("ports.rs", include_str!("ports.rs")),
    ("summary.rs", include_str!("summary.rs")),
    ("summary_parse.rs", include_str!("summary_parse.rs")),
    ("summary_prompt.rs", include_str!("summary_prompt.rs")),
    ("summary_trigger.rs", include_str!("summary_trigger.rs")),
];

#[test]
fn the_worker_domain_logic_imports_no_persistence_http_or_kubernetes_type() {
    for (file, source) in SOURCES {
        for forbidden in [
            "sea_orm",
            "toolkit_db",
            "axum",
            "http::",
            "kube::",
            "k8s_openapi",
            "crate::infra",
            "oagw_sdk",
        ] {
            assert!(
                !source.lines().any(|line| {
                    let line = line.trim_start();
                    !line.starts_with("//") && line.contains(forbidden)
                }),
                "{file} must not use `{forbidden}`"
            );
        }
    }
}
