#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Tests of `cpt-cf-mini-chat-algo-gear-foundation-reconcile-providers`
//! (virtual time: `start_paused`).
//!
//! Schedule under test: the first attempt is made one interval (2 s) after the
//! start; after every round the task waits the current interval and then doubles
//! it up to 60 s. Attempts therefore happen at 2, 4, 8, 16, 32, 64, 124, 184 s.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use toolkit_security::SecurityContext;
use tracing::Level;
use uuid::Uuid;

use super::{
    RECONCILE_INITIAL_INTERVAL, RECONCILE_MAX_INTERVAL, RECONCILE_WARN_AFTER, ReconcileSchedule,
    RetryFailure, TargetRetry, reconcile_providers,
};
use crate::config::{AuthConfig, StorageKind};
use crate::infra::llm::oagw_provisioning::ProvisionTarget;
use crate::test_doubles::{LogCapture, S2S_SUBJECT, s2s_ctx};

const CAUSE: &str = "secret not readable yet";

fn target(provider_id: &str) -> ProvisionTarget {
    ProvisionTarget {
        provider_id: provider_id.to_owned(),
        tenant_id: None,
        alias: format!("{provider_id}.example.com"),
        host: format!("{provider_id}.example.com"),
        port: 443,
        use_http: false,
        auth_plugin_type: None,
        auth_config: AuthConfig::default(),
        api_path: "/v1/responses".to_owned(),
        storage_kind: StorageKind::Openai,
        api_version: None,
    }
}

fn secs(n: u64) -> Duration {
    Duration::from_secs(n)
}

/// Fails the first `n` retries of a provider, then succeeds; `u32::MAX` fails forever.
struct ScriptedRetry {
    start: Instant,
    failures: Mutex<HashMap<String, u32>>,
    calls: Mutex<Vec<(Duration, String)>>,
    subjects: Mutex<Vec<Uuid>>,
    hang: bool,
}

impl ScriptedRetry {
    fn new(failures: &[(&str, u32)]) -> Arc<Self> {
        Arc::new(Self {
            start: Instant::now(),
            failures: Mutex::new(
                failures
                    .iter()
                    .map(|(id, n)| ((*id).to_owned(), *n))
                    .collect(),
            ),
            calls: Mutex::new(Vec::new()),
            subjects: Mutex::new(Vec::new()),
            hang: false,
        })
    }

    fn hanging() -> Arc<Self> {
        Arc::new(Self {
            start: Instant::now(),
            failures: Mutex::new(HashMap::new()),
            calls: Mutex::new(Vec::new()),
            subjects: Mutex::new(Vec::new()),
            hang: true,
        })
    }

    fn calls(&self) -> Vec<(Duration, String)> {
        self.calls.lock().unwrap().clone()
    }

    fn offsets_of(&self, provider: &str) -> Vec<Duration> {
        self.calls()
            .into_iter()
            .filter(|(_, id)| id == provider)
            .map(|(at, _)| at)
            .collect()
    }
}

#[async_trait]
impl TargetRetry for ScriptedRetry {
    async fn retry(
        &self,
        ctx: &SecurityContext,
        target: &ProvisionTarget,
    ) -> Result<(), RetryFailure> {
        self.calls
            .lock()
            .unwrap()
            .push((self.start.elapsed(), target.provider_id.clone()));
        self.subjects.lock().unwrap().push(ctx.subject_id());
        if self.hang {
            std::future::pending::<()>().await;
        }
        let mut failures = self.failures.lock().unwrap();
        match failures.get_mut(&target.provider_id) {
            Some(remaining) if *remaining > 0 => {
                if *remaining != u32::MAX {
                    *remaining -= 1;
                }
                Err(RetryFailure {
                    cause: CAUSE.to_owned(),
                })
            }
            _ => Ok(()),
        }
    }
}

fn spawn(
    retry: &Arc<ScriptedRetry>,
    pending: Vec<ProvisionTarget>,
    cancel: &CancellationToken,
    schedule: ReconcileSchedule,
) -> tokio::task::JoinHandle<Vec<ProvisionTarget>> {
    tokio::spawn(reconcile_providers(
        Arc::clone(retry) as Arc<dyn TargetRetry>,
        s2s_ctx(),
        pending,
        cancel.clone(),
        schedule,
    ))
}

// ---- schedule -------------------------------------------------------------------------

#[test]
fn the_default_schedule_is_two_seconds_doubling_to_sixty_with_a_two_minute_warning() {
    assert_eq!(RECONCILE_INITIAL_INTERVAL, secs(2));
    assert_eq!(RECONCILE_MAX_INTERVAL, secs(60));
    assert_eq!(RECONCILE_WARN_AFTER, secs(120));
    assert_eq!(
        ReconcileSchedule::default(),
        ReconcileSchedule {
            initial_interval: secs(2),
            max_interval: secs(60),
            warn_after: secs(120),
        }
    );
}

