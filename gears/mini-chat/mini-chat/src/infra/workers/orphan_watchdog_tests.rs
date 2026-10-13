#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Slice 2: the orphan watchdog
//! (`cpt-cf-mini-chat-algo-background-workers-run-watchdog`,
//! `-finalize-orphan`, `cpt-cf-mini-chat-dod-background-workers-orphan-watchdog`,
//! `cpt-cf-mini-chat-dod-background-workers-orphan-finalization`): the scan by
//! durable progress time, the orphan CAS with the bounded debit, the usage and
//! audit events of the same transaction, the unreserved and corrupt turns, the
//! race with the stream path and leader-only scheduling.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use futures::future::BoxFuture;
use mini_chat_sdk::{
    BillingOutcome, SettlementMethod, TerminalState, TurnAuditEvent, TurnAuditEventType,
    usage_dedupe_key,
};
use time::OffsetDateTime;
use tokio_util::sync::CancellationToken;
use tracing::Level;
use uuid::Uuid;

use super::{ORPHAN_WATCHDOG_TASK, OrphanWatchdog, ScanReport, WatchdogDeps};
use crate::chat_test_env::request_id;
use crate::domain::error::DomainError;
use crate::domain::service::quota::finalize::{FinalizeRequest, FinalizeResult};
use crate::domain::service::quota::test_support::{
    PolicyCall, PolicyStep, design_catalog, design_fixture, snapshot,
};
use crate::domain::service::workers::ORPHAN_SCAN_BATCH;
use crate::domain::service::workers::metrics::WorkerMetrics;
use crate::domain::service::workers::orphan::OrphanCandidate;
use crate::domain::service::workers::ports::OrphanStore;
use crate::domain::service::workers::test_support::{Metric, RecordingWorkerMetrics};
use crate::infra::workers::leader::{LeaderElector, LeaderRole, NoopLeaderElector};
use crate::quota_test_env::TurnSeed;
use crate::stream_test_env::{body, oa, rid};
use crate::test_doubles::{LogCapture, USER, USER_TENANT};
use crate::worker_test_env::{WorkerEnv, WorkerOptions};

fn noop() -> Arc<dyn LeaderElector> {
    Arc::new(NoopLeaderElector)
}

/// A standard turn (effective model `s`, reserve 1500, 500 output tokens,
/// floor 50, policy version 7) whose last progress is 50 minutes before the
/// fixture clock: stale for the default timeout of 300 s.
fn stale(n: u128, chat: Uuid) -> TurnSeed {
    TurnSeed::standard(n, chat).started("2026-03-10T11:00:00Z", Some("2026-03-10T11:10:00Z"))
}

/// A fixture with the DESIGN 5.10 buckets (1 500 000 reserved by the turn) and
/// one stale turn in the default chat.
async fn world() -> (WorkerEnv, Uuid) {
    let env = WorkerEnv::new().await;
    env.quota.seed_design_world(1_500_000, false).await;
    let turn = env.quota.seed_turn(&stale(1, env.chat)).await;
    (env, turn)
}

fn usage_dedupe(turn: Uuid, n: u128) -> String {
    usage_dedupe_key(USER_TENANT, turn, request_id(n))
}

// ================================================================================================
// the finalization
// ================================================================================================

#[test]
fn the_task_name_and_the_batch_size_are_the_documented_ones() {
    assert_eq!(ORPHAN_WATCHDOG_TASK, "orphan-watchdog");
    assert_eq!(ORPHAN_SCAN_BATCH, 100);
}

#[tokio::test]
async fn a_stale_turn_is_finalized_failed_orphan_timeout_with_completed_at_set() {
    let (env, turn) = world().await;

    let report = env.watchdog(noop()).scan_once().await;

    assert_eq!(
        report,
        ScanReport {
            selected: 1,
            won: 1,
            lost: 0,
            errors: 0
        }
    );
    assert_eq!(env.quota.turn_state(turn).await, "failed");
    assert_eq!(
        env.quota.turn_col(turn, "error_code").await,
        "'orphan_timeout'"
    );
    assert_eq!(
        env.quota.turn_col(turn, "completed_at").await,
        "'2026-03-10T12:00:00.250001Z'",
        "completed_at is the application clock of the finalization"
    );
    assert_eq!(
        env.quota.turn_col(turn, "assistant_message_id").await,
        "NULL"
    );
    assert_eq!(
        env.quota.turn_col(turn, "provider_response_id").await,
        "NULL"
    );
}

#[tokio::test]
async fn the_buckets_are_settled_once_with_the_bounded_estimated_debit() {
    let (env, _turn) = world().await;

    env.watchdog(noop()).scan_once().await;

    // reserve 1500, max output 500: 1000 estimated input tokens + the floor 50
    for (period, spent) in [("daily", 26_050_000), ("monthly", 241_050_000)] {
        let total = env.quota.total(period).await.unwrap();
        assert_eq!(
            (total.spent, total.reserved, total.calls),
            (spent, 0, 1),
            "{period}: the reserve is released and 1050 tokens are debited"
        );
    }
}

