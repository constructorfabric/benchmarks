//! Reconcile task tests with paused tokio time: retry schedule (2 s doubling up to 60 s), the
//! single warning after 2 minutes, recovery and cancellation.
//!
//! Slice S2: algorithm `oagw-reconcile`, state machine `provider-provisioning` (transitions
//! `DEFERRED -> DEFERRED`, `DEFERRED -> PROVISIONED`, `DEFERRED -> UNPROVISIONED`).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::time::Duration;

use serde_json::json;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use tracing_test::traced_test;

use super::{next_delay, run_reconcile};
use crate::config::MiniChatConfig;
use crate::domain::provider_registry::ProviderRegistry;
use crate::infra::oagw::provisioning::ProvisionTarget;
use crate::test_support::s2::{RecordingOagw, S2S_SUBJECT, UpstreamScript, s2s_ctx};

fn targets(ids: &[(&str, &str)]) -> Vec<ProvisionTarget> {
    let mut providers = serde_json::Map::new();
    for (id, host) in ids {
        providers.insert(
            (*id).to_owned(),
            json!({ "kind": "openai_responses", "host": host, "storage_kind": "openai" }),
        );
    }
    let config = MiniChatConfig::from_value(json!({
        "client_credentials": { "client_id": "c", "client_secret": "s" },
        "providers": providers
    }))
    .unwrap();
    let registry = ProviderRegistry::build(&config.providers);
    ids.iter()
        .map(|(id, _)| ProvisionTarget {
            label: (*id).to_owned(),
            provider: registry.entry(id).unwrap().base.clone(),
        })
        .collect()
}

fn offsets_millis(oagw: &RecordingOagw, start: Instant) -> Vec<u128> {
    oagw.upstream_calls
        .lock()
        .unwrap()
        .iter()
        .map(|call| call.at.duration_since(start).as_millis())
        .collect()
}

// ── schedule ─────────────────────────────────────────────────────────────────

#[test]
fn the_delay_doubles_from_2_seconds_and_stays_at_60() {
    let mut delay = Duration::from_secs(2);
    let mut seen = vec![delay.as_secs()];
    for _ in 0..7 {
        delay = next_delay(delay);
        seen.push(delay.as_secs());
    }
    assert_eq!(seen, vec![2, 4, 8, 16, 32, 60, 60, 60]);
}

#[test]
fn a_delay_above_30_seconds_is_capped_at_60() {
    assert_eq!(next_delay(Duration::from_secs(40)), Duration::from_secs(60));
    assert_eq!(next_delay(Duration::from_secs(60)), Duration::from_secs(60));
}

#[tokio::test(start_paused = true)]
async fn retries_run_at_2_6_14_30_62_seconds_and_so_on_while_the_secret_stays_unreadable() {
    let oagw = RecordingOagw::accepting();
    oagw.script(
        "waiting.example.com",
        UpstreamScript::SecretUnreadableForever,
    );
    let cancel = CancellationToken::new();
    let start = Instant::now();

    let task = tokio::spawn(run_reconcile(
        oagw.clone(),
        s2s_ctx(),
        targets(&[("waiting", "waiting.example.com")]),
        cancel.clone(),
    ));
    tokio::time::sleep(Duration::from_secs(300)).await;
    cancel.cancel();
    task.await.unwrap();

    let offsets = offsets_millis(&oagw, start);
    assert_eq!(
        offsets,
        vec![
            2_000, 6_000, 14_000, 30_000, 62_000, 122_000, 182_000, 242_000
        ]
    );
    assert_eq!(oagw.route_call_count(), 0);
}

#[tokio::test(start_paused = true)]
async fn the_first_retry_never_runs_before_2_seconds() {
    let oagw = RecordingOagw::accepting();
    oagw.script(
        "waiting.example.com",
        UpstreamScript::SecretUnreadableForever,
    );
    let cancel = CancellationToken::new();
    let task = tokio::spawn(run_reconcile(
        oagw.clone(),
        s2s_ctx(),
        targets(&[("waiting", "waiting.example.com")]),
        cancel.clone(),
    ));
    tokio::time::sleep(Duration::from_millis(1_900)).await;
    assert_eq!(oagw.upstream_call_count(), 0);
    cancel.cancel();
    task.await.unwrap();
}

// ── recovery ─────────────────────────────────────────────────────────────────

#[tokio::test(start_paused = true)]
async fn the_task_ends_by_itself_once_the_entry_is_provisioned() {
    let oagw = RecordingOagw::accepting();
    oagw.script(
        "waiting.example.com",
        UpstreamScript::SecretUnreadable { failures: 2 },
    );
    let start = Instant::now();

    let task = tokio::spawn(run_reconcile(
        oagw.clone(),
        s2s_ctx(),
        targets(&[("waiting", "waiting.example.com")]),
        CancellationToken::new(),
    ));
    tokio::time::timeout(Duration::from_secs(600), task)
        .await
        .expect("the task must end once nothing is pending")
        .unwrap();

    assert_eq!(offsets_millis(&oagw, start), vec![2_000, 6_000, 14_000]);
    assert_eq!(oagw.route_call_count(), 6, "chat route and five RAG routes");
    for call in oagw.upstream_calls.lock().unwrap().iter() {
        assert_eq!(call.subject, S2S_SUBJECT, "the S2S context is reused");
    }
    for route in oagw.routes() {
        assert_eq!(route.subject, S2S_SUBJECT);
    }
}