#[tokio::test(start_paused = true)]
async fn the_first_retry_waits_one_interval_after_the_start() {
    let retry = ScriptedRetry::new(&[("alpha", u32::MAX)]);
    let cancel = CancellationToken::new();
    let handle = spawn(
        &retry,
        vec![target("alpha")],
        &cancel,
        ReconcileSchedule::default(),
    );

    tokio::time::sleep(Duration::from_millis(1900)).await;
    assert!(retry.calls().is_empty(), "{:?}", retry.calls());
    tokio::time::sleep(Duration::from_millis(200)).await;

    assert_eq!(retry.offsets_of("alpha"), vec![secs(2)]);
    cancel.cancel();
    handle.await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn the_interval_doubles_up_to_sixty_seconds_and_stays_there() {
    let retry = ScriptedRetry::new(&[("alpha", u32::MAX)]);
    let cancel = CancellationToken::new();
    let handle = spawn(
        &retry,
        vec![target("alpha")],
        &cancel,
        ReconcileSchedule::default(),
    );

    tokio::time::sleep(secs(200)).await;
    cancel.cancel();
    handle.await.unwrap();

    assert_eq!(
        retry.offsets_of("alpha"),
        vec![
            secs(2),
            secs(4),
            secs(8),
            secs(16),
            secs(32),
            secs(64),
            secs(124),
            secs(184)
        ]
    );
}

#[tokio::test(start_paused = true)]
async fn a_custom_schedule_is_honoured() {
    let retry = ScriptedRetry::new(&[("alpha", u32::MAX)]);
    let cancel = CancellationToken::new();
    let schedule = ReconcileSchedule {
        initial_interval: secs(1),
        max_interval: secs(4),
        warn_after: secs(6),
    };
    let logs = LogCapture::start();
    let handle = spawn(&retry, vec![target("alpha")], &cancel, schedule);

    tokio::time::sleep(Duration::from_millis(13_500)).await;
    cancel.cancel();
    handle.await.unwrap();

    assert_eq!(
        retry.offsets_of("alpha"),
        vec![secs(1), secs(2), secs(4), secs(8), secs(12)]
    );
    assert_eq!(logs.at(Level::WARN).len(), 1, "{:?}", logs.at(Level::WARN));
}

// ---- inst-retry-target ---------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn a_successful_retry_removes_the_target_and_the_task_returns_when_none_is_pending() {
    let retry = ScriptedRetry::new(&[("alpha", 2)]);
    let cancel = CancellationToken::new();
    let started = Instant::now();
    let handle = spawn(
        &retry,
        vec![target("alpha"), target("beta")],
        &cancel,
        ReconcileSchedule::default(),
    );

    let pending = handle.await.unwrap();

    assert!(pending.is_empty(), "{pending:?}");
    assert_eq!(
        retry.calls(),
        vec![
            (secs(2), "alpha".to_owned()),
            (secs(2), "beta".to_owned()),
            (secs(4), "alpha".to_owned()),
            (secs(8), "alpha".to_owned()),
        ],
        "beta is not retried after it succeeded"
    );
    assert_eq!(
        started.elapsed(),
        secs(8),
        "returns as soon as the set is empty"
    );
}

#[tokio::test(start_paused = true)]
async fn the_s2s_context_is_passed_to_every_retry() {
    let retry = ScriptedRetry::new(&[("alpha", 1)]);
    let cancel = CancellationToken::new();
    let handle = spawn(
        &retry,
        vec![target("alpha")],
        &cancel,
        ReconcileSchedule::default(),
    );

    handle.await.unwrap();

    let subjects = retry.subjects.lock().unwrap().clone();
    assert_eq!(subjects, vec![S2S_SUBJECT, S2S_SUBJECT]);
}

#[tokio::test(start_paused = true)]
async fn an_empty_pending_set_returns_immediately_without_retrying() {
    let retry = ScriptedRetry::new(&[]);
    let cancel = CancellationToken::new();
    let started = Instant::now();

    let handle = spawn(&retry, Vec::new(), &cancel, ReconcileSchedule::default());
    let pending = handle.await.unwrap();

    assert!(pending.is_empty());
    assert!(retry.calls().is_empty());
    assert_eq!(started.elapsed(), Duration::ZERO);
}