#[tokio::test]
async fn the_debit_is_the_estimate_below_the_reserve_and_the_rest_of_the_reserve_is_released() {
    let env = WorkerEnv::new().await;
    env.quota.seed_design_world(2_000_000, false).await;
    // reserve 2000, 800 output tokens: 1200 estimated input tokens + the floor 50
    env.quota
        .seed_turn(&stale(1, env.chat).with_reserve(2000, 800, 2_000_000))
        .await;

    env.watchdog(noop()).scan_once().await;

    let daily = env.quota.total("daily").await.unwrap();
    assert_eq!(
        (daily.spent, daily.reserved),
        (26_250_000, 0),
        "min(2000, 1200 + 50) = 1250 tokens are debited, the other 750 are released"
    );
    assert_eq!(env.usage().await[0].actual_credits_micro, 1_250_000);
}

#[tokio::test]
async fn one_usage_event_aborted_estimated_with_the_turn_dedupe_key_is_enqueued() {
    let (env, turn) = world().await;

    env.watchdog(noop()).scan_once().await;

    let events = env.usage().await;
    assert_eq!(events.len(), 1, "exactly one terminal billing event");
    let event = &events[0];
    assert_eq!(event.terminal_state, TerminalState::Failed);
    assert_eq!(event.billing_outcome, BillingOutcome::Aborted);
    assert_eq!(event.settlement_method, SettlementMethod::Estimated);
    assert_eq!(event.actual_credits_micro, 1_050_000);
    assert_eq!(event.effective_model, "s");
    assert_eq!(event.selected_model, "s");
    assert_eq!(event.policy_version_applied, 7);
    assert_eq!(event.dedupe_key, usage_dedupe(turn, 1));
    assert_eq!(event.turn_id, Some(turn));
    assert_eq!(event.user_id, Some(USER));
    let raw = env.quota.usage_rows().await[0].json();
    assert!(raw["usage"].is_null(), "no usage after a crash: {raw}");
}

#[tokio::test]
async fn one_turn_failed_audit_event_names_the_effective_model_and_an_unknown_quota_decision() {
    let (env, turn) = world().await;
    env.exec(
        "UPDATE chat_turns SET web_search_completed_count = 2, file_search_completed_count = 3",
    )
    .await;

    env.watchdog(noop()).scan_once().await;

    let audits: Vec<TurnAuditEvent> = env.audits().await;
    assert_eq!(audits.len(), 1);
    let audit = &audits[0];
    assert_eq!(audit.event_type, TurnAuditEventType::TurnFailed);
    assert_eq!(audit.turn_id, turn);
    assert_eq!(audit.request_id, request_id(1));
    assert_eq!(audit.tenant_id, USER_TENANT);
    assert_eq!(audit.requester_user_id, USER);
    assert_eq!(audit.selected_model, "s");
    assert_eq!(audit.effective_model, "s");
    assert_eq!(audit.policy_decisions.quota.decision, "unknown");
    assert_eq!(
        (audit.usage.input_tokens, audit.usage.output_tokens),
        (0, 0)
    );
    assert_eq!(audit.tool_calls.web_search, 2);
    assert_eq!(audit.tool_calls.file_search, 3);
    assert_eq!(
        audit.latency_ms, 3_600_250,
        "from started_at 11:00:00 to the finalization at 12:00:00.250001"
    );
    assert_eq!(audit.timestamp, env.now());
    assert!(audit.prompt.is_empty() && audit.response.is_empty());
}

#[tokio::test]
async fn no_message_row_is_written_and_no_summary_work_item_is_enqueued() {
    let (env, _turn) = world().await;
    let messages = env.count("messages").await;

    env.watchdog(noop()).scan_once().await;

    assert_eq!(env.count("messages").await, messages);
    assert!(env.summary_messages().await.is_empty());
    assert_eq!(env.count("thread_summaries").await, 0);
}

#[tokio::test]
async fn a_won_finalization_records_detected_finalized_aborted_and_one_scan_sample() {
    let (env, _turn) = world().await;

    env.watchdog(noop()).scan_once().await;

    assert_eq!(
        env.metrics.events(),
        vec![
            Metric::OrphanDetected,
            Metric::OrphanFinalized,
            Metric::OrphanStreamAborted,
            Metric::OrphanScan
        ]
    );
    assert_eq!(env.metrics.scan_seconds().len(), 1);
}

#[tokio::test]
async fn the_warning_names_the_chat_the_turn_and_the_request() {
    let logs = LogCapture::start();
    let (env, turn) = world().await;

    env.watchdog(noop()).scan_once().await;

    let warnings = logs.at_containing(Level::WARN, "turn finalized by orphan watchdog");
    assert_eq!(warnings.len(), 1, "{:?}", logs.records());
    let text = warnings[0].text();
    for id in [env.chat, turn, request_id(1)] {
        assert!(text.contains(&id.to_string()), "{text}");
    }
}

// ================================================================================================
// the candidates
// ================================================================================================

#[tokio::test]
async fn a_turn_with_an_old_started_at_and_recent_progress_is_never_a_candidate() {
    let env = WorkerEnv::new().await;
    env.quota.seed_design_world(1_500_000, false).await;
    let turn = env
        .quota
        .seed_turn(
            &TurnSeed::standard(1, env.chat)
                .started("2026-03-09T08:00:00Z", Some("2026-03-10T11:59:00Z")),
        )
        .await;

    let report = env.watchdog(noop()).scan_once().await;

    assert_eq!(report.selected, 0);
    assert_eq!(env.quota.turn_state(turn).await, "running");
    assert!(env.usage().await.is_empty());
}

