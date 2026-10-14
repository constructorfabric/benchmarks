//! The orphan watchdog over in-memory `SQLite`, the real quota service and the shared outbox,
//! with a controllable application clock: stale and fresh turns, the `started_at` fallback,
//! soft-deleted turns, the 100-candidate cap with the oldest first, turns without reserve columns,
//! the estimated settlement with the `aborted` usage event and the `turn_failed` audit event of
//! the orphan path, a non-leader, a failing transaction retried by the next scan, the metrics
//! and the worker loop.
//!
//! Slice S2-provider-path of `cpt-cf-mini-chat-feature-message-streaming`: algorithm
//! `orphan-watchdog`; states `turn-lifecycle`; definitions of done
//! `cpt-cf-mini-chat-dod-message-streaming-orphan-watchdog` and
//! `cpt-cf-mini-chat-dod-message-streaming-turn-lifecycle`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use time::Duration as TimeDuration;

use crate::domain::quota::RequestFacts;
use crate::infra::leader::{LeaderElector, LeaderRole};
use crate::infra::orphan_watchdog::{SCAN_BATCH, ScanReport};
use crate::test_support::chats::{fail_outbox_inserts, message_rows};
use crate::test_support::quota::{
    FixedClock, STANDARD, TurnSeed, all_quota_rows, default_now, plain_facts, seed_turn,
    touch_turn, turn_row,
};
use crate::test_support::raw_exec;
use crate::test_support::send::{SendRig, SendRigOptions, send_rig_with};

const TIMEOUT: Duration = Duration::from_secs(300);

struct Follower;

#[async_trait]
impl LeaderElector for Follower {
    async fn is_leader(&self, _role: LeaderRole) -> bool {
        false
    }
}

async fn rig_at_start() -> (SendRig, Arc<FixedClock>) {
    let clock = FixedClock::at(default_now());
    let rig = send_rig_with(
        SendRigOptions::new()
            .with_clock(clock.clone())
            .with_metrics(),
    )
    .await;
    (rig, clock)
}

fn facts() -> RequestFacts {
    plain_facts()
}

fn later(seconds: i64) -> time::OffsetDateTime {
    default_now() + TimeDuration::seconds(seconds)
}

async fn reserved(rig: &SendRig) -> i64 {
    all_quota_rows(&rig.quota.db)
        .await
        .iter()
        .map(|row| row.reserved)
        .sum()
}

// ── stale turns are finalized ────────────────────────────────────────────────

#[tokio::test]
async fn a_stale_running_turn_is_failed_with_orphan_timeout_and_settled_estimated() {
    let (rig, clock) = rig_at_start().await;
    let chat = rig.quota.chat_a1(STANDARD).await;
    let booked = rig.quota.book(&chat, STANDARD, facts()).await;
    assert!(reserved(&rig).await > 0);

    clock.set(later(301));
    let report = rig.watchdog(TIMEOUT).scan_once().await.unwrap();
    assert_eq!(
        report,
        ScanReport {
            scanned: true,
            candidates: 1,
            finalized: 1,
            skipped: 0,
            failed: 0,
        }
    );

    let turn = turn_row(&rig.quota.db, booked.turn_id).await;
    assert_eq!(turn.state, "failed");
    assert_eq!(turn.error_code.as_deref(), Some("orphan_timeout"));
    assert_eq!(turn.assistant_message_id, None);
    assert_eq!(turn.completed_at, Some(later(301)));
    // the estimated settlement released the reserve
    assert_eq!(reserved(&rig).await, 0);

    // one `aborted` usage event with the effective model as the selected model
    let events = rig.events().await;
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!(events[0].terminal_state, "failed");
    assert_eq!(events[0].billing_outcome, "aborted");
    assert_eq!(events[0].settlement_method, "estimated");
    assert_eq!(events[0].usage, None);
    assert_eq!(events[0].effective_model, STANDARD);
    assert_eq!(events[0].selected_model, STANDARD);

    // one `turn_failed` audit event of the orphan path
    let audits = rig.audit_payloads().await;
    assert_eq!(audits.len(), 1, "{audits:?}");
    assert_eq!(audits[0]["event_type"], "turn_failed");
    assert_eq!(audits[0]["terminal_state"], "failed");
    assert_eq!(audits[0]["error_code"], "orphan_timeout");
    assert_eq!(audits[0]["selected_model"], STANDARD);
    assert_eq!(audits[0]["effective_model"], STANDARD);
    assert_eq!(audits[0]["quota_decision"], "unknown");
}