#[tokio::test(start_paused = true)]
async fn a_failed_retry_keeps_the_target_and_logs_the_cause() {
    let retry = ScriptedRetry::new(&[("alpha", u32::MAX)]);
    let cancel = CancellationToken::new();
    let logs = LogCapture::start();
    let handle = spawn(
        &retry,
        vec![target("alpha")],
        &cancel,
        ReconcileSchedule::default(),
    );

    tokio::time::sleep(secs(5)).await;
    cancel.cancel();
    let pending = handle.await.unwrap();

    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].provider_id, "alpha");
    let mentions: Vec<_> = logs
        .records()
        .into_iter()
        .filter(|record| record.text().contains(CAUSE))
        .collect();
    assert!(!mentions.is_empty(), "{:?}", logs.records());
    assert!(
        mentions
            .iter()
            .any(|record| record.text().contains("alpha")),
        "the log names the provider: {mentions:?}"
    );
}

// ---- inst-warn-pending ------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn one_warning_naming_the_pending_providers_is_logged_after_two_minutes() {
    let retry = ScriptedRetry::new(&[("alpha", u32::MAX), ("beta", u32::MAX), ("gamma", 1)]);
    let cancel = CancellationToken::new();
    let logs = LogCapture::start();
    let handle = spawn(
        &retry,
        vec![target("alpha"), target("beta"), target("gamma")],
        &cancel,
        ReconcileSchedule::default(),
    );

    tokio::time::sleep(secs(100)).await;
    assert!(
        logs.at(Level::WARN).is_empty(),
        "no warning before two minutes: {:?}",
        logs.at(Level::WARN)
    );

    tokio::time::sleep(secs(150)).await;
    cancel.cancel();
    handle.await.unwrap();

    let warnings = logs.at(Level::WARN);
    assert_eq!(warnings.len(), 1, "exactly one warning: {warnings:?}");
    let text = warnings[0].text();
    assert!(text.contains("alpha"), "{text}");
    assert!(text.contains("beta"), "{text}");
    assert!(!text.contains("gamma"), "gamma recovered at 4 s: {text}");
}

#[tokio::test(start_paused = true)]
async fn no_warning_is_logged_when_everything_recovers_in_time() {
    let retry = ScriptedRetry::new(&[("alpha", 3)]);
    let cancel = CancellationToken::new();
    let logs = LogCapture::start();
    let handle = spawn(
        &retry,
        vec![target("alpha")],
        &cancel,
        ReconcileSchedule::default(),
    );

    let pending = handle.await.unwrap();

    assert!(pending.is_empty());
    assert!(
        logs.at(Level::WARN).is_empty(),
        "{:?}",
        logs.at(Level::WARN)
    );
}

// ---- inst-while-pending / cancellation -----------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn cancelling_while_waiting_stops_the_task_and_returns_the_pending_targets() {
    let retry = ScriptedRetry::new(&[("alpha", u32::MAX)]);
    let cancel = CancellationToken::new();
    let handle = spawn(
        &retry,
        vec![target("alpha")],
        &cancel,
        ReconcileSchedule::default(),
    );

    tokio::time::sleep(secs(3)).await;
    cancel.cancel();
    let pending = tokio::time::timeout(secs(1), handle)
        .await
        .expect("the task ends promptly after cancellation")
        .unwrap();
    tokio::time::sleep(secs(500)).await;

    assert_eq!(pending.len(), 1);
    assert_eq!(
        retry.offsets_of("alpha"),
        vec![secs(2)],
        "no attempt after the cancel"
    );
}

#[tokio::test(start_paused = true)]
async fn cancelling_during_a_retry_that_never_answers_stops_the_task() {
    let retry = ScriptedRetry::hanging();
    let cancel = CancellationToken::new();
    let handle = spawn(
        &retry,
        vec![target("alpha")],
        &cancel,
        ReconcileSchedule::default(),
    );

    tokio::time::sleep(secs(5)).await;
    cancel.cancel();
    let pending = tokio::time::timeout(secs(1), handle)
        .await
        .expect("the task does not wait for the hanging retry")
        .unwrap();

    assert_eq!(pending.len(), 1);
    assert_eq!(
        retry.calls().len(),
        1,
        "the retry was started before the cancel"
    );
}

#[tokio::test(start_paused = true)]
async fn an_already_cancelled_token_makes_no_attempt() {
    let retry = ScriptedRetry::new(&[("alpha", u32::MAX)]);
    let cancel = CancellationToken::new();
    cancel.cancel();

    let handle = spawn(
        &retry,
        vec![target("alpha")],
        &cancel,
        ReconcileSchedule::default(),
    );
    let pending = handle.await.unwrap();

    assert_eq!(pending.len(), 1);
    assert!(retry.calls().is_empty());
}