#[tokio::test]
async fn a_null_progress_falls_back_to_started_at() {
    let env = WorkerEnv::new().await;
    env.quota.seed_design_world(3_000_000, false).await;
    let chat2 = env.quota.seed_chat(2).await;
    let old = env
        .quota
        .seed_turn(&TurnSeed::standard(1, env.chat).started("2026-03-10T11:00:00Z", None))
        .await;
    let recent = env
        .quota
        .seed_turn(&TurnSeed::standard(2, chat2).started("2026-03-10T11:58:00Z", None))
        .await;

    let report = env.watchdog(noop()).scan_once().await;

    assert_eq!(report.selected, 1);
    assert_eq!(env.quota.turn_state(old).await, "failed");
    assert_eq!(env.quota.turn_state(recent).await, "running");
}

#[tokio::test]
async fn the_cutoff_is_the_timeout_before_now_and_inclusive() {
    let env = WorkerEnv::with(WorkerOptions {
        orphan_timeout_secs: 600,
        ..WorkerOptions::default()
    })
    .await;
    env.quota.seed_design_world(3_000_000, false).await;
    let chat2 = env.quota.seed_chat(2).await;
    // now is 12:00:00.250001: the cutoff is 11:50:00.250001
    let exactly = env
        .quota
        .seed_turn(
            &TurnSeed::standard(1, env.chat)
                .started("2026-03-10T11:00:00Z", Some("2026-03-10T11:50:00.250001Z")),
        )
        .await;
    let just_after = env
        .quota
        .seed_turn(
            &TurnSeed::standard(2, chat2)
                .started("2026-03-10T11:00:00Z", Some("2026-03-10T11:50:00.250002Z")),
        )
        .await;

    let report = env.watchdog(noop()).scan_once().await;

    assert_eq!(report.selected, 1);
    assert_eq!(
        env.quota.turn_state(exactly).await,
        "failed",
        "at the cutoff is stale"
    );
    assert_eq!(env.quota.turn_state(just_after).await, "running");
}

#[tokio::test]
async fn soft_deleted_and_terminal_turns_are_not_touched() {
    let env = WorkerEnv::new().await;
    env.quota.seed_design_world(4_500_000, false).await;
    let chat2 = env.quota.seed_chat(2).await;
    let chat3 = env.quota.seed_chat(3).await;
    let mut deleted = stale(1, env.chat);
    deleted.deleted = true;
    let deleted = env.quota.seed_turn(&deleted).await;
    let done = env
        .quota
        .seed_turn(&stale(2, chat2).state("completed"))
        .await;
    let failed = env.quota.seed_turn(&stale(3, chat3).state("failed")).await;
    let before = env.quota.dump("chat_turns").await;

    let report = env.watchdog(noop()).scan_once().await;

    assert_eq!(report, ScanReport::default());
    assert_eq!(env.quota.dump("chat_turns").await, before);
    for turn in [deleted, done, failed] {
        assert_ne!(env.quota.turn_state(turn).await, "orphan");
    }
    assert!(env.usage().await.is_empty());
    assert!(env.metrics.events().contains(&Metric::OrphanScan));
    assert!(!env.metrics.events().contains(&Metric::OrphanDetected));
}

#[tokio::test]
async fn a_turn_of_a_soft_deleted_chat_is_finalized_and_billed_like_any_other() {
    let (env, turn) = world().await;
    env.soft_delete_chat(env.chat).await;

    let report = env.watchdog(noop()).scan_once().await;

    assert_eq!(report.won, 1);
    assert_eq!(env.quota.turn_state(turn).await, "failed");
    assert_eq!(env.usage().await.len(), 1);
    assert_eq!(env.quota.total("daily").await.unwrap().spent, 26_050_000);
}

#[tokio::test]
async fn the_scan_crosses_tenants() {
    let env = WorkerEnv::new().await;
    let other_tenant = crate::test_doubles::OTHER_TENANT;
    let other_user = crate::test_doubles::OTHER_USER;
    env.quota.seed_design_world(1_500_000, false).await;
    let chat = env.quota.seed_chat_of(2, other_tenant, other_user).await;
    let mut seed = stale(1, chat);
    seed.tenant = other_tenant;
    seed.requester = Some(other_user);
    let turn = env.quota.seed_turn(&seed).await;

    let report = env.watchdog(noop()).scan_once().await;

    assert_eq!(report.won, 1);
    assert_eq!(env.quota.turn_state(turn).await, "failed");
    let events = env.usage().await;
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].tenant_id, other_tenant);
    assert_eq!(env.audits().await[0].tenant_id, other_tenant);
}