#[tokio::test]
async fn the_watchdog_never_creates_or_changes_messages() {
    let (rig, clock) = rig_at_start().await;
    let chat = rig.quota.chat_a1(STANDARD).await;
    rig.quota.book(&chat, STANDARD, facts()).await;
    let before = message_rows(&rig.quota.db, chat.id).await.len();
    clock.set(later(1000));
    rig.watchdog(TIMEOUT).scan_once().await.unwrap();
    assert_eq!(message_rows(&rig.quota.db, chat.id).await.len(), before);
}

#[tokio::test]
async fn the_chat_accepts_a_new_turn_after_the_orphan_failure() {
    let (rig, clock) = rig_at_start().await;
    let chat = rig.quota.chat_a1(STANDARD).await;
    rig.quota.book(&chat, STANDARD, facts()).await;
    // the parallel-turn guard holds while the stale turn runs
    let blocked = rig
        .stream(chat.id, &serde_json::json!({ "content": "again" }))
        .await;
    assert_eq!(
        blocked.status,
        axum::http::StatusCode::CONFLICT,
        "{}",
        blocked.text
    );
    clock.set(later(301));
    rig.watchdog(TIMEOUT).scan_once().await.unwrap();
    let accepted = rig
        .stream(chat.id, &serde_json::json!({ "content": "again" }))
        .await;
    assert_eq!(
        accepted.status,
        axum::http::StatusCode::OK,
        "{}",
        accepted.text
    );
}

// ── candidates ───────────────────────────────────────────────────────────────

#[tokio::test]
async fn a_turn_with_recent_progress_is_left_alone() {
    let (rig, clock) = rig_at_start().await;
    let chat = rig.quota.chat_a1(STANDARD).await;
    let booked = rig.quota.book(&chat, STANDARD, facts()).await;
    clock.set(later(1000));
    // progress 10 s before the scan
    touch_turn(&rig.quota.db, booked.turn_id, Some(later(990)), None).await;
    let report = rig.watchdog(TIMEOUT).scan_once().await.unwrap();
    assert_eq!(report.candidates, 0, "{report:?}");
    assert_eq!(
        turn_row(&rig.quota.db, booked.turn_id).await.state,
        "running"
    );
    assert!(reserved(&rig).await > 0);
    assert!(rig.events().await.is_empty());
}

#[tokio::test]
async fn a_null_last_progress_falls_back_to_started_at() {
    let (rig, clock) = rig_at_start().await;
    let chat = rig.quota.chat_a1(STANDARD).await;
    let fresh = rig.quota.book(&chat, STANDARD, facts()).await;
    // `book` leaves `last_progress_at` NULL: only `started_at` can make it stale
    assert_eq!(
        turn_row(&rig.quota.db, fresh.turn_id)
            .await
            .last_progress_at,
        None
    );
    clock.set(later(299));
    assert_eq!(
        rig.watchdog(TIMEOUT).scan_once().await.unwrap().candidates,
        0
    );
    clock.set(later(300));
    assert_eq!(
        rig.watchdog(TIMEOUT).scan_once().await.unwrap().finalized,
        1
    );
}