#[tokio::test(start_paused = true)]
async fn an_entry_that_succeeds_leaves_the_pending_set_and_is_not_retried_again() {
    let oagw = RecordingOagw::accepting();
    oagw.script("slow.example.com", UpstreamScript::SecretUnreadableForever);
    let cancel = CancellationToken::new();
    let start = Instant::now();

    let task = tokio::spawn(run_reconcile(
        oagw.clone(),
        s2s_ctx(),
        targets(&[("quick", "quick.example.com"), ("slow", "slow.example.com")]),
        cancel.clone(),
    ));
    tokio::time::sleep(Duration::from_secs(20)).await;
    cancel.cancel();
    task.await.unwrap();

    // round 1 at 2 s retries both; rounds at 6 s and 14 s retry only the slow entry
    let calls: Vec<(String, u128)> = oagw
        .upstream_calls
        .lock()
        .unwrap()
        .iter()
        .map(|call| {
            (
                call.alias.clone(),
                call.at.duration_since(start).as_millis(),
            )
        })
        .collect();
    assert_eq!(
        calls,
        vec![
            ("quick.example.com".to_owned(), 2_000),
            ("slow.example.com".to_owned(), 2_000),
            ("slow.example.com".to_owned(), 6_000),
            ("slow.example.com".to_owned(), 14_000),
        ]
    );
}

#[tokio::test(start_paused = true)]
async fn an_empty_deferred_set_returns_at_once() {
    let oagw = RecordingOagw::accepting();
    tokio::time::timeout(
        Duration::from_millis(1),
        run_reconcile(oagw.clone(), s2s_ctx(), vec![], CancellationToken::new()),
    )
    .await
    .expect("nothing to retry");
    assert_eq!(oagw.upstream_call_count(), 0);
}

// ── cancellation ─────────────────────────────────────────────────────────────

#[tokio::test(start_paused = true)]
async fn cancelling_during_the_wait_ends_the_task_at_once_without_a_retry() {
    let oagw = RecordingOagw::accepting();
    oagw.script(
        "waiting.example.com",
        UpstreamScript::SecretUnreadableForever,
    );
    let cancel = CancellationToken::new();
    let task = tokio::spawn(run_reconcile(
        oagw.clone(),
        s2s_ctx(),
        targets(&[("waiting", "waiting.example.com")]),
        cancel.clone(),
    ));
    tokio::time::sleep(Duration::from_secs(1)).await;
    cancel.cancel();
    tokio::time::timeout(Duration::from_millis(10), task)
        .await
        .expect("the wait ends early when the token is cancelled")
        .unwrap();
    assert_eq!(oagw.upstream_call_count(), 0);
}

#[tokio::test(start_paused = true)]
async fn a_token_cancelled_before_the_start_prevents_any_retry() {
    let oagw = RecordingOagw::accepting();
    oagw.script(
        "waiting.example.com",
        UpstreamScript::SecretUnreadableForever,
    );
    let cancel = CancellationToken::new();
    cancel.cancel();
    tokio::time::timeout(
        Duration::from_millis(10),
        run_reconcile(
            oagw.clone(),
            s2s_ctx(),
            targets(&[("waiting", "waiting.example.com")]),
            cancel,
        ),
    )
    .await
    .expect("returns at once");
    assert_eq!(oagw.upstream_call_count(), 0);
}

#[tokio::test(start_paused = true)]
async fn after_a_cancellation_no_further_retry_runs() {
    let oagw = RecordingOagw::accepting();
    oagw.script(
        "waiting.example.com",
        UpstreamScript::SecretUnreadableForever,
    );
    let cancel = CancellationToken::new();
    let task = tokio::spawn(run_reconcile(
        oagw.clone(),
        s2s_ctx(),
        targets(&[("waiting", "waiting.example.com")]),
        cancel.clone(),
    ));
    tokio::time::sleep(Duration::from_secs(7)).await;
    cancel.cancel();
    task.await.unwrap();
    let calls_at_stop = oagw.upstream_call_count();
    assert_eq!(calls_at_stop, 2, "retries at 2 s and 6 s");
    tokio::time::sleep(Duration::from_secs(600)).await;
    assert_eq!(oagw.upstream_call_count(), calls_at_stop);
}

// ── warning ──────────────────────────────────────────────────────────────────

#[traced_test]
#[tokio::test(start_paused = true)]
async fn one_warning_naming_the_pending_providers_follows_2_minutes_without_success() {
    let expect_warnings = |label: &str, expected: usize| {
        let label = label.to_owned();
        logs_assert(move |lines: &[&str]| {
            let found = lines
                .iter()
                .filter(|line| line.contains("WARN") && line.contains(label.as_str()))
                .count();
            if found == expected {
                Ok(())
            } else {
                Err(format!(
                    "expected {expected} WARN lines naming {label}, found {found}: {lines:#?}"
                ))
            }
        });
    };

    let oagw = RecordingOagw::accepting();
    oagw.script("slow.example.com", UpstreamScript::SecretUnreadableForever);
    let cancel = CancellationToken::new();
    let reconcile = run_reconcile(
        oagw.clone(),
        s2s_ctx(),
        targets(&[
            ("quick-provider", "quick.example.com"),
            ("slow-provider", "slow.example.com"),
        ]),
        cancel.clone(),
    );
    let driver = async {
        // no warning before 2 minutes have passed
        tokio::time::sleep(Duration::from_secs(119)).await;
        expect_warnings("slow-provider", 0);
        // one warning after them, naming only the entry that is still pending
        tokio::time::sleep(Duration::from_secs(10)).await;
        expect_warnings("slow-provider", 1);
        expect_warnings("quick-provider", 0);
        // and no second one while the entry stays pending
        tokio::time::sleep(Duration::from_secs(600)).await;
        expect_warnings("slow-provider", 1);
        cancel.cancel();
    };
    tokio::join!(reconcile, driver);
}