#[tokio::test]
async fn a_scan_takes_at_most_100_candidates_oldest_progress_first_and_the_next_scan_the_rest() {
    let env = WorkerEnv::new().await;
    // 105 stale turns whose progress is `n` seconds after 11:00:00
    for n in 1..=105_u128 {
        let chat = if n == 1 {
            env.chat
        } else {
            env.quota.seed_chat(n).await
        };
        let progress = format!("2026-03-10T11:{:02}:{:02}Z", n.div_euclid(60) % 60, n % 60);
        env.quota
            .seed_turn(
                &TurnSeed::standard(n, chat)
                    .without_reserve()
                    .started("2026-03-10T10:00:00Z", Some(&progress)),
            )
            .await;
    }

    let first = env.watchdog(noop()).scan_once().await;
    let second = env.watchdog(noop()).scan_once().await;

    assert_eq!((first.selected, first.won), (100, 100));
    assert_eq!((second.selected, second.won), (5, 5));
    let remaining_at_second = env.metrics.count(Metric::OrphanFinalized);
    assert_eq!(remaining_at_second, 105);
    // the five newest were left for the second scan
    for n in 101..=105_u128 {
        assert_eq!(
            env.quota
                .turn_state(TurnSeed::standard(n, env.chat).id())
                .await,
            "failed"
        );
    }
}

// ================================================================================================
// the race with the stream path and with the progress writer
// ================================================================================================

type Hook = Box<dyn Fn() -> BoxFuture<'static, ()> + Send + Sync>;

/// Runs `hook` after the next scan of the wrapped store, before the finalization.
struct AfterScan {
    inner: Arc<dyn OrphanStore>,
    hook: Mutex<Option<Hook>>,
}

#[async_trait]
impl OrphanStore for AfterScan {
    async fn scan(
        &self,
        cutoff: OffsetDateTime,
        limit: u32,
    ) -> Result<Vec<OrphanCandidate>, DomainError> {
        let rows = self.inner.scan(cutoff, limit).await?;
        let hook = self.hook.lock().unwrap().take();
        if let Some(hook) = hook {
            hook().await;
        }
        Ok(rows)
    }

    async fn finalize(
        &self,
        request: &FinalizeRequest,
        audit: Option<&TurnAuditEvent>,
    ) -> Result<FinalizeResult, DomainError> {
        self.inner.finalize(request, audit).await
    }
}

fn after_scan(
    env: &WorkerEnv,
    hook: impl Fn() -> BoxFuture<'static, ()> + Send + Sync + 'static,
) -> OrphanWatchdog {
    let store = Arc::new(AfterScan {
        inner: env.orphan_store(),
        hook: Mutex::new(Some(Box::new(hook))),
    });
    env.watchdog_over(store, noop(), env.wall())
}

#[tokio::test]
async fn a_turn_whose_progress_is_refreshed_between_the_scan_and_the_cas_is_detected_but_not_finalized()
 {
    let (env, turn) = world().await;
    let db = env.sql();
    let watchdog = after_scan(&env, move || {
        let db = db.clone();
        Box::pin(async move {
            db.exec("UPDATE chat_turns SET last_progress_at = '2026-03-10T11:59:59Z'")
                .await;
        })
    });
    let quota_before = env.quota.quota_dump().await;

    let report = watchdog.scan_once().await;

    assert_eq!((report.selected, report.won, report.lost), (1, 0, 1));
    assert_eq!(env.quota.turn_state(turn).await, "running");
    assert_eq!(env.metrics.count(Metric::OrphanDetected), 1);
    assert_eq!(
        env.metrics.count(Metric::OrphanFinalized),
        0,
        "detected but not finalized"
    );
    assert_eq!(env.metrics.count(Metric::OrphanStreamAborted), 0);
    assert!(env.usage().await.is_empty());
    assert!(env.audits().await.is_empty());
    assert_eq!(
        env.quota.quota_dump().await,
        quota_before,
        "nothing is settled"
    );
}

#[tokio::test]
async fn a_turn_finalized_elsewhere_between_the_scan_and_the_cas_is_skipped_without_a_trace() {
    let (env, turn) = world().await;
    let db = env.sql();
    let watchdog = after_scan(&env, move || {
        let db = db.clone();
        Box::pin(async move {
            db.exec(
                "UPDATE chat_turns SET state = 'completed', completed_at = '2026-03-10T11:59:59Z'",
            )
            .await;
        })
    });

    let report = watchdog.scan_once().await;

    assert_eq!((report.won, report.lost), (0, 1));
    assert_eq!(env.quota.turn_state(turn).await, "completed");
    assert!(
        env.usage().await.is_empty(),
        "no settlement, no outbox message"
    );
    assert!(env.audits().await.is_empty());
    assert_eq!(env.metrics.count(Metric::OrphanFinalized), 0);
}

#[tokio::test]
async fn two_watchdogs_scanning_the_same_database_finalize_a_turn_once() {
    let (env, turn) = world().await;
    let (first, second) = (env.watchdog(noop()), env.watchdog(noop()));

    let (a, b) = tokio::join!(first.scan_once(), second.scan_once());

    assert_eq!(a.won + b.won, 1, "{a:?} {b:?}");
    assert_eq!(env.quota.turn_state(turn).await, "failed");
    assert_eq!(env.usage().await.len(), 1, "one usage event");
    assert_eq!(env.audits().await.len(), 1, "one audit event");
    assert_eq!(
        env.quota.total("daily").await.unwrap().spent,
        26_050_000,
        "settled once"
    );
    assert_eq!(env.metrics.count(Metric::OrphanFinalized), 1);
}