#[tokio::test]
async fn soft_deleted_and_terminal_turns_are_skipped() {
    let (rig, clock) = rig_at_start().await;
    let chat = rig.quota.chat_a1(STANDARD).await;
    let deleted = rig.quota.book(&chat, STANDARD, facts()).await;
    touch_turn(&rig.quota.db, deleted.turn_id, None, Some(default_now())).await;
    // a terminal turn of another chat
    let other = rig.quota.chat_a1(STANDARD).await;
    let mut done = TurnSeed::running(&other, default_now());
    done.state = "completed";
    seed_turn(&rig.quota.db, &done).await;
    clock.set(later(5000));
    let report = rig.watchdog(TIMEOUT).scan_once().await.unwrap();
    assert_eq!(report.candidates, 0, "{report:?}");
    assert_eq!(
        turn_row(&rig.quota.db, deleted.turn_id).await.state,
        "running"
    );
    assert_eq!(turn_row(&rig.quota.db, done.id).await.state, "completed");
}

#[tokio::test]
async fn a_scan_handles_at_most_100_turns_oldest_progress_first() {
    let (rig, clock) = rig_at_start().await;
    let mut ids = Vec::new();
    for index in 0..=SCAN_BATCH {
        let chat = rig.quota.chat_a1(STANDARD).await;
        // turn `index` started `index` seconds after the default instant, so the last is newest
        let started = default_now() + TimeDuration::seconds(i64::try_from(index).unwrap());
        ids.push(seed_turn(&rig.quota.db, &TurnSeed::running(&chat, started)).await);
    }
    assert_eq!(ids.len(), SCAN_BATCH + 1);
    clock.set(later(10_000));
    let watchdog = rig.watchdog(TIMEOUT);
    let first = watchdog.scan_once().await.unwrap();
    assert_eq!(
        (first.candidates, first.finalized),
        (SCAN_BATCH, SCAN_BATCH),
        "{first:?}"
    );
    // the newest one is left for the next scan
    let newest = ids.last().copied().unwrap();
    assert_eq!(turn_row(&rig.quota.db, newest).await.state, "running");
    for id in &ids[..SCAN_BATCH] {
        assert_eq!(turn_row(&rig.quota.db, *id).await.state, "failed");
    }
    let second = watchdog.scan_once().await.unwrap();
    assert_eq!((second.candidates, second.finalized), (1, 1), "{second:?}");
    assert_eq!(turn_row(&rig.quota.db, newest).await.state, "failed");
}

// ── turns without a reserve ──────────────────────────────────────────────────

#[tokio::test]
async fn a_turn_without_reserve_columns_gets_the_placeholder_event_and_no_bucket_change() {
    let (rig, clock) = rig_at_start().await;
    let chat = rig.quota.chat_a1(STANDARD).await;
    let id = seed_turn(&rig.quota.db, &TurnSeed::running(&chat, default_now())).await;
    clock.set(later(301));
    let report = rig.watchdog(TIMEOUT).scan_once().await.unwrap();
    assert_eq!(report.finalized, 1, "{report:?}");
    let turn = turn_row(&rig.quota.db, id).await;
    assert_eq!(turn.state, "failed");
    assert_eq!(turn.error_code.as_deref(), Some("orphan_timeout"));
    assert!(all_quota_rows(&rig.quota.db).await.is_empty());
    let events = rig.events().await;
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!(events[0].billing_outcome, "aborted");
    assert_eq!(events[0].settlement_method, "estimated");
    assert_eq!(events[0].actual_credits_micro, 0);
    assert_eq!(events[0].usage, None);
    assert_eq!(events[0].effective_model, "");
    assert_eq!(events[0].selected_model, "");
    assert_eq!(events[0].policy_version_applied, 0);
    let audits = rig.audit_payloads().await;
    assert_eq!(audits.len(), 1);
    assert_eq!(audits[0]["selected_model"], "");
    assert_eq!(audits[0]["quota_decision"], "unknown");
}