#[tokio::test]
async fn a_turn_the_watchdog_wins_while_the_provider_holds_the_stream_ends_with_stream_interrupted()
{
    let env = WorkerEnv::with(WorkerOptions {
        ping: Duration::from_millis(40),
        ..WorkerOptions::default()
    })
    .await;
    let _feed = env.gate();
    let request = rid(1);
    let mut stream = env.m.stream.open(env.chat, &body("Hi", request)).await;
    stream.next_frame().await.unwrap();
    assert_eq!(env.turn_status(request).await.json["state"], "running");
    env.advance(400);

    let report = env.watchdog(noop()).scan_once().await;

    assert_eq!(report.won, 1);
    let frames = stream.rest().await;
    assert_eq!(frames.last().unwrap().name, "error");
    assert_eq!(frames.last().unwrap().data["code"], "stream_interrupted");
    assert_eq!(
        env.turn_text(request, "error_code").await.as_deref(),
        Some("orphan_timeout")
    );
    assert_eq!(
        env.usage().await.len(),
        1,
        "the watchdog's usage event only"
    );
    assert_eq!(
        env.audits().await.len(),
        1,
        "the watchdog's audit event only"
    );
    assert_eq!(
        env.messages(env.chat).await.len(),
        1,
        "no assistant message"
    );
}

#[tokio::test]
async fn after_the_watchdog_the_turn_status_is_error_orphan_timeout_and_a_new_send_is_accepted() {
    let env = WorkerEnv::with(WorkerOptions {
        ping: Duration::from_millis(40),
        ..WorkerOptions::default()
    })
    .await;
    let _feed = env.gate();
    let request = rid(1);
    let mut stream = env.m.stream.open(env.chat, &body("Hi", request)).await;
    stream.next_frame().await.unwrap();
    // before: the chat is blocked
    let blocked = env.post(env.chat, &body("again", rid(9))).await;
    assert_eq!(blocked.status.as_u16(), 409, "{}", blocked.text);
    env.advance(400);

    env.watchdog(noop()).scan_once().await;

    let status = env.turn_status(request).await;
    assert_eq!(status.json["state"], "error");
    assert_eq!(status.json["error_code"], "orphan_timeout");
    assert!(status.json.get("assistant_message_id").is_none());
    stream.rest().await;
    env.script(&[oa::delta("ok"), oa::completed(5, 5)]);
    let next = env.stream(env.chat, &body("next", rid(2))).await;
    assert_eq!(
        next.names().last(),
        Some(&"done"),
        "the chat accepts a new send"
    );
    let reused = env.post(env.chat, &body("Hi", request)).await;
    assert_eq!(
        reused.status.as_u16(),
        409,
        "the abandoned request id is a conflict"
    );
}

#[tokio::test]
async fn when_the_stream_path_finalizes_first_the_watchdog_changes_nothing() {
    let env = WorkerEnv::new().await;
    env.script(&[oa::delta("hello"), oa::completed(10, 5)]);
    env.stream(env.chat, &body("Hi", rid(1))).await;
    let before = env.snapshot().await;
    let usage = env.usage().await.len();
    env.advance(1000);

    let report = env.watchdog(noop()).scan_once().await;

    assert_eq!(report, ScanReport::default());
    assert_eq!(env.snapshot().await, before);
    assert_eq!(env.usage().await.len(), usage);
}

// ================================================================================================
// unreserved and corrupt turns
// ================================================================================================

#[tokio::test]
async fn a_replacement_turn_left_running_with_null_preflight_columns_is_finalized_without_settlement()
 {
    let env = WorkerEnv::new().await;
    env.quota.seed_design_world(1_500_000, false).await;
    let turn = env
        .quota
        .seed_turn(&stale(1, env.chat).without_reserve())
        .await;
    let quota_before = env.quota.quota_dump().await;

    let report = env.watchdog(noop()).scan_once().await;

    assert_eq!(report.won, 1);
    assert_eq!(env.quota.turn_state(turn).await, "failed");
    assert_eq!(
        env.quota.turn_col(turn, "error_code").await,
        "'orphan_timeout'"
    );
    assert_eq!(
        env.quota.quota_dump().await,
        quota_before,
        "no quota_usage row changed"
    );
    let events = env.usage().await;
    assert_eq!(events.len(), 1);
    let event = &events[0];
    assert_eq!(event.billing_outcome, BillingOutcome::Aborted);
    assert_eq!(event.settlement_method, SettlementMethod::Estimated);
    assert_eq!(event.actual_credits_micro, 0);
    assert_eq!(event.effective_model, "");
    assert_eq!(event.selected_model, "");
    assert_eq!(event.policy_version_applied, 0);
    assert!(env.quota.usage_rows().await[0].json()["usage"].is_null());
    let audits = env.audits().await;
    assert_eq!(audits.len(), 1);
    assert_eq!(audits[0].event_type, TurnAuditEventType::TurnFailed);
    assert_eq!(
        (
            audits[0].selected_model.as_str(),
            audits[0].effective_model.as_str()
        ),
        ("", "")
    );
    assert_eq!(audits[0].policy_decisions.quota.decision, "unknown");
}

#[tokio::test]
async fn a_stale_turn_without_a_requester_is_finalized_with_no_events_and_is_not_rescanned() {
    let logs = LogCapture::start();
    let env = WorkerEnv::new().await;
    env.quota.seed_design_world(1_500_000, false).await;
    let turn = env
        .quota
        .seed_turn(&stale(1, env.chat).without_requester())
        .await;

    let first = env.watchdog(noop()).scan_once().await;
    let second = env.watchdog(noop()).scan_once().await;

    assert_eq!(first.won, 1);
    assert_eq!(
        second.selected, 0,
        "the turn is terminal: it is not rescanned forever"
    );
    assert_eq!(env.quota.turn_state(turn).await, "failed");
    assert!(env.usage().await.is_empty(), "no usage event");
    assert!(env.audits().await.is_empty(), "no audit event");
    assert!(
        !logs.at(Level::ERROR).is_empty(),
        "the corrupt row is logged at error: {:?}",
        logs.records()
    );
}

#[tokio::test]
async fn a_failed_snapshot_fetch_finalizes_the_turn_with_the_deterministic_estimated_fallback() {
    let logs = LogCapture::start();
    let (env, turn) = world().await;
    env.quota.policy.fail(PolicyStep::Snapshot);

    let report = env.watchdog(noop()).scan_once().await;

    assert_eq!(report.won, 1);
    assert_eq!(env.quota.turn_state(turn).await, "failed");
    let events = env.usage().await;
    assert_eq!(events[0].settlement_method, SettlementMethod::Estimated);
    assert_eq!(
        events[0].actual_credits_micro, 1_500_000,
        "the fallback commits the reserved credits"
    );
    assert_eq!(env.quota.total("daily").await.unwrap().spent, 26_500_000);
    assert!(!logs.at(Level::ERROR).is_empty());
}

#[tokio::test]
async fn the_snapshot_of_a_version_is_fetched_once_per_distinct_version_per_scan() {
    let env = WorkerEnv::new().await;
    env.quota.seed_design_world(4_500_000, false).await;
    env.quota.policy.add_snapshot(snapshot(9, design_catalog()));
    let chats = [
        env.chat,
        env.quota.seed_chat(2).await,
        env.quota.seed_chat(3).await,
    ];
    for (n, (chat, version)) in (1..).zip(chats.into_iter().zip([7, 7, 9])) {
        env.quota
            .seed_turn(&stale(n, chat).applied_version(version))
            .await;
    }

    let report = env.watchdog(noop()).scan_once().await;

    assert_eq!(report.won, 3);
    let calls = env.quota.policy.calls();
    let snapshots = calls
        .iter()
        .filter(|call| matches!(call, PolicyCall::Snapshot(..)))
        .count();
    assert_eq!(snapshots, 2, "versions 7 and 9, once each: {calls:?}");
    assert!(calls.contains(&PolicyCall::Snapshot(USER, 7)));
    assert!(calls.contains(&PolicyCall::Snapshot(USER, 9)));
}

fn snapshot_calls(env: &WorkerEnv) -> Vec<PolicyCall> {
    env.quota
        .policy
        .calls()
        .into_iter()
        .filter(|call| matches!(call, PolicyCall::Snapshot(..)))
        .collect()
}

#[tokio::test]
async fn two_tenants_at_the_same_version_each_settle_with_their_own_snapshot() {
    use crate::domain::service::quota::test_support::model;
    use crate::quota_test_env::BucketSeed;
    use crate::test_doubles::{OTHER_TENANT, OTHER_USER};
    use mini_chat_sdk::ModelTier;

    let env = WorkerEnv::new().await;
    env.quota.seed_design_world(1_500_000, false).await;
    for (period, spent) in [("daily", 25_000_000), ("monthly", 240_000_000)] {
        env.quota
            .seed_bucket(&BucketSeed::total(period, spent, 1_500_000).of(OTHER_TENANT, OTHER_USER))
            .await;
    }
    // the same version 7, a different catalog: model `s` costs half a credit per 1K tokens
    let cheaper = vec![
        model("s", ModelTier::Standard)
            .mult(500_000_000, 500_000_000)
            .build(),
    ];
    env.quota
        .policy
        .add_user_snapshot(OTHER_USER, snapshot(7, cheaper));
    let other_chat = env.quota.seed_chat_of(2, OTHER_TENANT, OTHER_USER).await;
    let own = env.quota.seed_turn(&stale(1, env.chat)).await;
    let mut seed = stale(2, other_chat);
    seed.tenant = OTHER_TENANT;
    seed.requester = Some(OTHER_USER);
    let other = env.quota.seed_turn(&seed).await;

    let report = env.watchdog(noop()).scan_once().await;

    assert_eq!(report.won, 2);
    let calls = snapshot_calls(&env);
    assert_eq!(calls.len(), 2, "one fetch per tenant: {calls:?}");
    assert!(calls.contains(&PolicyCall::Snapshot(USER, 7)));
    assert!(calls.contains(&PolicyCall::Snapshot(OTHER_USER, 7)));
    let events = env.usage().await;
    let credits = |turn: Uuid| {
        events
            .iter()
            .find(|event| event.turn_id == Some(turn))
            .expect("one usage event per turn")
            .actual_credits_micro
    };
    assert_eq!(credits(own), 1_050_000, "the catalog of the first tenant");
    assert_eq!(credits(other), 525_000, "the catalog of the second tenant");
    let debited = |tenant, user| {
        let env = &env;
        async move {
            env.quota
                .bucket_of(tenant, user, "daily", "2026-03-10", "total")
                .await
                .unwrap()
                .spent
        }
    };
    assert_eq!(debited(USER_TENANT, USER).await, 26_050_000);
    assert_eq!(debited(OTHER_TENANT, OTHER_USER).await, 25_525_000);
}