#[tokio::test]
async fn a_turn_without_a_requester_is_finalized_without_a_bucket_change() {
    let (rig, clock) = rig_at_start().await;
    let chat = rig.quota.chat_a1(STANDARD).await;
    let id = seed_turn(
        &rig.quota.db,
        &TurnSeed::running(&chat, default_now()).without_user(),
    )
    .await;
    clock.set(later(301));
    assert_eq!(
        rig.watchdog(TIMEOUT).scan_once().await.unwrap().finalized,
        1
    );
    assert_eq!(turn_row(&rig.quota.db, id).await.state, "failed");
    assert!(all_quota_rows(&rig.quota.db).await.is_empty());
    assert_eq!(rig.events().await.len(), 1);
}

// ── leadership and failures ──────────────────────────────────────────────────

#[tokio::test]
async fn a_non_leader_scans_nothing_and_claims_nothing() {
    let clock = FixedClock::at(default_now());
    let rig = send_rig_with(
        SendRigOptions::new()
            .with_clock(clock.clone())
            .with_elector(Arc::new(Follower)),
    )
    .await;
    let chat = rig.quota.chat_a1(STANDARD).await;
    let booked = rig.quota.book(&chat, STANDARD, facts()).await;
    clock.set(later(5000));
    let report = rig.watchdog(TIMEOUT).scan_once().await.unwrap();
    assert_eq!(report, ScanReport::default());
    assert_eq!(
        turn_row(&rig.quota.db, booked.turn_id).await.state,
        "running"
    );
    assert!(rig.events().await.is_empty());
}

#[tokio::test]
async fn a_failing_transaction_leaves_the_turn_running_and_the_next_scan_retries_it() {
    let (rig, clock) = rig_at_start().await;
    let chat = rig.quota.chat_a1(STANDARD).await;
    let booked = rig.quota.book(&chat, STANDARD, facts()).await;
    clock.set(later(301));
    fail_outbox_inserts(&rig.quota.db).await;
    let watchdog = rig.watchdog(TIMEOUT);
    let failed = watchdog.scan_once().await.unwrap();
    assert_eq!(
        (failed.candidates, failed.finalized, failed.failed),
        (1, 0, 1),
        "{failed:?}"
    );
    // rolled back: still running, reserve intact, nothing enqueued
    assert_eq!(
        turn_row(&rig.quota.db, booked.turn_id).await.state,
        "running"
    );
    assert!(reserved(&rig).await > 0);
    assert!(rig.audit_payloads().await.is_empty());

    raw_exec(&rig.quota.db.raw, "DROP TRIGGER fail_outbox_insert")
        .await
        .unwrap();
    let retried = watchdog.scan_once().await.unwrap();
    assert_eq!(retried.finalized, 1, "{retried:?}");
    assert_eq!(
        turn_row(&rig.quota.db, booked.turn_id).await.state,
        "failed"
    );
}

#[tokio::test]
async fn one_failing_candidate_does_not_stop_the_scan() {
    let (rig, clock) = rig_at_start().await;
    // two stale turns; the first candidate's finalization fails (a poisoned row), the second works
    let first_chat = rig.quota.chat_a1(STANDARD).await;
    let second_chat = rig.quota.chat_a1(STANDARD).await;
    let first = seed_turn(
        &rig.quota.db,
        &TurnSeed::running(&first_chat, default_now()),
    )
    .await;
    let second = seed_turn(
        &rig.quota.db,
        &TurnSeed::running(&second_chat, default_now() + TimeDuration::seconds(1)),
    )
    .await;
    raw_exec(
        &rig.quota.db.raw,
        &format!(
            "CREATE TRIGGER fail_first_turn BEFORE UPDATE ON chat_turns \
             WHEN NEW.id = {} BEGIN SELECT RAISE(ABORT, 'forced turn failure'); END",
            crate::test_support::chats::blob(first)
        ),
    )
    .await
    .unwrap();
    clock.set(later(5000));
    let report = rig.watchdog(TIMEOUT).scan_once().await.unwrap();
    assert_eq!(
        (report.candidates, report.finalized, report.failed),
        (2, 1, 1),
        "{report:?}"
    );
    assert_eq!(turn_row(&rig.quota.db, first).await.state, "running");
    assert_eq!(turn_row(&rig.quota.db, second).await.state, "failed");
}