#[tokio::test]
async fn a_transient_snapshot_failure_is_not_cached_and_the_next_candidate_fetches_again() {
    let env = WorkerEnv::new().await;
    env.quota.seed_design_world(4_500_000, false).await;
    env.quota.policy.fail_once(PolicyStep::Snapshot);
    let chats = [
        env.chat,
        env.quota.seed_chat(2).await,
        env.quota.seed_chat(3).await,
    ];
    // oldest progress first: turn 1 meets the failure, turns 2 and 3 the plugin
    let progress = [
        "2026-03-10T11:05:00Z",
        "2026-03-10T11:10:00Z",
        "2026-03-10T11:12:00Z",
    ];
    let mut turns = Vec::new();
    for (n, (chat, at)) in (1..).zip(chats.into_iter().zip(progress)) {
        let seed = TurnSeed::standard(n, chat).started("2026-03-10T11:00:00Z", Some(at));
        turns.push(env.quota.seed_turn(&seed).await);
    }

    let report = env.watchdog(noop()).scan_once().await;

    assert_eq!(report.won, 3);
    let calls = snapshot_calls(&env);
    assert_eq!(
        calls.len(),
        2,
        "the failed fetch, the retry of the next candidate, then the cache: {calls:?}"
    );
    let events = env.usage().await;
    let credits = |turn: Uuid| {
        events
            .iter()
            .find(|event| event.turn_id == Some(turn))
            .unwrap()
            .actual_credits_micro
    };
    assert_eq!(
        credits(turns[0]),
        1_500_000,
        "the fallback commits the reserve"
    );
    assert_eq!(credits(turns[1]), 1_050_000);
    assert_eq!(credits(turns[2]), 1_050_000);
}

#[tokio::test]
async fn the_same_tenant_and_version_fetch_the_snapshot_once_per_scan() {
    let env = WorkerEnv::new().await;
    env.quota.seed_design_world(4_500_000, false).await;
    let chats = [
        env.chat,
        env.quota.seed_chat(2).await,
        env.quota.seed_chat(3).await,
    ];
    for (n, chat) in (1..).zip(chats) {
        env.quota.seed_turn(&stale(n, chat)).await;
    }

    let report = env.watchdog(noop()).scan_once().await;

    assert_eq!(report.won, 3);
    assert_eq!(snapshot_calls(&env), vec![PolicyCall::Snapshot(USER, 7)]);
    // a second scan has nothing left to fetch for, a later turn fetches anew
    let chat = env.quota.seed_chat(4).await;
    env.quota.seed_turn(&stale(4, chat)).await;
    env.watchdog(noop()).scan_once().await;
    assert_eq!(
        snapshot_calls(&env).len(),
        2,
        "the cache lives for one scan"
    );
}

// ================================================================================================
// leadership and the loop
// ================================================================================================

/// An elector that leads the roles in its set only.
struct Roles(Vec<LeaderRole>);

impl LeaderElector for Roles {
    fn is_leader(&self, role: LeaderRole) -> bool {
        self.0.contains(&role)
    }

    fn name(&self) -> &'static str {
        "roles"
    }
}

/// An elector that is the leader for its first `n` questions.
struct LeaderFor(usize, AtomicUsize);

impl LeaderElector for LeaderFor {
    fn is_leader(&self, _role: LeaderRole) -> bool {
        self.1.fetch_add(1, Ordering::SeqCst) < self.0
    }

    fn name(&self) -> &'static str {
        "leader-for"
    }
}

#[tokio::test]
async fn a_follower_scans_nothing_and_a_leader_of_the_watchdog_role_does() {
    let (env, turn) = world().await;

    let follower: Arc<dyn LeaderElector> = Arc::new(Roles(vec![LeaderRole::UploadReaper]));
    assert_eq!(
        env.watchdog(follower).tick().await,
        None,
        "leading the reaper role is not leading the watchdog role"
    );
    assert_eq!(env.quota.turn_state(turn).await, "running");
    assert!(
        env.metrics.events().is_empty(),
        "a follower records nothing"
    );

    let leader: Arc<dyn LeaderElector> = Arc::new(Roles(vec![LeaderRole::OrphanWatchdog]));
    let report = env.watchdog(leader).tick().await.expect("a scan");
    assert_eq!(report.won, 1);
}

#[tokio::test]
async fn a_scan_ends_when_the_leadership_is_lost_and_leaves_the_rest_to_the_next_leader() {
    let env = WorkerEnv::new().await;
    env.quota.seed_design_world(4_500_000, false).await;
    for n in 1..=3_u128 {
        let chat = if n == 1 {
            env.chat
        } else {
            env.quota.seed_chat(n).await
        };
        env.quota
            .seed_turn(&stale(n, chat).started(
                "2026-03-10T11:00:00Z",
                Some(&format!("2026-03-10T11:0{n}:00Z")),
            ))
            .await;
    }
    // leader for the tick and for the first candidate only
    let elector: Arc<dyn LeaderElector> = Arc::new(LeaderFor(2, AtomicUsize::new(0)));

    let report = env.watchdog(elector).tick().await.expect("a scan");

    assert_eq!(report.selected, 3);
    assert_eq!(
        report.won, 1,
        "the second candidate was left for the next leader"
    );
    assert_eq!(env.metrics.count(Metric::OrphanDetected), 2);
    assert_eq!(env.metrics.count(Metric::OrphanFinalized), 1);
    assert_eq!(
        env.quota
            .turn_state(TurnSeed::standard(2, env.chat).id())
            .await,
        "running"
    );
    assert_eq!(
        env.quota
            .turn_state(TurnSeed::standard(3, env.chat).id())
            .await,
        "running"
    );
}

/// A store with scripted candidates and no database.
#[derive(Default)]
struct FakeStore {
    scans: AtomicUsize,
}

#[async_trait]
impl OrphanStore for FakeStore {
    async fn scan(
        &self,
        _cutoff: OffsetDateTime,
        _limit: u32,
    ) -> Result<Vec<OrphanCandidate>, DomainError> {
        self.scans.fetch_add(1, Ordering::SeqCst);
        Ok(Vec::new())
    }

    async fn finalize(
        &self,
        _request: &FinalizeRequest,
        _audit: Option<&TurnAuditEvent>,
    ) -> Result<FinalizeResult, DomainError> {
        Ok(FinalizeResult::Lost)
    }
}

fn loop_watchdog(store: &Arc<FakeStore>, elector: Arc<dyn LeaderElector>) -> Arc<OrphanWatchdog> {
    let fixture = design_fixture();
    let config = crate::config::OrphanWatchdogConfig {
        scan_interval_secs: 60,
        ..crate::config::OrphanWatchdogConfig::default()
    };
    Arc::new(OrphanWatchdog::new(WatchdogDeps {
        store: Arc::clone(store) as Arc<dyn OrphanStore>,
        quota: Arc::new(fixture.service),
        elector,
        metrics: RecordingWorkerMetrics::new() as Arc<dyn WorkerMetrics>,
        config,
        clock: crate::domain::service::quota::test_support::fixed_clock(
            crate::domain::service::quota::test_support::noon(),
        ),
    }))
}

async fn settle() {
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }
}

#[tokio::test(start_paused = true)]
async fn the_loop_scans_once_per_interval_while_leader_and_ends_with_the_stop_token() {
    let store = Arc::new(FakeStore::default());
    let stop = CancellationToken::new();
    let task = tokio::spawn(loop_watchdog(&store, noop()).run(stop.clone()));

    settle().await;
    assert_eq!(
        store.scans.load(Ordering::SeqCst),
        0,
        "the first scan comes after one interval"
    );
    tokio::time::advance(Duration::from_secs(59)).await;
    settle().await;
    assert_eq!(store.scans.load(Ordering::SeqCst), 0);
    tokio::time::advance(Duration::from_secs(2)).await;
    settle().await;
    assert_eq!(store.scans.load(Ordering::SeqCst), 1);
    tokio::time::advance(Duration::from_secs(60)).await;
    settle().await;
    assert_eq!(store.scans.load(Ordering::SeqCst), 2);

    stop.cancel();
    tokio::time::timeout(Duration::from_secs(1), task)
        .await
        .expect("the loop ends without waiting for the interval")
        .unwrap();
    assert_eq!(store.scans.load(Ordering::SeqCst), 2);
}

#[tokio::test(start_paused = true)]
async fn the_loop_skips_a_round_without_leadership_and_resumes_when_it_is_gained() {
    let store = Arc::new(FakeStore::default());
    let toggle = Arc::new(Toggle(std::sync::atomic::AtomicBool::new(false)));
    let stop = CancellationToken::new();
    let task = tokio::spawn(
        loop_watchdog(&store, Arc::clone(&toggle) as Arc<dyn LeaderElector>).run(stop.clone()),
    );

    tokio::time::advance(Duration::from_secs(130)).await;
    settle().await;
    assert_eq!(
        store.scans.load(Ordering::SeqCst),
        0,
        "a follower never reads the table"
    );
    toggle.0.store(true, Ordering::SeqCst);
    tokio::time::advance(Duration::from_secs(61)).await;
    settle().await;
    assert!(store.scans.load(Ordering::SeqCst) >= 1);

    stop.cancel();
    task.await.unwrap();
}

struct Toggle(std::sync::atomic::AtomicBool);

impl LeaderElector for Toggle {
    fn is_leader(&self, _role: LeaderRole) -> bool {
        self.0.load(Ordering::SeqCst)
    }

    fn name(&self) -> &'static str {
        "toggle"
    }
}

#[test]
fn the_request_id_helper_matches_the_seeded_requests() {
    assert_eq!(TurnSeed::standard(1, Uuid::nil()).request(), request_id(1));
}