// ── metrics ──────────────────────────────────────────────────────────────────

#[tokio::test]
async fn the_orphan_series_are_recorded_with_low_cardinality_labels() {
    let (rig, clock) = rig_at_start().await;
    for _ in 0..2 {
        let chat = rig.quota.chat_a1(STANDARD).await;
        rig.quota.book(&chat, STANDARD, facts()).await;
    }
    clock.set(later(301));
    rig.watchdog(TIMEOUT).scan_once().await.unwrap();
    let metrics = rig.metrics.as_ref().unwrap();
    assert_eq!(
        metrics.total_where("orphan_detected", "reason", "stale_progress"),
        2
    );
    assert_eq!(
        metrics.total_where("orphan_finalized", "reason", "stale_progress"),
        2
    );
    assert_eq!(
        metrics.total_where("streams_aborted", "trigger", "orphan_timeout"),
        2
    );
    assert_eq!(metrics.total("orphan_scan_duration_seconds"), 1);
    for (name, attributes) in ["orphan_detected", "orphan_finalized", "streams_aborted"]
        .iter()
        .flat_map(|name| {
            metrics
                .series(name)
                .into_iter()
                .map(move |(a, _)| (*name, a))
        })
    {
        for key in attributes.keys() {
            assert!(
                ["reason", "trigger"].contains(&key.as_str()),
                "label `{key}` on {name}"
            );
        }
    }
}

#[tokio::test]
async fn a_scan_that_finds_nothing_records_only_its_duration() {
    let (rig, _clock) = rig_at_start().await;
    rig.watchdog(TIMEOUT).scan_once().await.unwrap();
    let metrics = rig.metrics.as_ref().unwrap();
    assert_eq!(metrics.total("orphan_detected"), 0);
    assert_eq!(metrics.total("orphan_finalized"), 0);
    assert_eq!(metrics.total("orphan_scan_duration_seconds"), 1);
}

// ── the worker loop ──────────────────────────────────────────────────────────

#[tokio::test]
async fn the_worker_scans_until_cancelled_and_stops_promptly() {
    let (rig, clock) = rig_at_start().await;
    let chat = rig.quota.chat_a1(STANDARD).await;
    let booked = rig.quota.book(&chat, STANDARD, facts()).await;
    clock.set(later(301));
    let watchdog = Arc::new(rig.watchdog(TIMEOUT));
    let token = tokio_util::sync::CancellationToken::new();
    let worker = tokio::spawn(Arc::clone(&watchdog).run(token.clone()));
    let turn = rig.wait_terminal(chat.id).await;
    assert_eq!(turn.id, booked.turn_id);
    assert_eq!(turn.error_code.as_deref(), Some("orphan_timeout"));
    token.cancel();
    tokio::time::timeout(Duration::from_secs(2), worker)
        .await
        .expect("the worker stops promptly")
        .unwrap();
}

#[tokio::test]
async fn a_worker_cancelled_before_its_first_scan_scans_nothing() {
    let (rig, clock) = rig_at_start().await;
    let chat = rig.quota.chat_a1(STANDARD).await;
    let booked = rig.quota.book(&chat, STANDARD, facts()).await;
    clock.set(later(301));
    let token = tokio_util::sync::CancellationToken::new();
    token.cancel();
    Arc::new(rig.watchdog(TIMEOUT)).run(token).await;
    assert_eq!(
        turn_row(&rig.quota.db, booked.turn_id).await.state,
        "running"
    );
}
